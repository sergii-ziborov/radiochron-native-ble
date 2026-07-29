use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, AnyThread, DefinedClass};
use objc2_core_bluetooth::{
    CBAdvertisementDataIsConnectable, CBAdvertisementDataLocalNameKey,
    CBAdvertisementDataManufacturerDataKey, CBAdvertisementDataServiceDataKey,
    CBAdvertisementDataServiceUUIDsKey, CBAdvertisementDataTxPowerLevelKey, CBCentralManager,
    CBCentralManagerDelegate, CBManagerState, CBPeripheral, CBUUID,
};
use objc2_foundation::{
    NSArray, NSData, NSDictionary, NSNumber, NSObject, NSObjectProtocol, NSString,
};
use radiochron::ble::{AddressType, ManufacturerData, ServiceData};

use crate::model::{AdapterReport, AdvertisementSet, RawAdvertisement, ScanReport};
use crate::{Error, ScanObserver};

const POLL_INTERVAL: Duration = Duration::from_millis(50);

enum Event {
    State(CBManagerState),
    Advertisement(RawAdvertisement),
}

define_class!(
    // SAFETY:
    // - NSObject has no subclassing requirements.
    // - The delegate does not implement Drop.
    #[unsafe(super(NSObject))]
    #[name = "RadioChronNativeBleCentralDelegate"]
    #[ivars = Sender<Event>]
    struct RadioChronCentralDelegate;

    // SAFETY: NSObjectProtocol has no additional safety requirements.
    unsafe impl NSObjectProtocol for RadioChronCentralDelegate {}

    // SAFETY: Both method signatures match CBCentralManagerDelegate exactly.
    unsafe impl CBCentralManagerDelegate for RadioChronCentralDelegate {
        #[unsafe(method(centralManagerDidUpdateState:))]
        fn central_manager_did_update_state(&self, central: &CBCentralManager) {
            let state = unsafe { central.state() };
            let _ = self.ivars().send(Event::State(state));
        }

        #[unsafe(method(centralManager:didDiscoverPeripheral:advertisementData:RSSI:))]
        fn central_manager_did_discover(
            &self,
            _central: &CBCentralManager,
            peripheral: &CBPeripheral,
            advertisement: &NSDictionary<NSString, AnyObject>,
            rssi: &NSNumber,
        ) {
            let item = unsafe { convert_advertisement(peripheral, advertisement, rssi) };
            let _ = self.ivars().send(Event::Advertisement(item));
        }
    }
);

impl RadioChronCentralDelegate {
    fn new(sender: Sender<Event>) -> Retained<Self> {
        let object = Self::alloc().set_ivars(sender);
        unsafe { msg_send![super(object), init] }
    }
}

pub(crate) fn scan(duration: Duration, observer: &dyn ScanObserver) -> Result<ScanReport, Error> {
    let started = Instant::now();
    let (sender, receiver) = mpsc::channel();
    let delegate = RadioChronCentralDelegate::new(sender);
    let queue = DispatchQueue::new("com.radiochron.native-ble", None);
    let manager = unsafe {
        CBCentralManager::initWithDelegate_queue(
            CBCentralManager::alloc(),
            Some(ProtocolObject::from_ref(&*delegate)),
            Some(&queue),
        )
    };

    let state = wait_until_ready(&manager, &receiver, duration, observer)?;
    if state != CBManagerState::PoweredOn {
        return Ok(ScanReport::new(
            started.elapsed(),
            vec![AdapterReport {
                name: "CoreBluetooth".to_owned(),
                state: state_name(state).to_owned(),
                scan_started: false,
                errors: vec![format!("CoreBluetooth adapter is {}", state_name(state))],
            }],
            Vec::new(),
            0,
            vec![format!("CoreBluetooth adapter is {}", state_name(state))],
        ));
    }

    unsafe {
        manager.scanForPeripheralsWithServices_options(None, None);
    }

    let mut advertisements = AdvertisementSet::default();
    while started.elapsed() < duration && !observer.is_cancelled() {
        let remaining = duration.saturating_sub(started.elapsed());
        match receiver.recv_timeout(remaining.min(POLL_INTERVAL)) {
            Ok(Event::Advertisement(item)) => advertisements.merge(item),
            Ok(Event::State(next)) if next != CBManagerState::PoweredOn => {
                unsafe { manager.stopScan() };
                let message = format!("CoreBluetooth adapter became {}", state_name(next));
                return Err(Error::new(message));
            }
            Ok(Event::State(_)) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                unsafe { manager.stopScan() };
                return Err(Error::new("CoreBluetooth delegate stopped"));
            }
        }
        observer.progress(started.elapsed().min(duration), duration);
    }
    unsafe { manager.stopScan() };

    while let Ok(event) = receiver.try_recv() {
        if let Event::Advertisement(item) = event {
            advertisements.merge(item);
        }
    }
    let (advertisements, skipped_without_rssi) = advertisements.finish();
    Ok(ScanReport::new(
        started.elapsed(),
        vec![AdapterReport {
            name: "CoreBluetooth".to_owned(),
            state: if observer.is_cancelled() {
                "cancelled".to_owned()
            } else {
                "powered_on".to_owned()
            },
            scan_started: true,
            errors: Vec::new(),
        }],
        advertisements,
        skipped_without_rssi,
        Vec::new(),
    ))
}

