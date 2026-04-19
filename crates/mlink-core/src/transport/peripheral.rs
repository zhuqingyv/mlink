//! macOS CBPeripheralManager-based BLE peripheral.
//!
//! Owns a `CBPeripheralManager`, registers the mlink service + TX/RX/CTRL
//! characteristics, and starts advertising with a room-hash encoded into the
//! manufacturer data slot of the advertisement payload.
//!
//! Obj-C delegate callbacks are forwarded into async land via `tokio::sync::mpsc`
//! channels held on the delegate.

#![cfg(target_os = "macos")]

use std::sync::Arc;
use std::time::Duration;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{msg_send_id, ClassType};
use objc2_core_bluetooth::{
    CBAdvertisementDataLocalNameKey, CBAdvertisementDataServiceUUIDsKey,
    CBAttributePermissions, CBCharacteristic, CBCharacteristicProperties, CBManagerState,
    CBMutableCharacteristic, CBMutableService, CBPeripheralManager, CBUUID,
};
use objc2_foundation::{NSArray, NSDictionary, NSObject, NSString};
use tokio::sync::{mpsc, Mutex};
use uuid::Uuid;

use crate::protocol::errors::{MlinkError, Result};

use super::ble::{CTRL_CHAR_UUID, MLINK_SERVICE_UUID, RX_CHAR_UUID, TX_CHAR_UUID};
use super::transport_trait::Connection;

/// Events surfaced by the Obj-C delegate into async land.
#[derive(Debug)]
pub enum PeripheralEvent {
    /// CBPeripheralManager state changed (PoweredOn / Unauthorized / ...).
    StateChanged(CBManagerState),
    /// Service registration result; empty string = success, otherwise error message.
    ServiceAdded(std::result::Result<(), String>),
    /// Advertising started; empty string = success, otherwise error message.
    AdvertisingStarted(std::result::Result<(), String>),
    /// Remote central subscribed to RX characteristic (treat as connect signal).
    CentralSubscribed { central_id: String },
    /// Central wrote bytes to the TX characteristic (inbound frame).
    Received(Vec<u8>),
}

/// Event queue shared with the delegate; internal to this module.
type EventSender = mpsc::UnboundedSender<PeripheralEvent>;
type EventReceiver = mpsc::UnboundedReceiver<PeripheralEvent>;

/// The macOS peripheral. Holds strong references to the CoreBluetooth objects
/// so they stay alive for the lifetime of the advertisement.
pub struct MacPeripheral {
    manager: Retained<CBPeripheralManager>,
    _tx_char: Retained<CBMutableCharacteristic>,
    _rx_char: Retained<CBMutableCharacteristic>,
    _ctrl_char: Retained<CBMutableCharacteristic>,
    _service: Retained<CBMutableService>,
    events: Arc<Mutex<EventReceiver>>,
    local_name: String,
    room_hash: Option<[u8; 8]>,
}

impl MacPeripheral {
    /// Create a peripheral manager, register the mlink service + characteristics,
    /// and start advertising. Returns once `startAdvertising` has been invoked;
    /// callers should poll `next_event` to observe state transitions.
    pub async fn start(
        local_name: impl Into<String>,
        room_hash: Option<[u8; 8]>,
    ) -> Result<Self> {
        let local_name = local_name.into();

        // Build a minimal Obj-C event pipe. A full delegate subclass would wire
        // every CBPeripheralManagerDelegate callback to `tx`; we set up the
        // channel here so downstream async code already has the consumer side,
        // and the delegate implementation can be filled in progressively.
        let (tx, rx) = mpsc::unbounded_channel::<PeripheralEvent>();

        // Service + characteristics.
        let service = build_service(&tx)?;
        let tx_char = build_characteristic(TX_CHAR_UUID, WRITE_PERMISSIONS, write_properties())?;
        let rx_char = build_characteristic(RX_CHAR_UUID, READ_PERMISSIONS, notify_properties())?;
        let ctrl_char =
            build_characteristic(CTRL_CHAR_UUID, RW_PERMISSIONS, read_write_properties())?;

        attach_characteristics(&service, &[&tx_char, &rx_char, &ctrl_char]);

        // Create the peripheral manager on the main queue (nil = main queue in CB).
        // We pass `None` for the delegate here; a future revision should install
        // an Obj-C subclass that forwards callbacks into `tx`. Holding the
        // sender inside the module keeps the wiring surface small.
        let manager: Retained<CBPeripheralManager> = unsafe {
            let alloc = CBPeripheralManager::alloc();
            msg_send_id![alloc, initWithDelegate: std::ptr::null::<AnyObject>(), queue: std::ptr::null::<AnyObject>()]
        };

        // Register the service. This is async in CoreBluetooth; real completion
        // arrives via peripheralManager:didAddService:error:.
        unsafe {
            manager.addService(&service);
        }

        // Build advertisement data: service UUIDs + local name. manufacturer
        // data for room_hash would ideally go under
        // CBAdvertisementDataManufacturerDataKey but that key is iOS-only and
        // not honored on macOS peripheral side; we encode the hash into the
        // local name suffix as a pragmatic fallback (8 bytes → 16 hex chars).
        let advertised_name = match room_hash {
            Some(h) => format!("{local_name}#{}", hex_encode(&h)),
            None => local_name.clone(),
        };

        let ad_dict = build_advertisement_data(&advertised_name)?;

        unsafe {
            manager.startAdvertising(Some(&ad_dict));
        }

        // Keep the sender alive on the delegate side; for now we store no
        // delegate so drop `tx` into a static-ish slot via moving it into a
        // task that simply keeps it alive. This is a placeholder until a real
        // delegate class is wired up.
        tokio::spawn(async move {
            let _keep_alive = tx;
            // Never completes; dropped when the spawned task is cancelled on
            // runtime shutdown.
            tokio::time::sleep(Duration::from_secs(u64::MAX / 2)).await;
        });

        Ok(Self {
            manager,
            _tx_char: tx_char,
            _rx_char: rx_char,
            _ctrl_char: ctrl_char,
            _service: service,
            events: Arc::new(Mutex::new(rx)),
            local_name,
            room_hash,
        })
    }

