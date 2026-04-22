//! macOS CBPeripheralManager-based BLE peripheral.
//!
//! Owns a `CBPeripheralManager`, registers the mlink service + TX/RX/CTRL
//! characteristics, and starts advertising with a room-hash encoded into the
//! manufacturer data slot of the advertisement payload.
//!
//! Obj-C delegate callbacks are forwarded into async land via `tokio::sync::mpsc`
//! channels held on the delegate.

#![cfg(target_os = "macos")]

use std::collections::HashMap;
use std::ffi::{c_char, c_void, CString};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

// Minimal FFI into libdispatch. `dispatch_queue_create` returns a new serial
// queue (when `attr` is null). We use it to create a dedicated queue for
// CoreBluetooth delegate callbacks so they do not rely on the main thread's
// dispatch/run loop — which does not run under tokio.
//
// The returned object is a dispatch_queue_t (an Obj-C object); we type it as
// `*mut c_void` here since we only hand it back to Obj-C via `msg_send_id!`.
// Not releasing the queue is deliberate: it lives as long as the peripheral
// manager that uses it (i.e. the whole process in practice).
extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> *mut c_void;
}

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

use super::ble::{
    encode_identity_uuid, CTRL_CHAR_UUID, MLINK_SERVICE_UUID, RX_CHAR_UUID, TX_CHAR_UUID,
};
use super::transport_trait::Connection;

/// Events surfaced by the Obj-C delegate into async land.
///
/// `StateChanged`, `ServiceAdded`, `AdvertisingStarted` and `CentralSubscribed`
/// travel on the shared control channel (one receiver, drained by bring-up and
/// by `wait_for_central`). `Received` is kept as a public variant for backward
/// compatibility with external matchers, but inbound frames are routed on
/// per-central channels so a read loop for one central never consumes another
/// central's (or a handshake's) data.
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
    /// Not emitted on the shared control channel — kept as a public variant for
    /// compatibility with existing consumers and tests; inbound bytes are
    /// delivered on per-central channels returned from `wait_for_central`.
    Received(Vec<u8>),
}

/// Event queue shared with the delegate; internal to this module.
type EventSender = mpsc::UnboundedSender<PeripheralEvent>;
type EventReceiver = mpsc::UnboundedReceiver<PeripheralEvent>;

/// Per-central inbound-bytes channel type.
type BytesSender = mpsc::UnboundedSender<Vec<u8>>;
type BytesReceiver = mpsc::UnboundedReceiver<Vec<u8>>;

/// Map from central identifier to its inbound-bytes sender. Mutated from both
/// Obj-C delegate callbacks (non-async) and from `wait_for_central` on the
/// async side, so we use a std Mutex.
type PerCentralTx = Arc<StdMutex<HashMap<String, BytesSender>>>;

/// Centrals currently subscribed to the RX characteristic. Wrapped in a `std`
/// Mutex (not tokio) because we mutate it from Obj-C delegate callbacks which
/// are non-async.
///
/// `Retained<CBCentral>` is not `Send`, but we only access this vec under the
/// mutex and we never hand out raw refs to other threads, so the same Send
/// reasoning as for `MacPeripheral` applies. A newtype lets us declare `Send`
/// explicitly and keep the async state machine Send-clean when an Arc of this
/// crosses `.await` points during setup.
struct SubscribedCentralsInner(StdMutex<Vec<Retained<CBCentral>>>);

// SAFETY: see the doc comment above — access is mutex-guarded and confined.
unsafe impl Send for SubscribedCentralsInner {}
unsafe impl Sync for SubscribedCentralsInner {}

impl SubscribedCentralsInner {
    fn new() -> Self {
        Self(StdMutex::new(Vec::new()))
    }

    fn lock(
        &self,
    ) -> std::sync::LockResult<std::sync::MutexGuard<'_, Vec<Retained<CBCentral>>>> {
        self.0.lock()
    }
}

type SubscribedCentrals = SubscribedCentralsInner;

