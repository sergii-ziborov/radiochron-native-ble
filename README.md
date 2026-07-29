# radiochron-native-ble

Blocking, Tokio-free native BLE advertisement scanning for RadioChron.

The crate talks directly to the operating-system Bluetooth stack:

- Windows: WinRT `BluetoothLEAdvertisementWatcher`
- Linux: BlueZ through the system D-Bus
- macOS: CoreBluetooth

It intentionally does not depend on `btleplug`, Tokio, or `futures`. Consumers
receive normalized `radiochron::ble::Advertisement` values and can supply a
cooperative cancellation/progress observer.

```rust,no_run
use std::time::Duration;

fn main() -> Result<(), radiochron_native_ble::Error> {
    let report = radiochron_native_ble::scan(Duration::from_secs(3))?;
    println!("{} BLE advertisements", report.advertisements.len());
    Ok(())
}
```

Linux requires BlueZ and access to the system D-Bus. macOS applications must
include the Bluetooth usage description required by Apple for their packaging
model. Windows capability and privacy policy are controlled by the host
application.

## MSRV

Rust 1.78.

## License

MIT. See `LICENSE-MIT`.

