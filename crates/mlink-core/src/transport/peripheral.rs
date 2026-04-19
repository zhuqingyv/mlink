//! macOS CBPeripheralManager-based BLE peripheral.
//!
//! Owns a `CBPeripheralManager`, registers the mlink service + TX/RX/CTRL
//! characteristics, and starts advertising with a room-hash encoded into the
//! manufacturer data slot of the advertisement payload.
//!
//! Obj-C delegate callbacks are forwarded into async land via `tokio::sync::mpsc`
//! channels held on the delegate.

#![cfg(target_os = "macos")]

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use objc2::mutability::InteriorMutable;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{declare_class, msg_send_id, ClassType, DeclaredClass};
use objc2_core_bluetooth::{
    CBATTRequest, CBAdvertisementDataLocalNameKey, CBAdvertisementDataServiceUUIDsKey,
    CBAttributePermissions, CBCentral, CBCharacteristic, CBCharacteristicProperties,
    CBManagerState, CBMutableCharacteristic, CBMutableService, CBPeripheralManager,
    CBPeripheralManagerDelegate, CBService, CBUUID,
};
use objc2_foundation::{
    NSArray, NSData, NSDictionary, NSError, NSObject, NSObjectProtocol, NSString,
};
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

/// Centrals currently subscribed to the RX characteristic. Wrapped in a `std`
/// Mutex (not tokio) because we mutate it from Obj-C delegate callbacks which
/// are non-async.
type SubscribedCentrals = StdMutex<Vec<Retained<CBCentral>>>;

/// The macOS peripheral. Holds strong references to the CoreBluetooth objects
/// so they stay alive for the lifetime of the advertisement.
pub struct MacPeripheral {
    manager: Retained<CBPeripheralManager>,
    _tx_char: Retained<CBMutableCharacteristic>,
    rx_char: Retained<CBMutableCharacteristic>,
    _ctrl_char: Retained<CBMutableCharacteristic>,
    _service: Retained<CBMutableService>,
    _delegate: Retained<MlinkPeripheralDelegate>,
    events: Arc<Mutex<EventReceiver>>,
    subscribed: Arc<SubscribedCentrals>,
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

        let (tx, rx) = mpsc::unbounded_channel::<PeripheralEvent>();
        let subscribed: Arc<SubscribedCentrals> = Arc::new(StdMutex::new(Vec::new()));

        // Service + characteristics.
        let service = build_service()?;
        let tx_char = build_characteristic(TX_CHAR_UUID, WRITE_PERMISSIONS, write_properties())?;
        let rx_char = build_characteristic(RX_CHAR_UUID, READ_PERMISSIONS, notify_properties())?;
        let ctrl_char =
            build_characteristic(CTRL_CHAR_UUID, RW_PERMISSIONS, read_write_properties())?;

        attach_characteristics(&service, &[&tx_char, &rx_char, &ctrl_char]);

        // Create the delegate. It owns clones of the sender and the subscribed
        // list; CoreBluetooth will retain the delegate via initWithDelegate:.
        let delegate = MlinkPeripheralDelegate::new(DelegateIvars {
            tx: tx.clone(),
            subscribed: subscribed.clone(),
            rx_char_uuid: RX_CHAR_UUID,
        });