/// The macOS peripheral. Holds strong references to the CoreBluetooth objects
/// so they stay alive for the lifetime of the advertisement.
pub struct MacPeripheral {
    manager: Retained<CBPeripheralManager>,
    _tx_char: Retained<CBMutableCharacteristic>,
    rx_char: Retained<CBMutableCharacteristic>,
    _ctrl_char: Retained<CBMutableCharacteristic>,
    _service: Retained<CBMutableService>,
    _delegate: Retained<MlinkPeripheralDelegate>,
    /// Control-plane events: StateChanged / ServiceAdded / AdvertisingStarted /
    /// CentralSubscribed. Inbound write bytes do NOT flow through here — they
    /// are routed on per-central channels (see `per_central_tx`) so a single
    /// shared receiver can't swallow another central's frames.
    ctrl_events: Arc<Mutex<EventReceiver>>,
    /// Held solely to keep the control-plane sender alive for the lifetime of
    /// the peripheral. The delegate holds its own clone, but retaining one
    /// here leaves the channel open for future direct-emission paths (e.g.
    /// teardown events) without needing a long-lived keep-alive task.
    _ctrl_tx: EventSender,
    /// Per-central inbound-bytes senders; populated on CentralSubscribed and
    /// torn down on unsubscribe. Shared with the delegate.
    per_central_tx: PerCentralTx,
    subscribed: Arc<SubscribedCentrals>,
    local_name: String,
    room_hash: Option<[u8; 8]>,
    app_uuid: Option<String>,
}

