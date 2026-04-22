use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use btleplug::api::{
    Central, Characteristic, Manager as _, Peripheral as _, ScanFilter, ValueNotification, WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
use futures::stream::{Stream, StreamExt};
use tokio::sync::{Mutex, OnceCell};
use uuid::Uuid;

/// Process-wide shared BLE adapter. All BleTransport instances must share the
/// same CBCentralManager so peripherals discovered during scan remain reachable
/// when a separate instance (or caller) issues connect(). Without this, the
/// scan-side adapter's peripheral list is invisible to the connect-side
/// adapter and find_peripheral returns PeerGone.
static SHARED_ADAPTER: OnceCell<Adapter> = OnceCell::const_new();

async fn shared_adapter() -> Result<&'static Adapter> {
    SHARED_ADAPTER
        .get_or_try_init(|| async {
            let manager = Manager::new().await?;
            let adapters = manager.adapters().await?;
            adapters
                .into_iter()
                .next()
                .ok_or_else(|| MlinkError::HandlerError("no BLE adapter available".into()))
        })
        .await
}

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

/// 4-byte marker that identifies the secondary service UUID carrying the
/// advertiser's 8-byte app identity. Scanners look for this prefix in
/// `props.services`, both sides compare identities to decide central/peripheral
/// role before dialling (larger identity dials, smaller waits) — this avoids
/// the symmetric-dial race where two Macs both call `connect()` and collide at
/// the BLE physical layer.
pub const IDENTITY_UUID_MARKER: [u8; 4] = [0x00, 0x00, 0xFF, 0xBB];

const DEFAULT_MTU: usize = 512;
const DEFAULT_SCAN_DURATION: Duration = Duration::from_secs(3);

/// Encode the first 8 bytes of an app UUID string into a 128-bit service UUID.
/// Layout: `[0x00,0x00,0xFF,0xBB] + app_uuid_bytes[0..8] + [0x00,0x00,0x00,0x00]`
/// — a stable, round-trippable Uuid that carries identity in the middle 8
/// bytes and can be rebuilt on the scanner side.
pub fn encode_identity_uuid(app_uuid: &str) -> Option<Uuid> {
    let parsed = Uuid::parse_str(app_uuid).ok()?;
    let src = parsed.as_bytes();
    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&IDENTITY_UUID_MARKER);
    out[4..12].copy_from_slice(&src[0..8]);
    // out[12..16] stays zero.
    Some(Uuid::from_bytes(out))
}

/// Inverse of `encode_identity_uuid`. Returns the embedded 8-byte identity if
/// `uuid` carries the `0000FFBB` marker in its first four bytes, otherwise
/// `None`. Used by the scanner to lift a peer's identity out of
/// `props.services`.
pub fn decode_identity_uuid(uuid: &Uuid) -> Option<[u8; 8]> {
    let bytes = uuid.as_bytes();
    if bytes[0..4] != IDENTITY_UUID_MARKER {
        return None;
    }
    let mut out = [0u8; 8];
    out.copy_from_slice(&bytes[4..12]);
    Some(out)
}

/// Given our own app_uuid and a peer's advertised 8-byte identity, decide
/// whether we should act as the BLE central (dial) or peripheral (wait).
/// Returns `Some(true)` = we dial, `Some(false)` = we wait. Ties and
/// unparsable local UUIDs return `None` so callers can fall back to the old
/// symmetric-dial path (the only way we'd tie is if two nodes share an
/// app_uuid — recoverable by retry but not by role arbitration).
pub fn should_dial_as_central(local_app_uuid: &str, peer_identity: &[u8; 8]) -> Option<bool> {
    let parsed = Uuid::parse_str(local_app_uuid).ok()?;
    let local_first_8 = &parsed.as_bytes()[0..8];
    match local_first_8.cmp(&peer_identity[..]) {
        std::cmp::Ordering::Greater => Some(true),
        std::cmp::Ordering::Less => Some(false),
        std::cmp::Ordering::Equal => None,
    }
}

pub struct BleTransport {
    negotiated_mtu: usize,
    scan_duration: Duration,
    local_name: String,
    room_hash: Option<[u8; 8]>,
    /// Our own app_uuid, mirrored into the peripheral advertisement as a
    /// second service UUID so scanning peers can deterministically pick a
    /// central/peripheral role before dialling. `None` = fall back to the
    /// single-UUID advertisement.
    app_uuid: Option<String>,
}