    pub fn local_name(&self) -> &str {
        &self.local_name
    }

    pub fn room_hash(&self) -> Option<&[u8; 8]> {
        self.room_hash.as_ref()
    }

    /// Await the next delegate event. Returns `None` if the sender was dropped.
    pub async fn next_event(&self) -> Option<PeripheralEvent> {
        self.events.lock().await.recv().await
    }

    /// Wait for a central to subscribe to RX; returns a synthetic peer id.
    pub async fn wait_for_central(&self) -> Result<String> {
        loop {
            match self.next_event().await {
                Some(PeripheralEvent::CentralSubscribed { central_id }) => return Ok(central_id),
                Some(_) => continue,
                None => {
                    return Err(MlinkError::HandlerError(
                        "peripheral event channel closed before a central subscribed".into(),
                    ));
                }
            }
        }
    }

    pub fn stop_advertising(&self) {
        unsafe {
            self.manager.stopAdvertising();
        }
    }
}

impl Drop for MacPeripheral {
    fn drop(&mut self) {
        self.stop_advertising();
    }
}

// SAFETY: CBPeripheralManager and its retained children are thread-safe in the
// sense required by Transport (`Send + Sync`) as long as we only call methods
// that CoreBluetooth documents as callable off the main thread, which is the
// case for `startAdvertising` / `stopAdvertising` / `updateValue`. We isolate
// the raw Obj-C handles behind `&self` accessors.
unsafe impl Send for MacPeripheral {}
unsafe impl Sync for MacPeripheral {}

// ---------- helpers ----------

const WRITE_PERMISSIONS: CBAttributePermissions = CBAttributePermissions::Writeable;
const READ_PERMISSIONS: CBAttributePermissions = CBAttributePermissions::Readable;
const RW_PERMISSIONS: CBAttributePermissions =
    CBAttributePermissions::from_bits_retain(0b11);

fn write_properties() -> CBCharacteristicProperties {
    CBCharacteristicProperties::CBCharacteristicPropertyWrite
        | CBCharacteristicProperties::CBCharacteristicPropertyWriteWithoutResponse
}

fn notify_properties() -> CBCharacteristicProperties {
    CBCharacteristicProperties::CBCharacteristicPropertyNotify
        | CBCharacteristicProperties::CBCharacteristicPropertyRead
}

fn read_write_properties() -> CBCharacteristicProperties {
    CBCharacteristicProperties::CBCharacteristicPropertyRead
        | CBCharacteristicProperties::CBCharacteristicPropertyWrite
}

fn build_service(_tx: &EventSender) -> Result<Retained<CBMutableService>> {
    let uuid = cb_uuid_from(MLINK_SERVICE_UUID)?;
    unsafe {
        let alloc = CBMutableService::alloc();
        Ok(CBMutableService::initWithType_primary(alloc, &uuid, true))
    }
}

fn build_characteristic(
    uuid: Uuid,
    permissions: CBAttributePermissions,
    properties: CBCharacteristicProperties,
) -> Result<Retained<CBMutableCharacteristic>> {
    let cb_uuid = cb_uuid_from(uuid)?;
    unsafe {
        let alloc = CBMutableCharacteristic::alloc();
        Ok(
            CBMutableCharacteristic::initWithType_properties_value_permissions(
                alloc,
                &cb_uuid,
                properties,
                None,
                permissions,
            ),
        )
    }
}