impl MacPeripheral {
    /// Create a peripheral manager, register the mlink service + characteristics,
    /// and start advertising. Returns once `startAdvertising` has been invoked;
    /// callers should poll `next_event` to observe state transitions.
    pub async fn start(
        local_name: impl Into<String>,
        room_hash: Option<[u8; 8]>,
        app_uuid: Option<String>,
    ) -> Result<Self> {
        let local_name = local_name.into();

        let (tx, mut rx) = mpsc::unbounded_channel::<PeripheralEvent>();
        let subscribed: Arc<SubscribedCentrals> = Arc::new(SubscribedCentralsInner::new());
        let per_central_tx: PerCentralTx = Arc::new(StdMutex::new(HashMap::new()));

        let advertised_name = match room_hash {
            Some(h) => format!("{local_name}#{}", hex_encode(&h)),
            None => local_name.clone(),
        };

        // All CoreBluetooth Retained values are !Send. The Transport trait
        // requires the returned future to be Send, so we pack every Retained
        // object used during setup into a holder we've declared Send (mirroring
        // the existing `unsafe impl Send for MacPeripheral`). This keeps the
        // async state machine Send-clean even though we cross .await points
        // while holding onto these objects.
        struct SetupHolder {
            manager: Retained<CBPeripheralManager>,
            service: Retained<CBMutableService>,
            tx_char: Retained<CBMutableCharacteristic>,
            rx_char: Retained<CBMutableCharacteristic>,
            ctrl_char: Retained<CBMutableCharacteristic>,
            delegate: Retained<MlinkPeripheralDelegate>,
            ad_dict: Retained<NSDictionary<NSString, AnyObject>>,
        }
        // SAFETY: same reasoning as `unsafe impl Send for MacPeripheral` —
        // every Retained access goes through an explicit `unsafe` block and we
        // do not share references across threads concurrently during setup.
        unsafe impl Send for SetupHolder {}

        let holder: SetupHolder = {
            let service = build_service()?;
            let tx_char =
                build_characteristic(TX_CHAR_UUID, WRITE_PERMISSIONS, write_properties())?;
            let rx_char =
                build_characteristic(RX_CHAR_UUID, READ_PERMISSIONS, notify_properties())?;
            let ctrl_char =
                build_characteristic(CTRL_CHAR_UUID, RW_PERMISSIONS, read_write_properties())?;

            attach_characteristics(&service, &[&tx_char, &rx_char, &ctrl_char]);

            let delegate = MlinkPeripheralDelegate::new(DelegateIvars {
                ctrl_tx: tx.clone(),
                per_central_tx: per_central_tx.clone(),
                subscribed: subscribed.clone(),
                rx_char_uuid: RX_CHAR_UUID,
                tx_char_uuid: TX_CHAR_UUID,
            });

            let delegate_proto: &ProtocolObject<dyn CBPeripheralManagerDelegate> =
                ProtocolObject::from_ref(&*delegate);
            // Create a dedicated serial dispatch queue for delegate callbacks.
            // Passing null here would schedule callbacks on the main queue,
            // which never drains under tokio (the main thread is owned by the
            // runtime and no dispatch/run loop is pumping it) — so didUpdateState
            // et al. would never fire and `wait_for_powered_on` would hang.
            let label = CString::new("com.mlink.peripheral")
                .expect("static dispatch queue label contains no NUL");
            let queue_ptr: *mut c_void =
                unsafe { dispatch_queue_create(label.as_ptr(), std::ptr::null()) };
            if queue_ptr.is_null() {
                return Err(MlinkError::HandlerError(
                    "dispatch_queue_create returned null".into(),
                ));
            }
            let queue_obj: *mut AnyObject = queue_ptr as *mut AnyObject;

            eprintln!("[mlink:periph] creating CBPeripheralManager");
            let manager: Retained<CBPeripheralManager> = unsafe {
                let alloc = CBPeripheralManager::alloc();
                msg_send_id![alloc, initWithDelegate: delegate_proto, queue: queue_obj]
            };

            let ad_dict = build_advertisement_data(&advertised_name, app_uuid.as_deref())?;

            SetupHolder {
                manager,
                service,
                tx_char,
                rx_char,
                ctrl_char,
                delegate,
                ad_dict,
            }
        };

        // CoreBluetooth rejects addService / startAdvertising until the manager
        // transitions to PoweredOn. Wait for that state change before touching
        // either. Without this, addService silently no-ops and the service is
        // never included in the broadcast, which is exactly the bug that sent
        // centrals into discover_services empty-handed.
        wait_for_powered_on(&mut rx).await?;

        // Register the service. This is async in CoreBluetooth; real completion
        // arrives via peripheralManager:didAddService:error:. startAdvertising
        // MUST be deferred until after that callback — otherwise the service
        // is not yet part of the GATT DB and the advertisement goes out without
        // it.
        eprintln!(
            "[mlink:periph] addService uuid={} chars={}",
            MLINK_SERVICE_UUID, 3
        );
        unsafe {
            holder.manager.addService(&holder.service);
        }

        wait_for_service_added(&mut rx).await?;

        eprintln!(
            "[mlink:periph] startAdvertising name={:?}",
            advertised_name
        );
        unsafe {
            holder.manager.startAdvertising(Some(&holder.ad_dict));
        }

        wait_for_advertising_started(&mut rx).await?;

        let SetupHolder {
            manager,
            service,
            tx_char,
            rx_char,
            ctrl_char,
            delegate,
            ad_dict: _,
        } = holder;

        Ok(Self {
            manager,
            _tx_char: tx_char,
            rx_char,
            _ctrl_char: ctrl_char,
            _service: service,
            _delegate: delegate,
            ctrl_events: Arc::new(Mutex::new(rx)),
            _ctrl_tx: tx,
            per_central_tx,
            subscribed,
            local_name,
            room_hash,
            app_uuid,
        })
    }

    pub fn local_name(&self) -> &str {
        &self.local_name
    }

    pub fn room_hash(&self) -> Option<&[u8; 8]> {
        self.room_hash.as_ref()
    }

    pub fn app_uuid(&self) -> Option<&str> {
        self.app_uuid.as_deref()
    }

    /// Await the next control-plane delegate event. Returns `None` if the
    /// sender was dropped. Inbound write bytes no longer travel on this
    /// channel — they are surfaced on per-central receivers returned from
    /// `wait_for_central`.
    pub async fn next_event(&self) -> Option<PeripheralEvent> {
        self.ctrl_events.lock().await.recv().await
    }

