use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use radiochron::ble::{AddressType, ManufacturerData, ServiceData};
use windows::core::Ref;
use windows::Devices::Bluetooth::Advertisement::{
    BluetoothLEAdvertisementReceivedEventArgs, BluetoothLEAdvertisementType,
    BluetoothLEAdvertisementWatcher, BluetoothLEScanningMode,
};
use windows::Devices::Bluetooth::BluetoothAddressType;
use windows::Foundation::TypedEventHandler;
use windows::Storage::Streams::{DataReader, IBuffer};

use crate::model::{
    bluetooth_uuid16, bluetooth_uuid32, classify_random_address, uuid_from_le_bytes, AdapterReport,
    AdvertisementSet, RawAdvertisement, ScanReport,
};
use crate::{Error, ScanObserver};

const PROGRESS_INTERVAL: Duration = Duration::from_millis(50);
const SERVICE_DATA_16_BIT_UUID: u8 = 0x16;
const SERVICE_DATA_32_BIT_UUID: u8 = 0x20;
const SERVICE_DATA_128_BIT_UUID: u8 = 0x21;

pub(crate) fn scan(duration: Duration, observer: &dyn ScanObserver) -> Result<ScanReport, Error> {
    let started = Instant::now();
    let advertisements = Arc::new(Mutex::new(AdvertisementSet::default()));
    let callback_errors = Arc::new(Mutex::new(Vec::<String>::new()));
    let watcher = BluetoothLEAdvertisementWatcher::new()
        .map_err(|error| Error::new(format!("create WinRT BLE watcher: {error}")))?;
    watcher
        .SetScanningMode(BluetoothLEScanningMode::Active)
        .map_err(|error| Error::new(format!("configure WinRT BLE scan: {error}")))?;
    let _ = watcher.SetAllowExtendedAdvertisements(true);

    let callback_items = Arc::clone(&advertisements);
    let callback_error_items = Arc::clone(&callback_errors);
    let handler: TypedEventHandler<
        BluetoothLEAdvertisementWatcher,
        BluetoothLEAdvertisementReceivedEventArgs,
    > = TypedEventHandler::new(
        move |_sender, args: Ref<BluetoothLEAdvertisementReceivedEventArgs>| {
            let result = args.ok().and_then(convert_advertisement).map(|item| {
                callback_items
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .merge(item);
            });
            if let Err(error) = result {
                let mut errors = callback_error_items
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let message = format!("decode WinRT BLE advertisement: {error}");
                if !errors.contains(&message) {
                    errors.push(message);
                }
            }
            Ok(())
        },
    );
    let token = watcher
        .Received(&handler)
        .map_err(|error| Error::new(format!("subscribe WinRT BLE watcher: {error}")))?;
    if let Err(error) = watcher.Start() {
        let _ = watcher.RemoveReceived(token);
        return Err(Error::new(format!("start WinRT BLE scan: {error}")));
    }

    while started.elapsed() < duration && !observer.is_cancelled() {
        let remaining = duration.saturating_sub(started.elapsed());
        thread::sleep(remaining.min(PROGRESS_INTERVAL));
        observer.progress(started.elapsed().min(duration), duration);
    }

    let mut errors = Vec::new();
    if let Err(error) = watcher.Stop() {
        errors.push(format!("stop WinRT BLE scan: {error}"));
    }
    if let Err(error) = watcher.RemoveReceived(token) {
        errors.push(format!("unsubscribe WinRT BLE watcher: {error}"));
    }
    errors.extend(
        callback_errors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .cloned(),
    );
    let items = Arc::try_unwrap(advertisements)
        .map_err(|_| Error::new("WinRT BLE callback did not stop"))?
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (advertisements, skipped_without_rssi) = items.finish();
    Ok(ScanReport::new(
        started.elapsed(),
        vec![AdapterReport {
            name: "WinRT Bluetooth LE".to_owned(),
            state: if observer.is_cancelled() {
                "cancelled".to_owned()
            } else {
                "powered_on".to_owned()
            },
            scan_started: true,
            errors: errors.clone(),
        }],
        advertisements,
        skipped_without_rssi,
        errors,
    ))
}

