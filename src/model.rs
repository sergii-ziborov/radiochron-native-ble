use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use radiochron::ble::{AddressType, Advertisement, ManufacturerData, ServiceData};
use serde::{Deserialize, Serialize};

/// Status of one native Bluetooth adapter used by a scan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterReport {
    pub name: String,
    pub state: String,
    pub scan_started: bool,
    #[serde(default)]
    pub errors: Vec<String>,
}

/// Normalized result of one native BLE scan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanReport {
    pub observed_at_epoch_ms: i64,
    pub elapsed_ms: u64,
    pub discovery_mode: String,
    pub adapters: Vec<AdapterReport>,
    pub advertisements: Vec<Advertisement>,
    pub skipped_without_rssi: usize,
    #[serde(default)]
    pub errors: Vec<String>,
}

impl ScanReport {
    pub(crate) fn new(
        elapsed: Duration,
        adapters: Vec<AdapterReport>,
        advertisements: Vec<Advertisement>,
        skipped_without_rssi: usize,
        errors: Vec<String>,
    ) -> Self {
        let observed_at_epoch_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| {
                i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
            });
        Self {
            observed_at_epoch_ms,
            elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            discovery_mode: "native_ble".to_owned(),
            adapters,
            advertisements,
            skipped_without_rssi,
            errors,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RawAdvertisement {
    pub address: String,
    pub address_type: AddressType,
    pub local_name: Option<String>,
    pub rssi_dbm: Option<i16>,
    pub tx_power_dbm: Option<i16>,
    pub connectable: Option<bool>,
    pub service_uuids: Vec<String>,
    pub manufacturer_data: Vec<ManufacturerData>,
    pub service_data: Vec<ServiceData>,
    pub protocol_identity: Option<String>,
}

impl RawAdvertisement {
    pub(crate) fn minimal(address: impl Into<String>) -> Self {
        Self {
            address: address.into(),
            address_type: AddressType::Unknown,
            local_name: None,
            rssi_dbm: None,
            tx_power_dbm: None,
            connectable: None,
            service_uuids: Vec::new(),
            manufacturer_data: Vec::new(),
            service_data: Vec::new(),
            protocol_identity: None,
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct AdvertisementSet {
    entries: BTreeMap<String, RawAdvertisement>,
}

impl AdvertisementSet {
    pub(crate) fn merge(&mut self, incoming: RawAdvertisement) {
        let key = incoming.address.to_ascii_lowercase();
        let entry = self
            .entries
            .entry(key)
            .or_insert_with(|| RawAdvertisement::minimal(incoming.address.clone()));
        entry.address = incoming.address;
        if incoming.address_type != AddressType::Unknown {
            entry.address_type = incoming.address_type;
        }
        if incoming.local_name.is_some() {
            entry.local_name = incoming.local_name;
        }
        if incoming.rssi_dbm.is_some() {
            entry.rssi_dbm = incoming.rssi_dbm;
        }
        if incoming.tx_power_dbm.is_some() {
            entry.tx_power_dbm = incoming.tx_power_dbm;
        }
        if incoming.connectable.is_some() {
            entry.connectable = incoming.connectable;
        }
        if incoming.protocol_identity.is_some() {
            entry.protocol_identity = incoming.protocol_identity;
        }
        union_strings(&mut entry.service_uuids, incoming.service_uuids);
        merge_manufacturer_data(&mut entry.manufacturer_data, incoming.manufacturer_data);
        merge_service_data(&mut entry.service_data, incoming.service_data);
    }

    pub(crate) fn finish(self) -> (Vec<Advertisement>, usize) {
        let mut skipped = 0;
        let mut advertisements = Vec::with_capacity(self.entries.len());
        for (_, mut entry) in self.entries {
            let Some(rssi_dbm) = entry.rssi_dbm else {
                skipped += 1;
                continue;
            };
            entry.service_uuids.sort();
            entry.manufacturer_data.sort_by_key(|item| item.company_id);
            entry
                .service_data
                .sort_by(|left, right| left.uuid.cmp(&right.uuid));
            advertisements.push(Advertisement {
                address: entry.address,
                address_type: entry.address_type,
                local_name: entry.local_name,
                rssi_dbm,
                tx_power_dbm: entry.tx_power_dbm,
                connectable: entry.connectable,
                service_uuids: entry.service_uuids,
                manufacturer_data: entry.manufacturer_data,
                service_data: entry.service_data,
                protocol_identity: entry.protocol_identity,
            });
        }
        advertisements.sort_by(|left, right| left.address.cmp(&right.address));
        (advertisements, skipped)
    }
}

fn union_strings(target: &mut Vec<String>, incoming: Vec<String>) {
    let mut all = target.drain(..).collect::<BTreeSet<_>>();
    all.extend(incoming);
    target.extend(all);
}

fn merge_manufacturer_data(target: &mut Vec<ManufacturerData>, incoming: Vec<ManufacturerData>) {
    let mut all = target
        .drain(..)
        .map(|item| (item.company_id, item.data))
        .collect::<BTreeMap<_, _>>();
    all.extend(
        incoming
            .into_iter()
            .map(|item| (item.company_id, item.data)),
    );
    target.extend(
        all.into_iter()
            .map(|(company_id, data)| ManufacturerData { company_id, data }),
    );
}

fn merge_service_data(target: &mut Vec<ServiceData>, incoming: Vec<ServiceData>) {
    let mut all = target
        .drain(..)
        .map(|item| (item.uuid, item.data))
        .collect::<BTreeMap<_, _>>();
    all.extend(incoming.into_iter().map(|item| (item.uuid, item.data)));
    target.extend(
        all.into_iter()
            .map(|(uuid, data)| ServiceData { uuid, data }),
    );
}

#[cfg(any(windows, test))]
pub(crate) fn classify_random_address(address: u64) -> AddressType {
    match (address >> 46) & 0b11 {
        0b11 => AddressType::RandomStatic,
        0b01 => AddressType::ResolvablePrivate,
        0b00 => AddressType::NonResolvablePrivate,
        _ => AddressType::Unknown,
    }
}

#[cfg(any(windows, test))]
pub(crate) fn bluetooth_uuid16(value: u16) -> String {
    format!("{value:08x}-0000-1000-8000-00805f9b34fb")
}

#[cfg(any(windows, test))]
pub(crate) fn bluetooth_uuid32(value: u32) -> String {
    format!("{value:08x}-0000-1000-8000-00805f9b34fb")
}

#[cfg(any(windows, test))]
pub(crate) fn uuid_from_le_bytes(bytes: &[u8]) -> Option<String> {
    let bytes: [u8; 16] = bytes.try_into().ok()?;
    let value = u128::from_le_bytes(bytes);
    let hex = format!("{value:032x}");
    Some(format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        bluetooth_uuid16, bluetooth_uuid32, classify_random_address, uuid_from_le_bytes,
        AdvertisementSet, RawAdvertisement,
    };
    use radiochron::ble::{AddressType, ManufacturerData};

    #[test]
    fn merges_cumulative_advertisements_and_sorts_output() {
        let mut set = AdvertisementSet::default();
        let mut first = RawAdvertisement::minimal("AA:BB");
        first.rssi_dbm = Some(-70);
        first.service_uuids = vec!["z".to_owned()];
        set.merge(first);
        let mut second = RawAdvertisement::minimal("aa:bb");
        second.local_name = Some("sensor".to_owned());
        second.rssi_dbm = Some(-60);
        second.service_uuids = vec!["a".to_owned()];
        second.manufacturer_data = vec![ManufacturerData {
            company_id: 7,
            data: vec![1],
        }];
        set.merge(second);

        let (items, skipped) = set.finish();
        assert_eq!(skipped, 0);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].rssi_dbm, -60);
        assert_eq!(items[0].local_name.as_deref(), Some("sensor"));
        assert_eq!(items[0].service_uuids, ["a", "z"]);
    }

    #[test]
    fn skips_entries_without_signal_strength() {
        let mut set = AdvertisementSet::default();
        set.merge(RawAdvertisement::minimal("unknown"));
        let (items, skipped) = set.finish();
        assert!(items.is_empty());
        assert_eq!(skipped, 1);
    }

    #[test]
    fn classifies_random_address_top_bits() {
        assert_eq!(
            classify_random_address(0b11 << 46),
            AddressType::RandomStatic
        );
        assert_eq!(
            classify_random_address(0b01 << 46),
            AddressType::ResolvablePrivate
        );
        assert_eq!(
            classify_random_address(0),
            AddressType::NonResolvablePrivate
        );
    }

    #[test]
    fn expands_bluetooth_uuids() {
        assert_eq!(
            bluetooth_uuid16(0x180d),
            "0000180d-0000-1000-8000-00805f9b34fb"
        );
        assert_eq!(
            bluetooth_uuid32(0x1234_5678),
            "12345678-0000-1000-8000-00805f9b34fb"
        );
        assert_eq!(
            uuid_from_le_bytes(&[
                0xfb, 0x34, 0x9b, 0x5f, 0x80, 0x00, 0x00, 0x80, 0x00, 0x10, 0x00, 0x00, 0x0d, 0x18,
                0x00, 0x00
            ])
            .as_deref(),
            Some("0000180d-0000-1000-8000-00805f9b34fb")
        );
    }
}