    /// Wait for a central to subscribe to RX. Returns the central id plus a
    /// receiver that yields ONLY bytes written by that specific central to the
    /// TX characteristic. Each call creates a fresh per-central channel; the
    /// caller (typically a `MacPeripheralConnection`) owns the receiver and
    /// reads from it without contending with other centrals' traffic.
    pub async fn wait_for_central(&self) -> Result<(String, BytesReceiver)> {
        loop {
            match self.next_event().await {
                Some(PeripheralEvent::CentralSubscribed { central_id }) => {
                    let (tx, rx) = mpsc::unbounded_channel::<Vec<u8>>();
                    // Replace (not merge) any prior entry under the same id —
                    // a reconnecting central should get its own fresh channel,
                    // and dropping the old sender will close the previous
                    // receiver's stream so the stale reader sees EOF.
                    if let Ok(mut map) = self.per_central_tx.lock() {
                        map.insert(central_id.clone(), tx);
                    }
                    return Ok((central_id, rx));
                }
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

/// Ivars held by the declared Obj-C delegate class. Keeps the control-plane
/// sender, the per-central inbound-bytes map, and the shared subscribed-centrals
/// list accessible from delegate callbacks.
struct DelegateIvars {
    /// Control-plane events (state + service + advertising + subscribe).
    ctrl_tx: EventSender,
    /// Per-central inbound-bytes senders, keyed by CBCentral identifier.
    /// Shared with `MacPeripheral`; lookups happen on the CoreBluetooth
    /// delegate queue in `didReceiveWriteRequests:`.
    per_central_tx: PerCentralTx,
    subscribed: Arc<SubscribedCentrals>,
    /// UUID of the notify characteristic centrals subscribe to (RX on the
    /// peripheral side, where the peripheral notifies centrals).
    rx_char_uuid: Uuid,
    /// UUID of the write characteristic centrals write into (TX on the
    /// peripheral side, where the peripheral receives frames). Used to filter
    /// `didReceiveWriteRequests:` so CTRL writes don't masquerade as payload.
    tx_char_uuid: Uuid,
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
            eprintln!("[mlink:periph] delegate: didUpdateState -> {:?}", state);
            let _ = self.ivars().ctrl_tx.send(PeripheralEvent::StateChanged(state));
        }

        #[method(peripheralManagerDidStartAdvertising:error:)]
        fn peripheral_manager_did_start_advertising_error(
            &self,
            _peripheral: &CBPeripheralManager,
            error: Option<&NSError>,
        ) {
            let res = match error {
                None => {
                    eprintln!("[mlink:periph] delegate: didStartAdvertising OK");
                    Ok(())
                }
                Some(e) => {
                    let msg = format!("{}", e.localizedDescription());
                    eprintln!("[mlink:periph] delegate: didStartAdvertising ERR {}", msg);
                    Err(msg)
                }
            };
            let _ = self.ivars().ctrl_tx.send(PeripheralEvent::AdvertisingStarted(res));
        }

        #[method(peripheralManager:didAddService:error:)]
        fn peripheral_manager_did_add_service_error(
            &self,
            _peripheral: &CBPeripheralManager,
            service: &CBService,
            error: Option<&NSError>,
        ) {
            let res = match error {
                None => {
                    let uuid = unsafe { service.UUID().UUIDString().to_string() };
                    let chars_count = unsafe {
                        service
                            .characteristics()
                            .map(|arr| arr.len())
                            .unwrap_or(0)
                    };
                    eprintln!(
                        "[mlink:periph] delegate: didAddService OK uuid={} chars={}",
                        uuid, chars_count
                    );
                    Ok(())
                }
                Some(e) => {
                    let msg = format!("{}", e.localizedDescription());
                    eprintln!("[mlink:periph] delegate: didAddService ERR {}", msg);
                    Err(msg)
                }
            };
            let _ = self.ivars().ctrl_tx.send(PeripheralEvent::ServiceAdded(res));
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
                .ctrl_tx
                .send(PeripheralEvent::CentralSubscribed { central_id: id });
        }

        #[method(peripheralManager:central:didUnsubscribeFromCharacteristic:)]
        fn peripheral_manager_central_did_unsubscribe(
            &self,
            _peripheral: &CBPeripheralManager,
            central: &CBCentral,
            characteristic: &CBCharacteristic,
        ) {
            // Only tear down per-central state when the central drops the RX
            // subscription; unsubscribing from other characteristics (e.g.
            // CTRL notify) should leave the read channel open.
            let char_uuid = unsafe { characteristic.UUID().UUIDString().to_string() };
            if !uuid_matches(&char_uuid, &self.ivars().rx_char_uuid) {
                return;
            }

            let target_id = unsafe { central.identifier().UUIDString().to_string() };
            if let Ok(mut guard) = self.ivars().subscribed.lock() {
                guard.retain(|c| {
                    let id = unsafe { c.identifier().UUIDString().to_string() };
                    id != target_id
                });
            }
            // Drop the per-central inbound-bytes sender so the owning
            // `MacPeripheralConnection::read` observes EOF instead of hanging.
            if let Ok(mut map) = self.ivars().per_central_tx.lock() {
                map.remove(&target_id);
            }
        }

        #[method(peripheralManager:didReceiveWriteRequests:)]
        fn peripheral_manager_did_receive_writes(
            &self,
            peripheral: &CBPeripheralManager,
            requests: &NSArray<CBATTRequest>,
        ) {
            // Route each write to the originating central's inbound channel.
            // Per Apple docs we must respond to the FIRST request in the array
            // to acknowledge them all — regardless of filtering decisions for
            // the individual entries.
            let count = requests.len();
            for i in 0..count {
                let req = requests.get(i).expect("request index in range");
                let char_uuid = unsafe { req.characteristic().UUID().UUIDString().to_string() };
                let Some(data) = (unsafe { req.value() }) else { continue };
                let bytes = data.bytes().to_vec();
                // Delegate the filter/route decision to `classify_write` so
                // the logic tested in unit tests is the same logic running
                // here. Writes to CTRL / RX / anything non-TX fall through
                // as Ignore and never reach the protocol reader — the old
                // code treated every write as Received, which let CTRL
                // traffic corrupt the handshake stream.
                if classify_write(&char_uuid, &self.ivars().tx_char_uuid, bytes.len())
                    != WriteRouteDecision::Deliver
                {
                    continue;
                }
                let central_id = unsafe {
                    req.central().identifier().UUIDString().to_string()
                };
                let sender = self
                    .ivars()
                    .per_central_tx
                    .lock()
                    .ok()
                    .and_then(|map| map.get(&central_id).cloned());
                match sender {
                    Some(tx) => {
                        let _ = tx.send(bytes);
                    }
                    None => {
                        // Central wrote before we observed a subscribe, or
                        // after it unsubscribed. Nothing we can do with the
                        // bytes — drop them and log so operators can see it
                        // happen rather than silently losing data.
                        eprintln!(
                            "[mlink:periph] drop write from unknown central {}",
                            central_id
                        );
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

/// Outcome of evaluating a single `didReceiveWriteRequests:` entry.
#[derive(Debug, PartialEq, Eq)]
enum WriteRouteDecision {
    /// Deliver these bytes to the per-central channel for `central_id`.
    Deliver,
    /// Silently ignore (wrong characteristic, empty payload, etc.). The write
    /// still needs to be responded to at the batch level.
    Ignore,
}

/// Pure helper mirroring the delegate's per-write decision. Extracted so the
/// filter/route logic can be tested without a live CBPeripheralManager.
fn classify_write(
    request_char_uuid: &str,
    tx_char_uuid: &Uuid,
    payload_len: usize,
) -> WriteRouteDecision {
    if !uuid_matches(request_char_uuid, tx_char_uuid) {
        return WriteRouteDecision::Ignore;
    }
    if payload_len == 0 {
        return WriteRouteDecision::Ignore;
    }
    WriteRouteDecision::Deliver
}

// ---------- bring-up sequencing ----------

/// Timeout for each CoreBluetooth bring-up step. 10 s is long enough to wait
/// for user-visible permission prompts on first launch, but short enough that
/// a misconfigured manager fails fast instead of hanging the caller forever.
const BRING_UP_TIMEOUT: Duration = Duration::from_secs(10);

async fn wait_for_powered_on(rx: &mut EventReceiver) -> Result<()> {
    let deadline = tokio::time::Instant::now() + BRING_UP_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, rx.recv()).await {
            Err(_) => {
                return Err(MlinkError::HandlerError(
                    "CBPeripheralManager never reached PoweredOn within 10s".into(),
                ));
            }
            Ok(None) => {
                return Err(MlinkError::HandlerError(
                    "peripheral event channel closed before PoweredOn".into(),
                ));
            }
            Ok(Some(PeripheralEvent::StateChanged(state))) => {
                eprintln!("[mlink:periph] state changed -> {:?}", state);
                if state == CBManagerState::PoweredOn {
                    return Ok(());
                }
                if state == CBManagerState::Unauthorized
                    || state == CBManagerState::Unsupported
                    || state == CBManagerState::PoweredOff
                {
                    return Err(MlinkError::HandlerError(format!(
                        "CBPeripheralManager in non-usable state {:?}",
                        state
                    )));
                }
            }
            Ok(Some(other)) => {
                eprintln!(
                    "[mlink:periph] unexpected event before PoweredOn: {:?}",
                    other
                );
            }
        }
    }
}

async fn wait_for_service_added(rx: &mut EventReceiver) -> Result<()> {
    let deadline = tokio::time::Instant::now() + BRING_UP_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, rx.recv()).await {
            Err(_) => {
                return Err(MlinkError::HandlerError(
                    "didAddService callback never fired within 10s".into(),
                ));
            }
            Ok(None) => {
                return Err(MlinkError::HandlerError(
                    "peripheral event channel closed before didAddService".into(),
                ));
            }
            Ok(Some(PeripheralEvent::ServiceAdded(Ok(())))) => {
                eprintln!("[mlink:periph] didAddService OK");
                return Ok(());
            }
            Ok(Some(PeripheralEvent::ServiceAdded(Err(msg)))) => {
                eprintln!("[mlink:periph] didAddService FAILED: {}", msg);
                return Err(MlinkError::HandlerError(format!(
                    "addService failed: {msg}"
                )));
            }
            Ok(Some(other)) => {
                eprintln!(
                    "[mlink:periph] unexpected event while awaiting didAddService: {:?}",
                    other
                );
            }
        }
    }
}

async fn wait_for_advertising_started(rx: &mut EventReceiver) -> Result<()> {
    let deadline = tokio::time::Instant::now() + BRING_UP_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, rx.recv()).await {
            Err(_) => {
                return Err(MlinkError::HandlerError(
                    "didStartAdvertising callback never fired within 10s".into(),
                ));
            }
            Ok(None) => {
                return Err(MlinkError::HandlerError(
                    "peripheral event channel closed before didStartAdvertising".into(),
                ));
            }
            Ok(Some(PeripheralEvent::AdvertisingStarted(Ok(())))) => {
                eprintln!("[mlink:periph] didStartAdvertising OK");
                return Ok(());
            }
            Ok(Some(PeripheralEvent::AdvertisingStarted(Err(msg)))) => {
                eprintln!("[mlink:periph] didStartAdvertising FAILED: {}", msg);
                return Err(MlinkError::HandlerError(format!(
                    "startAdvertising failed: {msg}"
                )));
            }
            Ok(Some(other)) => {
                eprintln!(
                    "[mlink:periph] unexpected event while awaiting didStartAdvertising: {:?}",
                    other
                );
            }
        }
    }
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

fn build_advertisement_data(
    name: &str,
    app_uuid: Option<&str>,
) -> Result<Retained<NSDictionary<NSString, AnyObject>>> {
    let ns_name = NSString::from_str(name);
    let ns_service_uuid = cb_uuid_from(MLINK_SERVICE_UUID)?;
    // Second service UUID carries the first 8 bytes of our app_uuid so
    // scanners can pick a central/peripheral role before dialling. If the
    // caller didn't supply an app_uuid, fall back to a single-UUID ad so
    // listen() still works on older paths.
    let identity_uuid: Option<Retained<CBUUID>> = match app_uuid
        .and_then(encode_identity_uuid)
    {
        Some(u) => Some(cb_uuid_from(u)?),
        None => None,
    };
    let uuids: Retained<NSArray<CBUUID>> = match identity_uuid.as_deref() {
        Some(id) => NSArray::from_slice(&[&*ns_service_uuid, id]),
        None => NSArray::from_slice(&[&*ns_service_uuid]),
    };

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

/// `Connection` returned from `listen()` on macOS. Reads pull from this
/// connection's private per-central receiver (created by `wait_for_central`),
/// so a single peripheral serving multiple centrals hands each one an
/// independent read stream and no two connections can steal each other's
/// frames. Writes push bytes to subscribed centrals via
/// `updateValue:forCharacteristic:onSubscribedCentrals:`.
pub struct MacPeripheralConnection {
    peer_id: String,
    peripheral: Arc<MacPeripheral>,
    rx: Mutex<BytesReceiver>,
}

impl MacPeripheralConnection {
    pub fn new(
        peer_id: impl Into<String>,
        peripheral: Arc<MacPeripheral>,
        rx: BytesReceiver,
    ) -> Self {
        Self {
            peer_id: peer_id.into(),
            peripheral,
            rx: Mutex::new(rx),
        }
    }

    pub fn peripheral(&self) -> &Arc<MacPeripheral> {
        &self.peripheral
    }
}

#[async_trait::async_trait]
impl Connection for MacPeripheralConnection {
    async fn read(&mut self) -> Result<Vec<u8>> {
        match self.rx.lock().await.recv().await {
            Some(bytes) => Ok(bytes),
            None => Err(MlinkError::PeerGone {
                peer_id: self.peer_id.clone(),
            }),
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
        // Advertising lifetime is owned by MacPeripheral, not by any single
        // inbound connection. Stopping it here would kill discoverability for
        // every peer after the first mismatched/aborted handshake, so we only
        // tear down per-connection state (no-op today; subscribe tracking is
        // cleared by CoreBluetooth when the central disconnects).
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
    fn build_advertisement_data_builds_for_all_three_app_uuid_variants() {
        // Smoke-test every combination the CLI can hit:
        //  1. a well-formed app_uuid -> 2-UUID advertisement,
        //  2. no app_uuid -> 1-UUID advertisement,
        //  3. a malformed app_uuid -> must NOT fail bring-up; silently drop
        //     the identity UUID so the peripheral still advertises the mlink
        //     service. We only assert Ok() here — the array-length check
        //     is intentionally avoided because `NSDictionary::objectForKey`
        //     return typing for arbitrary-value dicts is awkward to cast
        //     safely; full identity-UUID encoding round-trips live in
        //     ble.rs::tests and don't need CoreBluetooth mocked.
        let with_app = build_advertisement_data(
            "mlink-node",
            Some("aabbccdd-eeff-1122-3344-556677889900"),
        );
        assert!(with_app.is_ok(), "expected Ok with valid app_uuid");

        let without_app = build_advertisement_data("mlink-node", None);
        assert!(without_app.is_ok(), "expected Ok with no app_uuid");

        let malformed = build_advertisement_data("mlink-node", Some("not-a-uuid"));
        assert!(
            malformed.is_ok(),
            "malformed app_uuid must fall back to single-UUID ad, not error"
        );
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

    #[test]
    fn classify_write_accepts_tx_with_payload() {
        let tx_str = TX_CHAR_UUID.to_string();
        assert_eq!(
            classify_write(&tx_str, &TX_CHAR_UUID, 42),
            WriteRouteDecision::Deliver
        );
        // Uppercase UUID string still matches.
        assert_eq!(
            classify_write(&tx_str.to_uppercase(), &TX_CHAR_UUID, 1),
            WriteRouteDecision::Deliver
        );
    }

    #[test]
    fn classify_write_ignores_ctrl_char_writes() {
        // A write to the CTRL characteristic must NOT be forwarded as a
        // payload frame — this was the R5 bug where the delegate treated
        // every write as an inbound protocol byte regardless of the target.
        let ctrl_str = CTRL_CHAR_UUID.to_string();
        assert_eq!(
            classify_write(&ctrl_str, &TX_CHAR_UUID, 8),
            WriteRouteDecision::Ignore
        );
        // RX (notify-only on the peripheral side) should never receive
        // writes, but even if CoreBluetooth forwards one we ignore it.
        let rx_str = RX_CHAR_UUID.to_string();
        assert_eq!(
            classify_write(&rx_str, &TX_CHAR_UUID, 8),
            WriteRouteDecision::Ignore
        );
        // An unknown UUID is ignored too — fail closed, not open.
        assert_eq!(
            classify_write("00000000-0000-0000-0000-000000000000", &TX_CHAR_UUID, 8),
            WriteRouteDecision::Ignore
        );
    }

    #[test]
    fn classify_write_ignores_empty_payload_even_on_tx() {
        // Zero-length TX writes are legal at the BLE layer but carry no
        // protocol information; the old delegate also skipped them and we
        // preserve that behavior so readers don't see spurious empty frames.
        let tx_str = TX_CHAR_UUID.to_string();
        assert_eq!(
            classify_write(&tx_str, &TX_CHAR_UUID, 0),
            WriteRouteDecision::Ignore
        );
    }

    #[tokio::test]
    async fn per_central_routing_isolates_receivers() {
        // Simulate the map the delegate uses: sending to central A must only
        // surface on A's receiver, never on B's. This is the core R2 guarantee
        // (the pre-fix code shared one global receiver across centrals, so
        // handshake frames for one peer were swallowed by another's read loop).
        let map: PerCentralTx = Arc::new(StdMutex::new(HashMap::new()));
        let (tx_a, mut rx_a) = mpsc::unbounded_channel::<Vec<u8>>();
        let (tx_b, mut rx_b) = mpsc::unbounded_channel::<Vec<u8>>();
        map.lock().unwrap().insert("central-a".into(), tx_a);
        map.lock().unwrap().insert("central-b".into(), tx_b);

        // Send two frames to A and one to B.
        {
            let guard = map.lock().unwrap();
            guard.get("central-a").unwrap().send(b"hello-a1".to_vec()).unwrap();
            guard.get("central-a").unwrap().send(b"hello-a2".to_vec()).unwrap();
            guard.get("central-b").unwrap().send(b"hello-b1".to_vec()).unwrap();
        }

        assert_eq!(rx_a.recv().await.unwrap(), b"hello-a1");
        assert_eq!(rx_a.recv().await.unwrap(), b"hello-a2");
        assert_eq!(rx_b.recv().await.unwrap(), b"hello-b1");

        // Nothing further should arrive on either side after the writers
        // above have been exhausted — but both senders are still live in the
        // map, so the channels stay open. `try_recv` confirms emptiness
        // without blocking.
        assert!(rx_a.try_recv().is_err());
        assert!(rx_b.try_recv().is_err());
    }

    #[tokio::test]
    async fn per_central_unsubscribe_closes_receiver() {
        // Dropping the sender (what didUnsubscribeFromCharacteristic does by
        // removing the map entry) must close the receiver so the connection's
        // read loop observes EOF and surfaces PeerGone, rather than hanging.
        let map: PerCentralTx = Arc::new(StdMutex::new(HashMap::new()));
        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        map.lock().unwrap().insert("central-a".into(), tx);

        // Simulate unsubscribe: remove (and therefore drop) the sender.
        map.lock().unwrap().remove("central-a");

        assert!(rx.recv().await.is_none(), "receiver should observe EOF");
    }

    #[tokio::test]
    async fn wait_for_central_creates_fresh_channel_on_reconnect() {
        // Calling wait_for_central twice under the same id must replace the
        // previous sender so a reconnecting central doesn't share a channel
        // with a stale connection. We exercise the same map-insert semantics
        // wait_for_central uses, then verify the old receiver observes EOF.
        let map: PerCentralTx = Arc::new(StdMutex::new(HashMap::new()));
        let (tx_old, mut rx_old) = mpsc::unbounded_channel::<Vec<u8>>();
        map.lock().unwrap().insert("central-a".into(), tx_old);

        // Reconnect: fresh pair, insert under the same key (drops the old tx).
        let (tx_new, mut rx_new) = mpsc::unbounded_channel::<Vec<u8>>();
        map.lock().unwrap().insert("central-a".into(), tx_new);

        assert!(rx_old.recv().await.is_none(), "old receiver should EOF");

        map.lock().unwrap().get("central-a").unwrap().send(b"post-reconnect".to_vec()).unwrap();
        assert_eq!(rx_new.recv().await.unwrap(), b"post-reconnect");
    }
}