fn convert_advertisement(
    args: &BluetoothLEAdvertisementReceivedEventArgs,
) -> windows::core::Result<RawAdvertisement> {
    let address = args.BluetoothAddress()?;
    let address_type = match args.BluetoothAddressType()? {
        BluetoothAddressType::Public => AddressType::Public,
        BluetoothAddressType::Random => classify_random_address(address),
        _ => AddressType::Unknown,
    };
    let advertisement = args.Advertisement()?;
    let local_name = advertisement
        .LocalName()
        .ok()
        .map(|name| name.to_string())
        .filter(|name| !name.is_empty());
    let rssi_dbm = args.RawSignalStrengthInDBm().ok();
    let tx_power_dbm = args
        .TransmitPowerLevelInDBm()
        .ok()
        .and_then(|value| value.Value().ok());
    let connectable = args
        .AdvertisementType()
        .ok()
        .and_then(connectable_from_type);
    let service_uuids = advertisement
        .ServiceUuids()
        .map(|items| {
            items
                .into_iter()
                .map(|uuid| format!("{uuid:?}").to_ascii_lowercase())
                .collect()
        })
        .unwrap_or_default();
    let manufacturer_data = advertisement
        .ManufacturerData()
        .map(|items| {
            items
                .into_iter()
                .filter_map(|item| {
                    Some(ManufacturerData {
                        company_id: item.CompanyId().ok()?,
                        data: buffer_to_vec(&item.Data().ok()?).ok()?,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let service_data = advertisement
        .DataSections()
        .map(|items| {
            items
                .into_iter()
                .filter_map(|section| {
                    let kind = section.DataType().ok()?;
                    let data = buffer_to_vec(&section.Data().ok()?).ok()?;
                    decode_service_data(kind, &data)
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(RawAdvertisement {
        address: format_address(address),
        address_type,
        local_name,
        rssi_dbm,
        tx_power_dbm,
        connectable,
        service_uuids,
        manufacturer_data,
        service_data,
        protocol_identity: None,
    })
}

fn connectable_from_type(kind: BluetoothLEAdvertisementType) -> Option<bool> {
    if kind == BluetoothLEAdvertisementType::ConnectableUndirected
        || kind == BluetoothLEAdvertisementType::ConnectableDirected
    {
        Some(true)
    } else if kind == BluetoothLEAdvertisementType::ScannableUndirected
        || kind == BluetoothLEAdvertisementType::NonConnectableUndirected
        || kind == BluetoothLEAdvertisementType::ScanResponse
        || kind == BluetoothLEAdvertisementType::Extended
    {
        Some(false)
    } else {
        None
    }
}

fn buffer_to_vec(buffer: &IBuffer) -> windows::core::Result<Vec<u8>> {
    let reader = DataReader::FromBuffer(buffer)?;
    let mut bytes = vec![0; reader.UnconsumedBufferLength()? as usize];
    reader.ReadBytes(&mut bytes)?;
    Ok(bytes)
}

fn decode_service_data(kind: u8, bytes: &[u8]) -> Option<ServiceData> {
    let (uuid, data) = match kind {
        SERVICE_DATA_16_BIT_UUID if bytes.len() >= 2 => (
            bluetooth_uuid16(u16::from_le_bytes(bytes[..2].try_into().ok()?)),
            bytes[2..].to_vec(),
        ),
        SERVICE_DATA_32_BIT_UUID if bytes.len() >= 4 => (
            bluetooth_uuid32(u32::from_le_bytes(bytes[..4].try_into().ok()?)),
            bytes[4..].to_vec(),
        ),
        SERVICE_DATA_128_BIT_UUID if bytes.len() >= 16 => {
            (uuid_from_le_bytes(&bytes[..16])?, bytes[16..].to_vec())
        }
        _ => return None,
    };
    Some(ServiceData { uuid, data })
}

fn format_address(address: u64) -> String {
    let bytes = address.to_be_bytes();
    bytes[2..]
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
mod tests {
    use super::{decode_service_data, format_address};

    #[test]
    fn formats_six_byte_bluetooth_address() {
        assert_eq!(format_address(0x00aa_bbcc_ddee), "00:AA:BB:CC:DD:EE");
    }

    #[test]
    fn decodes_service_data_prefix() {
        let item = decode_service_data(0x16, &[0x0d, 0x18, 1, 2]).unwrap();
        assert_eq!(item.uuid, "0000180d-0000-1000-8000-00805f9b34fb");
        assert_eq!(item.data, [1, 2]);
    }
}
