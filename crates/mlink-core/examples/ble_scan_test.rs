use btleplug::api::{Central, Manager as _, Peripheral as _, ScanFilter};
use btleplug::platform::Manager;
use std::time::Duration;

#[tokio::main]
async fn main() {
    let manager = Manager::new().await.expect("BLE manager");
    let adapters = manager.adapters().await.expect("adapters");
    println!("adapters: {}", adapters.len());

    let adapter = &adapters[0];
    println!("adapter: {:?}", adapter.adapter_info().await);

    println!("starting scan (no filter)...");
    adapter.start_scan(ScanFilter::default()).await.expect("scan");
    tokio::time::sleep(Duration::from_secs(5)).await;

    let peripherals = adapter.peripherals().await.expect("peripherals");
    println!("found {} peripherals:", peripherals.len());

    for p in &peripherals {
        if let Ok(Some(props)) = p.properties().await {
            println!("  name={:?} services={:?} rssi={:?}",
                props.local_name, props.services, props.rssi);
        }
    }

    let _ = adapter.stop_scan().await;
}
