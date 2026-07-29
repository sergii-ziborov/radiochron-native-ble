use std::collections::HashMap;
use std::thread;
use std::time::{Duration, Instant};

use dbus::arg::{prop_cast, ArgType, PropMap, RefArg, Variant};
use dbus::blocking::Connection;
use dbus::Path;
use radiochron::ble::{AddressType, ManufacturerData, ServiceData};

use crate::model::{AdapterReport, AdvertisementSet, RawAdvertisement, ScanReport};
use crate::{Error, ScanObserver};

const BLUEZ_DESTINATION: &str = "org.bluez";
const OBJECT_MANAGER: &str = "org.freedesktop.DBus.ObjectManager";
const ADAPTER_INTERFACE: &str = "org.bluez.Adapter1";
const DEVICE_INTERFACE: &str = "org.bluez.Device1";
const DBUS_TIMEOUT: Duration = Duration::from_secs(5);
const PROGRESS_INTERVAL: Duration = Duration::from_millis(50);

type Interfaces = HashMap<String, PropMap>;
type ManagedObjects = HashMap<Path<'static>, Interfaces>;

pub(crate) fn scan(duration: Duration, observer: &dyn ScanObserver) -> Result<ScanReport, Error> {
    let started = Instant::now();
    let connection = Connection::new_system()
        .map_err(|error| Error::new(format!("connect to system D-Bus: {error}")))?;
    let objects = managed_objects(&connection)?;
    let mut adapters = Vec::new();
    let mut active_adapters = Vec::new();

    for (path, interfaces) in &objects {
        let Some(properties) = interfaces.get(ADAPTER_INTERFACE) else {
            continue;
        };
        let name = prop_cast::<String>(properties, "Alias")
            .or_else(|| prop_cast::<String>(properties, "Name"))
            .cloned()
            .unwrap_or_else(|| path.to_string());
        let powered = prop_cast::<bool>(properties, "Powered")
            .copied()
            .unwrap_or(false);
        let mut report = AdapterReport {
            name,
            state: if powered {
                "powered_on".to_owned()
            } else {
                "powered_off".to_owned()
            },
            scan_started: false,
            errors: Vec::new(),
        };
        if powered {
            let proxy = connection.with_proxy(BLUEZ_DESTINATION, path.clone(), DBUS_TIMEOUT);
            let mut filter = PropMap::new();
            filter.insert("Transport".to_owned(), Variant(Box::new("le".to_owned())));
            filter.insert("DuplicateData".to_owned(), Variant(Box::new(true)));
            let filter_result: Result<(), dbus::Error> =
                proxy.method_call(ADAPTER_INTERFACE, "SetDiscoveryFilter", (filter,));
            if let Err(error) = filter_result {
                report
                    .errors
                    .push(format!("set BlueZ discovery filter: {error}"));
            }
            let start_result: Result<(), dbus::Error> =
                proxy.method_call(ADAPTER_INTERFACE, "StartDiscovery", ());
            match start_result {
                Ok(()) => {
                    report.scan_started = true;
                    active_adapters.push((path.clone(), adapters.len()));
                }
                Err(error) => report
                    .errors
                    .push(format!("start BlueZ discovery: {error}")),
            }
        }
        adapters.push(report);
    }

    while started.elapsed() < duration && !observer.is_cancelled() {
        let remaining = duration.saturating_sub(started.elapsed());
        thread::sleep(remaining.min(PROGRESS_INTERVAL));
        observer.progress(started.elapsed().min(duration), duration);
    }

    for (path, adapter_index) in &active_adapters {
        let proxy = connection.with_proxy(BLUEZ_DESTINATION, path.clone(), DBUS_TIMEOUT);
        let stop_result: Result<(), dbus::Error> =
            proxy.method_call(ADAPTER_INTERFACE, "StopDiscovery", ());
        if let Err(error) = stop_result {
            if let Some(report) = adapters.get_mut(*adapter_index) {
                report.errors.push(format!("stop BlueZ discovery: {error}"));
            }
        }
    }

    let discovered = managed_objects(&connection)?;
    let mut items = AdvertisementSet::default();
    for interfaces in discovered.values() {
        if let Some(properties) = interfaces.get(DEVICE_INTERFACE) {
            if let Some(item) = convert_device(properties) {
                items.merge(item);
            }
        }
    }
    let (advertisements, skipped_without_rssi) = items.finish();
    let errors = adapters
        .iter()
        .flat_map(|adapter| adapter.errors.iter().cloned())
        .collect();
    Ok(ScanReport::new(
        started.elapsed(),
        adapters,
        advertisements,
        skipped_without_rssi,
        errors,
    ))
}

fn managed_objects(connection: &Connection) -> Result<ManagedObjects, Error> {
    let proxy = connection.with_proxy(BLUEZ_DESTINATION, "/", DBUS_TIMEOUT);
    let (objects,): (ManagedObjects,) = proxy
        .method_call(OBJECT_MANAGER, "GetManagedObjects", ())
        .map_err(|error| {
            Error::new(format!(
                "query BlueZ managed objects (is bluetoothd running?): {error}"
            ))
        })?;
    Ok(objects)
}

fn convert_device(properties: &PropMap) -> Option<RawAdvertisement> {
    let address = prop_cast::<String>(properties, "Address")?.clone();
    let address_type = match prop_cast::<String>(properties, "AddressType").map(String::as_str) {
        Some("public") => AddressType::Public,
        Some("random") => AddressType::Unknown,
        _ => AddressType::Unknown,
    };
    let local_name = prop_cast::<String>(properties, "Name")
        .or_else(|| prop_cast::<String>(properties, "Alias"))
        .cloned();
    let rssi_dbm = prop_cast::<i16>(properties, "RSSI").copied();
    let tx_power_dbm = prop_cast::<i16>(properties, "TxPower").copied();
    let service_uuids = prop_cast::<Vec<String>>(properties, "UUIDs")
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|uuid| uuid.to_ascii_lowercase())
        .collect();
    let manufacturer_data = properties
        .get("ManufacturerData")
        .and_then(|value| value.0.as_iter())
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let mut fields = entry.as_iter()?;
            let key = fields.next()?;
            let value = fields.next()?;
            Some(ManufacturerData {
                company_id: u16::try_from(key.as_u64()?).ok()?,
                data: refarg_bytes(value)?,
            })
        })
        .collect();
    let service_data = properties
        .get("ServiceData")
        .and_then(|value| value.0.as_iter())
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let mut fields = entry.as_iter()?;
            let key = fields.next()?;
            let value = fields.next()?;
            Some(ServiceData {
                uuid: key.as_str()?.to_ascii_lowercase(),
                data: refarg_bytes(value)?,
            })
        })
        .collect();
    Some(RawAdvertisement {
        address,
        address_type,
        local_name,
        rssi_dbm,
        tx_power_dbm,
        connectable: None,
        service_uuids,
        manufacturer_data,
        service_data,
        protocol_identity: None,
    })
}

fn refarg_bytes(value: &(dyn RefArg + 'static)) -> Option<Vec<u8>> {
    let value = if value.arg_type() == ArgType::Variant {
        value.as_iter()?.next()?
    } else {
        value
    };
    Some(
        value
            .as_iter()?
            .filter_map(|item| item.as_u64().and_then(|byte| u8::try_from(byte).ok()))
            .collect(),
    )
}