impl BleTransport {
    pub fn new() -> Self {
        Self {
            negotiated_mtu: DEFAULT_MTU,
            scan_duration: DEFAULT_SCAN_DURATION,
            local_name: "mlink".into(),
            room_hash: None,
            app_uuid: None,
        }
    }

    pub fn with_scan_duration(mut self, duration: Duration) -> Self {
        self.scan_duration = duration;
        self
    }

    pub fn set_negotiated_mtu(&mut self, mtu: usize) {
        self.negotiated_mtu = mtu;
    }

    /// Set the local name advertised when acting as peripheral.
    pub fn set_local_name(&mut self, name: impl Into<String>) {
        self.local_name = name.into();
    }

    /// Set the room hash (8 bytes) woven into the advertisement payload.
    /// Used by the peer-side scanner to filter peripherals by room code.
    pub fn set_room_hash(&mut self, hash: [u8; 8]) {
        self.room_hash = Some(hash);
    }

    pub fn clear_room_hash(&mut self) {
        self.room_hash = None;
    }

    pub fn room_hash(&self) -> Option<&[u8; 8]> {
        self.room_hash.as_ref()
    }

    /// Set our app_uuid so peripheral advertisements carry an identity service
    /// UUID and scanners on other nodes can pick a role deterministically.
    pub fn set_app_uuid(&mut self, uuid: impl Into<String>) {
        self.app_uuid = Some(uuid.into());
    }

    pub fn app_uuid(&self) -> Option<&str> {
        self.app_uuid.as_deref()
    }

    /// Start peripheral advertising and return the `MacPeripheral` handle so
    /// the caller can loop on `wait_for_central()` to accept multiple centrals.
    /// `listen()` only returns a single `Connection` and is kept for the
    /// single-central path; callers that need multi-central acceptance should
    /// use this method instead.
    #[cfg(target_os = "macos")]
    pub async fn start_peripheral(
        &mut self,
    ) -> Result<std::sync::Arc<super::peripheral::MacPeripheral>> {
        use super::peripheral::MacPeripheral;
        let peripheral = MacPeripheral::start(
            self.local_name.clone(),
            self.room_hash,
            self.app_uuid.clone(),
        )
        .await?;
        Ok(std::sync::Arc::new(peripheral))
    }