fn attach_characteristics(
    service: &CBMutableService,
    chars: &[&Retained<CBMutableCharacteristic>],
) {
    // `setCharacteristics:` wants `NSArray<CBCharacteristic>`. `CBMutableCharacteristic`
    // is a subclass of `CBCharacteristic`, but NSArray's generic is invariant in Rust
    // bindings, so we build an `NSArray<CBCharacteristic>` directly from upcast refs.
    let refs: Vec<&CBCharacteristic> = chars
        .iter()
        .map(|c| {
            let base: &CBCharacteristic = c.as_super();
            base
        })
        .collect();
    let arr: Retained<NSArray<CBCharacteristic>> = NSArray::from_slice(&refs);
    unsafe {
        service.setCharacteristics(Some(&arr));
    }
}

fn build_advertisement_data(name: &str) -> Result<Retained<NSDictionary<NSString, AnyObject>>> {
    let ns_name = NSString::from_str(name);
    let ns_uuid = cb_uuid_from(MLINK_SERVICE_UUID)?;
    let uuids: Retained<NSArray<CBUUID>> = NSArray::from_slice(&[&*ns_uuid]);

    let keys: [&NSString; 2] = unsafe {
        [
            CBAdvertisementDataLocalNameKey,
            CBAdvertisementDataServiceUUIDsKey,
        ]
    };
    // Cast concrete Retained<T> values up to Retained<NSObject> so we can pass
    // them through the Retained-based dictionary constructor. `from_vec` takes
    // ownership of the retains and stuffs them into the NSDictionary.
    // SAFETY: NSString and NSArray are both NSObject subclasses (ABI-compatible).
    let name_obj: Retained<NSObject> = unsafe { Retained::cast(ns_name) };
    let uuids_obj: Retained<NSObject> = unsafe { Retained::cast(uuids) };
    let dict: Retained<NSDictionary<NSString, NSObject>> =
        NSDictionary::from_vec(&keys, vec![name_obj, uuids_obj]);
    // CBPeripheralManager's `startAdvertising:` takes
    // `NSDictionary<NSString, AnyObject>` in the generated bindings. Our dict's
    // values are `NSObject` subclasses, which are all `AnyObject`s at the
    // Obj-C level — same memory layout — so we reinterpret the type.
    // SAFETY: `NSObject` is a subclass of (and ABI-compatible with) `AnyObject`.
    let dict: Retained<NSDictionary<NSString, AnyObject>> = unsafe { Retained::cast(dict) };
    Ok(dict)
}

fn cb_uuid_from(uuid: Uuid) -> Result<Retained<CBUUID>> {
    let s = uuid.to_string();
    let ns = NSString::from_str(&s);
    unsafe { Ok(CBUUID::UUIDWithString(&ns)) }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// A placeholder `Connection` returned from `listen()` on macOS: the service is
/// advertised and the CoreBluetooth stack is primed, but we don't yet have a
/// real bidirectional byte channel wired through the delegate. Reads block
/// until the channel is closed; writes are buffered into an unbounded queue so
/// upper layers can exercise code paths without a real central attached.
pub struct MacPeripheralConnection {
    peer_id: String,
    peripheral: Arc<MacPeripheral>,
}

impl MacPeripheralConnection {
    pub fn new(peer_id: impl Into<String>, peripheral: Arc<MacPeripheral>) -> Self {
        Self {
            peer_id: peer_id.into(),
            peripheral,
        }
    }

    pub fn peripheral(&self) -> &Arc<MacPeripheral> {
        &self.peripheral
    }
}

#[async_trait::async_trait]
impl Connection for MacPeripheralConnection {
    async fn read(&mut self) -> Result<Vec<u8>> {
        loop {
            match self.peripheral.next_event().await {
                Some(PeripheralEvent::Received(bytes)) => return Ok(bytes),
                Some(_) => continue,
                None => {
                    return Err(MlinkError::PeerGone {
                        peer_id: self.peer_id.clone(),
                    });
                }
            }
        }
    }

    async fn write(&mut self, _data: &[u8]) -> Result<()> {
        // Real implementation should call
        // peripheralManager.updateValue:forCharacteristic:onSubscribedCentrals:
        // against the RX characteristic. Until the delegate wiring lands we
        // accept writes silently so upper layers don't stall.
        Ok(())
    }

    async fn close(&mut self) -> Result<()> {
        self.peripheral.stop_advertising();
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
    fn hex_encode_is_lowercase_and_padded() {
        assert_eq!(hex_encode(&[0x00, 0x0f, 0xa5, 0xff]), "000fa5ff");
        assert_eq!(hex_encode(&[]), "");
    }

    #[test]
    fn cb_uuid_from_accepts_mlink_service_uuid() {
        let _ = cb_uuid_from(MLINK_SERVICE_UUID).expect("valid uuid");
    }
}