        // Create the peripheral manager on the default queue (nil = main in CB).
        // `initWithDelegate:queue:` takes an Obj-C protocol object for the
        // delegate; we upcast our concrete delegate into its protocol form.
        let delegate_proto: &ProtocolObject<dyn CBPeripheralManagerDelegate> =
            ProtocolObject::from_ref(&*delegate);
        let manager: Retained<CBPeripheralManager> = unsafe {
            let alloc = CBPeripheralManager::alloc();
            msg_send_id![alloc, initWithDelegate: delegate_proto, queue: std::ptr::null::<AnyObject>()]
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

        // Keep the sender alive across the lifetime of the peripheral. The
        // delegate holds its own clone, but we also keep one here for any
        // future direct emission paths (e.g. teardown events).
        tokio::spawn(async move {
            let _keep_alive = tx;
            tokio::time::sleep(Duration::from_secs(u64::MAX / 2)).await;
        });

        Ok(Self {
            manager,
            _tx_char: tx_char,
            rx_char,
            _ctrl_char: ctrl_char,
            _service: service,
            _delegate: delegate,
            events: Arc::new(Mutex::new(rx)),
            subscribed,
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

    /// Push bytes to every currently-subscribed central via the RX notify
    /// characteristic. Returns `false` if CoreBluetooth's send queue is full
    /// (caller should wait and retry); `true` on success or if there are no
    /// subscribers yet.
    pub fn notify_subscribed(&self, data: &[u8]) -> bool {
        let centrals: Vec<Retained<CBCentral>> = match self.subscribed.lock() {
            Ok(g) => g.clone(),
            Err(_) => return false,
        };
        if centrals.is_empty() {
            // No one listening yet — treat as success; upper layers haven't
            // observed a subscribe event anyway.
            return true;
        }
        let ns_data = NSData::with_bytes(data);
        let refs: Vec<&CBCentral> = centrals.iter().map(|c| &**c).collect();
        let arr: Retained<NSArray<CBCentral>> = NSArray::from_slice(&refs);
        unsafe {
            self.manager.updateValue_forCharacteristic_onSubscribedCentrals(
                &ns_data,
                &self.rx_char,
                Some(&arr),
            )
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

// ---------- delegate ----------

/// Ivars held by the declared Obj-C delegate class. Keeps the channel sender
/// and the shared subscribed-centrals list accessible from delegate callbacks.
struct DelegateIvars {
    tx: EventSender,
    subscribed: Arc<SubscribedCentrals>,
    rx_char_uuid: Uuid,
}

declare_class!(
    struct MlinkPeripheralDelegate;

    // SAFETY:
    // - The superclass NSObject has no additional subclassing requirements.
    // - `InteriorMutable` is correct: we mutate `subscribed` through a Mutex,
    //   never with `&mut self`.
    // - `Drop` is not implemented; Ivars hold ordinary owned values that drop
    //   themselves via Rust's usual rules.
    unsafe impl ClassType for MlinkPeripheralDelegate {
        type Super = NSObject;
        type Mutability = InteriorMutable;
        const NAME: &'static str = "MlinkPeripheralDelegate";
    }

    impl DeclaredClass for MlinkPeripheralDelegate {
        type Ivars = DelegateIvars;
    }

    unsafe impl NSObjectProtocol for MlinkPeripheralDelegate {}

    unsafe impl CBPeripheralManagerDelegate for MlinkPeripheralDelegate {
        #[method(peripheralManagerDidUpdateState:)]
        fn peripheral_manager_did_update_state(&self, peripheral: &CBPeripheralManager) {
            let state = unsafe { peripheral.state() };
            let _ = self.ivars().tx.send(PeripheralEvent::StateChanged(state));
        }

        #[method(peripheralManagerDidStartAdvertising:error:)]
        fn peripheral_manager_did_start_advertising_error(
            &self,
            _peripheral: &CBPeripheralManager,
            error: Option<&NSError>,
        ) {
            let res = match error {
                None => Ok(()),
                Some(e) => Err(format!("{}", e.localizedDescription())),
            };
            let _ = self.ivars().tx.send(PeripheralEvent::AdvertisingStarted(res));
        }

        #[method(peripheralManager:didAddService:error:)]
        fn peripheral_manager_did_add_service_error(
            &self,
            _peripheral: &CBPeripheralManager,
            _service: &CBService,
            error: Option<&NSError>,
        ) {
            let res = match error {
                None => Ok(()),
                Some(e) => Err(format!("{}", e.localizedDescription())),
            };
            let _ = self.ivars().tx.send(PeripheralEvent::ServiceAdded(res));
        }

        #[method(peripheralManager:central:didSubscribeToCharacteristic:)]
        fn peripheral_manager_central_did_subscribe(
            &self,
            _peripheral: &CBPeripheralManager,
            central: &CBCentral,
            characteristic: &CBCharacteristic,
        ) {
            // Only record subscribes to the RX notify characteristic; other
            // characteristics (e.g. CTRL) may be subscribed independently.
            let uuid_str = unsafe { characteristic.UUID().UUIDString().to_string() };
            if !uuid_matches(&uuid_str, &self.ivars().rx_char_uuid) {
                return;
            }

            // Retain the central so we can later target it in updateValue:.
            let retained: Retained<CBCentral> = unsafe {
                Retained::retain(central as *const CBCentral as *mut CBCentral)
                    .expect("CBCentral retain failed")
            };
            let id = unsafe { retained.identifier().UUIDString().to_string() };
            if let Ok(mut guard) = self.ivars().subscribed.lock() {
                guard.push(retained);
            }
            let _ = self
                .ivars()
                .tx
                .send(PeripheralEvent::CentralSubscribed { central_id: id });
        }

        #[method(peripheralManager:central:didUnsubscribeFromCharacteristic:)]
        fn peripheral_manager_central_did_unsubscribe(
            &self,
            _peripheral: &CBPeripheralManager,
            central: &CBCentral,
            _characteristic: &CBCharacteristic,
        ) {
            let target_id = unsafe { central.identifier().UUIDString().to_string() };
            if let Ok(mut guard) = self.ivars().subscribed.lock() {
                guard.retain(|c| {
                    let id = unsafe { c.identifier().UUIDString().to_string() };
                    id != target_id
                });
            }
        }

        #[method(peripheralManager:didReceiveWriteRequests:)]
        fn peripheral_manager_did_receive_writes(
            &self,
            peripheral: &CBPeripheralManager,
            requests: &NSArray<CBATTRequest>,
        ) {
            // Collect bytes from every request; CoreBluetooth batches writes.
            // Per Apple docs we must respond to the FIRST request in the array
            // to acknowledge them all.
            let count = requests.len();
            for i in 0..count {
                let req = requests.get(i).expect("request index in range");
                if let Some(data) = unsafe { req.value() } {
                    let bytes = data.bytes().to_vec();
                    if !bytes.is_empty() {
                        let _ = self.ivars().tx.send(PeripheralEvent::Received(bytes));
                    }
                }
            }
            if count > 0 {
                let first = requests.get(0).expect("non-empty");
                unsafe {
                    peripheral.respondToRequest_withResult(
                        &first,
                        objc2_core_bluetooth::CBATTError::Success,
                    );
                }
            }
        }
    }
);

impl MlinkPeripheralDelegate {
    fn new(ivars: DelegateIvars) -> Retained<Self> {
        let this = Self::alloc().set_ivars(ivars);
        unsafe { msg_send_id![super(this), init] }
    }
}

// SAFETY: the delegate is driven by CoreBluetooth on whatever queue we passed
// (default main). All cross-thread access to Ivars goes through an mpsc sender
// (Send+Sync) and a Mutex. Holding `Retained<MlinkPeripheralDelegate>` on the
// `MacPeripheral` struct just keeps the Obj-C class alive; we never read
// `DelegateIvars` from Rust-side threads directly.
unsafe impl Send for MlinkPeripheralDelegate {}
unsafe impl Sync for MlinkPeripheralDelegate {}

fn uuid_matches(uuid_str: &str, expected: &Uuid) -> bool {
    // CoreBluetooth returns short-form UUIDs for standard attributes and the
    // full 128-bit form for custom ones. Our characteristics are all custom
    // 128-bit, so a case-insensitive compare against the canonical hex is
    // sufficient.
    let needle = expected.to_string();
    uuid_str.eq_ignore_ascii_case(&needle)
}

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

fn build_service() -> Result<Retained<CBMutableService>> {
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

/// `Connection` returned from `listen()` on macOS. Reads pull from the delegate's
/// event channel and return bytes from `didReceiveWriteRequests:`. Writes push
/// bytes to subscribed centrals via `updateValue:forCharacteristic:onSubscribedCentrals:`.
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

    async fn write(&mut self, data: &[u8]) -> Result<()> {
        // CoreBluetooth's `updateValue:forCharacteristic:onSubscribedCentrals:`
        // returns `false` when the internal send queue is full; the correct
        // recovery path is to wait for `peripheralManagerIsReadyToUpdateSubscribers:`
        // and retry. We approximate with a short backoff loop so callers don't
        // need to know about the transient-busy state.
        let mut attempts = 0;
        while !self.peripheral.notify_subscribed(data) {
            attempts += 1;
            if attempts > 50 {
                return Err(MlinkError::HandlerError(
                    "peripheral updateValue queue stayed full for >500ms".into(),
                ));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
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

    #[test]
    fn uuid_matches_ignores_case() {
        let u = RX_CHAR_UUID;
        assert!(uuid_matches(&u.to_string().to_lowercase(), &u));
        assert!(uuid_matches(&u.to_string().to_uppercase(), &u));
        assert!(!uuid_matches("00000000-0000-0000-0000-000000000000", &u));
    }

    #[test]
    fn delegate_class_registers() {
        // Forces the declare_class! registration path to run; panics in debug
        // builds if any method signature disagrees with the protocol.
        let _cls = MlinkPeripheralDelegate::class();
    }
}