    async fn ensure_adapter(&mut self) -> Result<&'static Adapter> {
        shared_adapter().await
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
        // Filter at the CoreBluetooth layer by service UUID. On macOS the
        // peripheral's advertised service UUID often ends up in the overflow
        // area, which means `props.services` will be empty even though the
        // device genuinely advertises MLINK_SERVICE_UUID. Passing the UUID
        // into ScanFilter lets CoreBluetooth match against the overflow area
        // directly, so we still surface those peripherals.
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
                None => {
                    eprintln!("[mlink:debug] peripheral {} has no properties", p.id());
                    continue;
                }
            };
            let raw_name = props.local_name.clone().unwrap_or_default();
            // Prefer the peer's 8-byte app identity encoded into the second
            // service UUID (0000FFBB-...) — it lets the CLI pick a
            // central/peripheral role without racing symmetric dials. Fall
            // back to the old local-name room-hash path, then to
            // manufacturer_data, so older advertisers still surface.
            let identity = props.services.iter().find_map(decode_identity_uuid);
            let (name, metadata) = if let Some(id) = identity {
                let base = parse_room_hash_from_name(&raw_name)
                    .map(|(b, _)| b)
                    .unwrap_or_else(|| raw_name.clone());
                (base, id.to_vec())
            } else {
                match parse_room_hash_from_name(&raw_name) {
                    Some((base, hash)) => (base, hash.to_vec()),
                    None => {
                        let manuf_bytes: Vec<u8> = props
                            .manufacturer_data
                            .values()
                            .flat_map(|v| v.iter().copied())
                            .collect();
                        (raw_name.clone(), manuf_bytes)
                    }
                }
            };
            eprintln!(
                "[mlink:debug] discovered peer id={} name={:?} rssi={:?} services={:?} metadata_len={}",
                p.id(),
                raw_name,
                props.rssi,
                props.services,
                metadata.len()
            );
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
        eprintln!("[mlink:conn] ble.connect start peer_id={peer_id} name={:?}", peer.name);
        let adapter = self.ensure_adapter().await?;
        let peripheral = find_peripheral(adapter, &peer_id).await?;

        let was_connected = peripheral.is_connected().await.unwrap_or(false);
        eprintln!("[mlink:conn] ble.connect is_connected(before)={was_connected} peer={peer_id}");
        if !was_connected {
            eprintln!("[mlink:conn] ble.connect -> peripheral.connect() peer={peer_id}");
            if let Err(e) = peripheral.connect().await {
                eprintln!("[mlink:conn] ble.connect peripheral.connect() FAILED peer={peer_id}: {e}");
                return Err(e.into());
            }
            eprintln!("[mlink:conn] ble.connect peripheral.connect() OK peer={peer_id}");
        }
        eprintln!("[mlink:conn] ble.connect -> discover_services() peer={peer_id}");
        if let Err(e) = peripheral.discover_services().await {
            eprintln!("[mlink:conn] ble.connect discover_services FAILED peer={peer_id}: {e}");
            return Err(e.into());
        }

        let chars = peripheral.characteristics();
        eprintln!(
            "[mlink:conn] ble.connect discovered {} characteristic(s) peer={peer_id}",
            chars.len()
        );
        for c in &chars {
            eprintln!("[mlink:conn]   char uuid={} service={}", c.uuid, c.service_uuid);
        }
        let tx_char = chars
            .iter()
            .find(|c| c.uuid == TX_CHAR_UUID)
            .cloned()
            .ok_or_else(|| {
                eprintln!("[mlink:conn] ble.connect TX char MISSING peer={peer_id}");
                MlinkError::HandlerError(format!("TX characteristic missing on {peer_id}"))
            })?;
        let rx_char = chars
            .iter()
            .find(|c| c.uuid == RX_CHAR_UUID)
            .cloned()
            .ok_or_else(|| {
                eprintln!("[mlink:conn] ble.connect RX char MISSING peer={peer_id}");
                MlinkError::HandlerError(format!("RX characteristic missing on {peer_id}"))
            })?;
        eprintln!("[mlink:conn] ble.connect TX+RX chars found peer={peer_id}");

        let notifications = peripheral.notifications().await?;
        if let Err(e) = peripheral.subscribe(&rx_char).await {
            eprintln!("[mlink:conn] ble.connect subscribe(RX) FAILED peer={peer_id}: {e}");
            return Err(e.into());
        }
        eprintln!("[mlink:conn] ble.connect subscribe(RX) OK peer={peer_id}");

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
        #[cfg(target_os = "macos")]
        {
            use super::peripheral::{MacPeripheral, MacPeripheralConnection};

            let peripheral = MacPeripheral::start(
                self.local_name.clone(),
                self.room_hash,
                self.app_uuid.clone(),
            )
            .await?;
            let peripheral = std::sync::Arc::new(peripheral);
            let (central_id, rx) = peripheral.wait_for_central().await?;
            let conn = MacPeripheralConnection::new(central_id, peripheral, rx);
            Ok(Box::new(conn))
        }

        #[cfg(not(target_os = "macos"))]
        {
            Err(MlinkError::HandlerError(
                "BleTransport::listen unsupported on this platform: btleplug has no peripheral-role \
                 API; only macOS has a native CBPeripheralManager bridge"
                    .into(),
            ))
        }
    }

    fn mtu(&self) -> usize {
        self.negotiated_mtu
    }
}