fn wait_until_ready(
    manager: &CBCentralManager,
    receiver: &Receiver<Event>,
    duration: Duration,
    observer: &dyn ScanObserver,
) -> Result<CBManagerState, Error> {
    let started = Instant::now();
    loop {
        let state = unsafe { manager.state() };
        if state != CBManagerState::Unknown && state != CBManagerState::Resetting {
            return Ok(state);
        }
        if started.elapsed() >= duration || observer.is_cancelled() {
            return Ok(state);
        }
        match receiver.recv_timeout(POLL_INTERVAL) {
            Ok(Event::State(state)) => return Ok(state),
            Ok(Event::Advertisement(_)) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(Error::new("CoreBluetooth delegate stopped"));
            }
        }
    }
}

unsafe fn convert_advertisement(
    peripheral: &CBPeripheral,
    advertisement: &NSDictionary<NSString, AnyObject>,
    rssi: &NSNumber,
) -> RawAdvertisement {
    let identifier = unsafe {
        peripheral
            .identifier()
            .UUIDString()
            .to_string()
            .to_ascii_lowercase()
    };
    let local_name = advertisement
        .objectForKey(unsafe { CBAdvertisementDataLocalNameKey })
        .map(|value| unsafe { cast_object::<NSString>(&value) }.to_string());
    let manufacturer_data = advertisement
        .objectForKey(unsafe { CBAdvertisementDataManufacturerDataKey })
        .and_then(|value| {
            let data = unsafe { cast_object::<NSData>(&value) };
            let bytes = data.to_vec();
            (bytes.len() >= 2).then(|| ManufacturerData {
                company_id: u16::from_le_bytes([bytes[0], bytes[1]]),
                data: bytes[2..].to_vec(),
            })
        })
        .into_iter()
        .collect();
    let service_data = advertisement
        .objectForKey(unsafe { CBAdvertisementDataServiceDataKey })
        .map(|value| {
            let dictionary = unsafe { cast_object::<NSDictionary<CBUUID, NSData>>(&value) };
            dictionary
                .keys()
                .filter_map(|uuid| {
                    let data = dictionary.objectForKey(&uuid)?;
                    Some(ServiceData {
                        uuid: normalize_uuid(&uuid),
                        data: data.to_vec(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let service_uuids = advertisement
        .objectForKey(unsafe { CBAdvertisementDataServiceUUIDsKey })
        .map(|value| {
            let array = unsafe { cast_object::<NSArray<CBUUID>>(&value) };
            array
                .into_iter()
                .map(|uuid| normalize_uuid(&uuid))
                .collect()
        })
        .unwrap_or_default();
    let tx_power_dbm = advertisement
        .objectForKey(unsafe { CBAdvertisementDataTxPowerLevelKey })
        .map(|value| unsafe { cast_object::<NSNumber>(&value) }.as_i16());
    let connectable = advertisement
        .objectForKey(unsafe { CBAdvertisementDataIsConnectable })
        .map(|value| unsafe { cast_object::<NSNumber>(&value) }.as_bool());

    RawAdvertisement {
        address: identifier.clone(),
        address_type: AddressType::Unknown,
        local_name,
        rssi_dbm: Some(rssi.as_i16()),
        tx_power_dbm,
        connectable,
        service_uuids,
        manufacturer_data,
        service_data,
        protocol_identity: Some(format!("corebluetooth:{identifier}")),
    }
}

unsafe fn cast_object<T>(value: &AnyObject) -> &T {
    unsafe { &*(std::ptr::from_ref(value).cast::<T>()) }
}

fn normalize_uuid(uuid: &CBUUID) -> String {
    let value = unsafe { uuid.UUIDString() }
        .to_string()
        .to_ascii_lowercase();
    match value.len() {
        4 => format!("0000{value}-0000-1000-8000-00805f9b34fb"),
        8 => format!("{value}-0000-1000-8000-00805f9b34fb"),
        _ => value,
    }
}

fn state_name(state: CBManagerState) -> &'static str {
    if state == CBManagerState::PoweredOn {
        "powered_on"
    } else if state == CBManagerState::PoweredOff {
        "powered_off"
    } else if state == CBManagerState::Unauthorized {
        "unauthorized"
    } else if state == CBManagerState::Unsupported {
        "unsupported"
    } else if state == CBManagerState::Resetting {
        "resetting"
    } else {
        "unknown"
    }
}
