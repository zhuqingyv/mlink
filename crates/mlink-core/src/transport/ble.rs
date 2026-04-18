use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use btleplug::api::{
    Central, Characteristic, Manager as _, Peripheral as _, ScanFilter, ValueNotification, WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
use futures::stream::{Stream, StreamExt};
use tokio::sync::Mutex;
use uuid::Uuid;

type NotificationStream = Pin<Box<dyn Stream<Item = ValueNotification> + Send>>;

use crate::protocol::errors::{MlinkError, Result};

use super::transport_trait::{Connection, DiscoveredPeer, Transport, TransportCapabilities};

pub const MLINK_SERVICE_UUID: Uuid = match Uuid::try_parse("0000FFAA-0000-1000-8000-00805F9B34FB") {
    Ok(u) => u,
    Err(_) => panic!("invalid MLINK_SERVICE_UUID literal"),
};
pub const TX_CHAR_UUID: Uuid = match Uuid::try_parse("0000FFAB-0000-1000-8000-00805F9B34FB") {
    Ok(u) => u,
    Err(_) => panic!("invalid TX_CHAR_UUID literal"),
};
pub const RX_CHAR_UUID: Uuid = match Uuid::try_parse("0000FFAC-0000-1000-8000-00805F9B34FB") {
    Ok(u) => u,
    Err(_) => panic!("invalid RX_CHAR_UUID literal"),
};
pub const CTRL_CHAR_UUID: Uuid = match Uuid::try_parse("0000FFAD-0000-1000-8000-00805F9B34FB") {
    Ok(u) => u,
    Err(_) => panic!("invalid CTRL_CHAR_UUID literal"),
};

const DEFAULT_MTU: usize = 512;
const DEFAULT_SCAN_DURATION: Duration = Duration::from_secs(3);

pub struct BleTransport {
    adapter: Option<Adapter>,
    negotiated_mtu: usize,
    scan_duration: Duration,
}

impl BleTransport {
    pub fn new() -> Self {
        Self {
            adapter: None,
            negotiated_mtu: DEFAULT_MTU,
            scan_duration: DEFAULT_SCAN_DURATION,
        }
    }

    pub fn with_scan_duration(mut self, duration: Duration) -> Self {
        self.scan_duration = duration;
        self
    }

    pub fn set_negotiated_mtu(&mut self, mtu: usize) {
        self.negotiated_mtu = mtu;
    }

    async fn ensure_adapter(&mut self) -> Result<&Adapter> {
        if self.adapter.is_none() {
            let manager = Manager::new().await?;
            let adapters = manager.adapters().await?;
            let adapter = adapters.into_iter().next().ok_or_else(|| {
                MlinkError::HandlerError("no BLE adapter available".into())
            })?;
            self.adapter = Some(adapter);
        }
        Ok(self.adapter.as_ref().unwrap())
    }
}

impl Default for BleTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Transport for BleTransport {
    fn id(&self) -> &str {
        "ble"
    }

    fn capabilities(&self) -> TransportCapabilities {
        TransportCapabilities {
            max_peers: 7,
            throughput_bps: 1_200_000,
            latency_ms: 15,
            reliable: true,
            bidirectional: true,
        }
    }

    async fn discover(&mut self) -> Result<Vec<DiscoveredPeer>> {
        let scan_duration = self.scan_duration;
        let adapter = self.ensure_adapter().await?;
        let filter = ScanFilter {
            services: vec![MLINK_SERVICE_UUID],
        };
        adapter.start_scan(filter).await?;
        tokio::time::sleep(scan_duration).await;
        let peripherals = adapter.peripherals().await?;
        let _ = adapter.stop_scan().await;

        let mut out = Vec::with_capacity(peripherals.len());
        for p in peripherals {
            let props = match p.properties().await? {
                Some(p) => p,
                None => continue,
            };
            if !props.services.contains(&MLINK_SERVICE_UUID) {
                continue;
            }
            let name = props.local_name.unwrap_or_else(|| p.id().to_string());
            let metadata = props
                .manufacturer_data
                .values()
                .next()
                .cloned()
                .unwrap_or_default();
            out.push(DiscoveredPeer {
                id: p.id().to_string(),
                name,
                rssi: props.rssi,
                metadata,
            });
        }
        Ok(out)
    }

    async fn connect(&mut self, peer: &DiscoveredPeer) -> Result<Box<dyn Connection>> {
        let peer_id = peer.id.clone();
        let adapter = self.ensure_adapter().await?;
        let peripheral = find_peripheral(adapter, &peer_id).await?;

        if !peripheral.is_connected().await? {
            peripheral.connect().await?;
        }
        peripheral.discover_services().await?;

        let chars = peripheral.characteristics();
        let tx_char = chars
            .iter()
            .find(|c| c.uuid == TX_CHAR_UUID)
            .cloned()
            .ok_or_else(|| {
                MlinkError::HandlerError(format!("TX characteristic missing on {peer_id}"))
            })?;
        let rx_char = chars
            .iter()
            .find(|c| c.uuid == RX_CHAR_UUID)
            .cloned()
            .ok_or_else(|| {
                MlinkError::HandlerError(format!("RX characteristic missing on {peer_id}"))
            })?;

        peripheral.subscribe(&rx_char).await?;
        let notifications = peripheral.notifications().await?;

        Ok(Box::new(BleConnection {
            peer_id,
            peripheral: Some(peripheral),
            tx_char,
            rx_char,
            notifications: Arc::new(Mutex::new(Some(notifications))),
            mtu: self.negotiated_mtu,
        }))
    }

    async fn listen(&mut self) -> Result<Box<dyn Connection>> {
        Err(MlinkError::HandlerError(
            "BleTransport::listen unsupported: btleplug has no peripheral-role API on macOS; \
             advertise via a platform-specific peripheral implementation instead"
                .into(),
        ))
    }

    fn mtu(&self) -> usize {
        self.negotiated_mtu
    }
}

async fn find_peripheral(adapter: &Adapter, peer_id: &str) -> Result<Peripheral> {
    for p in adapter.peripherals().await? {
        if p.id().to_string() == peer_id {
            return Ok(p);
        }
    }
    Err(MlinkError::PeerGone {
        peer_id: peer_id.to_string(),
    })
}

pub struct BleConnection {
    peer_id: String,
    peripheral: Option<Peripheral>,
    tx_char: Characteristic,
    rx_char: Characteristic,
    notifications: Arc<Mutex<Option<NotificationStream>>>,
    mtu: usize,
}

impl BleConnection {
    pub fn mtu(&self) -> usize {
        self.mtu
    }
}

#[async_trait]
impl Connection for BleConnection {
    async fn read(&mut self) -> Result<Vec<u8>> {
        let mut guard = self.notifications.lock().await;
        let stream = guard.as_mut().ok_or_else(|| MlinkError::PeerGone {
            peer_id: self.peer_id.clone(),
        })?;
        loop {
            let n = stream.next().await.ok_or_else(|| MlinkError::PeerGone {
                peer_id: self.peer_id.clone(),
            })?;
            if n.uuid == self.rx_char.uuid {
                return Ok(n.value);
            }
        }
    }

    async fn write(&mut self, data: &[u8]) -> Result<()> {
        let peripheral = self.peripheral.as_ref().ok_or_else(|| MlinkError::PeerGone {
            peer_id: self.peer_id.clone(),
        })?;
        peripheral
            .write(&self.tx_char, data, WriteType::WithoutResponse)
            .await?;
        Ok(())
    }

    async fn close(&mut self) -> Result<()> {
        self.notifications.lock().await.take();
        if let Some(p) = self.peripheral.take() {
            let _ = p.unsubscribe(&self.rx_char).await;
            p.disconnect().await?;
        }
        Ok(())
    }

    fn peer_id(&self) -> &str {
        &self.peer_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_uuid_value() {
        assert_eq!(
            MLINK_SERVICE_UUID.to_string().to_uppercase(),
            "0000FFAA-0000-1000-8000-00805F9B34FB"
        );
    }

    #[test]
    fn characteristic_uuids_are_distinct() {
        let all = [MLINK_SERVICE_UUID, TX_CHAR_UUID, RX_CHAR_UUID, CTRL_CHAR_UUID];
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                assert_ne!(all[i], all[j], "uuids at {i} and {j} collide");
            }
        }
    }

    #[test]
    fn transport_metadata() {
        let t = BleTransport::new();
        assert_eq!(t.id(), "ble");
        assert_eq!(t.mtu(), DEFAULT_MTU);
        let caps = t.capabilities();
        assert_eq!(caps.max_peers, 7);
        assert_eq!(caps.throughput_bps, 1_200_000);
        assert_eq!(caps.latency_ms, 15);
        assert!(caps.reliable);
        assert!(caps.bidirectional);
    }

    #[test]
    fn set_negotiated_mtu_updates_reported_mtu() {
        let mut t = BleTransport::new();
        t.set_negotiated_mtu(247);
        assert_eq!(t.mtu(), 247);
    }

    #[tokio::test]
    async fn listen_returns_error_with_explanation() {
        let mut t = BleTransport::new();
        let res = t.listen().await;
        let err = match res {
            Ok(_) => panic!("expected listen() to return Err"),
            Err(e) => e,
        };
        match err {
            MlinkError::HandlerError(msg) => {
                assert!(msg.to_lowercase().contains("peripheral"));
            }
            other => panic!("expected HandlerError, got {other:?}"),
        }
    }
}