/// Parse `<name>#<16 lowercase-hex>` into `(base_name, [u8; 8])`. Returns
/// `None` if the suffix isn't present or isn't a valid 8-byte hex string.
///
/// NOTE: on macOS, CoreBluetooth does not surface the peripheral's advertised
/// `CBAdvertisementDataLocalNameKey` to centrals in a reliable way
/// (overflow-area truncation), so this parser is effectively a no-op there —
/// room_hash is encoded via the service-UUID slot and exchanged in the
/// handshake instead. Kept for other platforms (Linux/Android) where the
/// local name round-trips through the advertisement payload intact.
fn parse_room_hash_from_name(name: &str) -> Option<(String, [u8; 8])> {
    let hash_pos = name.rfind('#')?;
    let hex_str = &name[hash_pos + 1..];
    if hex_str.len() != 16 || !hex_str.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 8];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex_str[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some((name[..hash_pos].to_string(), out))
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

    #[cfg(not(target_os = "macos"))]
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

    #[test]
    fn room_hash_round_trips() {
        let mut t = BleTransport::new();
        assert!(t.room_hash().is_none());
        let hash = [0xAB; 8];
        t.set_room_hash(hash);
        assert_eq!(t.room_hash(), Some(&hash));
        t.clear_room_hash();
        assert!(t.room_hash().is_none());
    }

    #[test]
    fn set_local_name_updates_field() {
        let mut t = BleTransport::new();
        t.set_local_name("mlink-node-a");
        assert_eq!(t.local_name, "mlink-node-a");
    }

    #[test]
    fn parse_room_hash_roundtrips_valid_suffix() {
        let hash: [u8; 8] = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x00, 0x11];
        let name = "mlink-node#aabbccddeeff0011";
        let (base, got) = parse_room_hash_from_name(name).expect("should parse");
        assert_eq!(base, "mlink-node");
        assert_eq!(got, hash);
    }

    #[test]
    fn parse_room_hash_returns_none_without_suffix() {
        assert!(parse_room_hash_from_name("mlink-node").is_none());
    }

    #[test]
    fn parse_room_hash_returns_none_for_short_suffix() {
        assert!(parse_room_hash_from_name("mlink#abcd").is_none());
    }

    #[test]
    fn parse_room_hash_returns_none_for_non_hex_suffix() {
        assert!(parse_room_hash_from_name("mlink#zzzzzzzzzzzzzzzz").is_none());
    }

    #[test]
    fn parse_room_hash_uses_last_hash() {
        // A name that legitimately contains `#` still parses the trailing hex.
        let (base, _) = parse_room_hash_from_name("foo#bar#0011223344556677").unwrap();
        assert_eq!(base, "foo#bar");
    }

    #[test]
    fn identity_uuid_encode_decode_roundtrip() {
        let app_uuid = "aabbccdd-eeff-1122-3344-556677889900";
        let encoded = encode_identity_uuid(app_uuid).expect("valid uuid");
        // The first four bytes must match the shared marker so scanners can
        // distinguish the identity UUID from MLINK_SERVICE_UUID.
        assert_eq!(&encoded.as_bytes()[0..4], &IDENTITY_UUID_MARKER);
        let decoded = decode_identity_uuid(&encoded).expect("marker present");
        let expected: [u8; 8] = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x11, 0x22];
        assert_eq!(decoded, expected);
    }

    #[test]
    fn identity_uuid_decode_rejects_non_marker_uuid() {
        // MLINK_SERVICE_UUID (starts 0000FFAA) must not be mistaken for an
        // identity UUID — otherwise scanners would pick up the wrong 8 bytes
        // and pick the wrong role.
        assert!(decode_identity_uuid(&MLINK_SERVICE_UUID).is_none());
    }

    #[test]
    fn identity_uuid_encode_rejects_invalid_app_uuid() {
        assert!(encode_identity_uuid("not-a-uuid").is_none());
    }

    #[test]
    fn should_dial_as_central_orders_by_uuid_bytes() {
        // Local UUID first 8 bytes > peer identity -> we dial.
        let local = "ffffffff-ffff-ffff-ffff-ffffffffffff";
        let peer: [u8; 8] = [0x00; 8];
        assert_eq!(should_dial_as_central(local, &peer), Some(true));

        // Local first 8 bytes < peer -> we wait.
        let local_small = "00000000-0000-0000-ffff-ffffffffffff";
        let peer_big: [u8; 8] = [0xff; 8];
        assert_eq!(should_dial_as_central(local_small, &peer_big), Some(false));
    }

    #[test]
    fn should_dial_as_central_returns_none_on_tie() {
        // Same 8 bytes on both sides — can't arbitrate, fall back.
        let local = "aabbccdd-eeff-1122-3344-556677889900";
        let peer_id: [u8; 8] = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x11, 0x22];
        assert_eq!(should_dial_as_central(local, &peer_id), None);
    }

    #[test]
    fn should_dial_as_central_returns_none_for_invalid_local() {
        let peer: [u8; 8] = [0; 8];
        assert_eq!(should_dial_as_central("not-a-uuid", &peer), None);
    }

    #[test]
    fn encode_is_deterministic_same_input_same_uuid() {
        // Two calls with the same app_uuid must return the same service UUID,
        // otherwise our own advertisement would drift round-to-round and
        // confuse peers.
        let u = "01234567-89ab-cdef-fedc-ba9876543210";
        assert_eq!(encode_identity_uuid(u), encode_identity_uuid(u));
    }
}
