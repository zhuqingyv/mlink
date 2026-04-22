use btleplug::api::{Central, Manager as _, Peripheral as _, ScanFilter};
use btleplug::platform::Manager;
use std::time::Duration;
use tokio::time::timeout;
use uuid::Uuid;

const MLINK_SERVICE_UUID: Uuid = match Uuid::try_parse("0000FFAA-0000-1000-8000-00805F9B34FB") {
    Ok(u) => u,
    Err(_) => panic!("invalid MLINK_SERVICE_UUID literal"),
};

#[tokio::main]
async fn main() {
    let manager = Manager::new().await.expect("BLE manager");
    let adapters = manager.adapters().await.expect("adapters");
    println!("adapters: {}", adapters.len());
    if adapters.is_empty() {
        eprintln!("no BLE adapter available");
        return;
    }

    let adapter = &adapters[0];
    println!("adapter: {:?}", adapter.adapter_info().await);

    println!("starting scan (no filter, 5s)...");
    adapter.start_scan(ScanFilter::default()).await.expect("scan");
    tokio::time::sleep(Duration::from_secs(5)).await;
    let _ = adapter.stop_scan().await;

    let peripherals = adapter.peripherals().await.expect("peripherals");
    println!("\n=== found {} peripherals ===", peripherals.len());

    // 1) List all devices
    for (i, p) in peripherals.iter().enumerate() {
        match p.properties().await {
            Ok(Some(props)) => {
                println!(
                    "[{}] name={:?} services={:?} rssi={:?}",
                    i, props.local_name, props.services, props.rssi
                );
            }
            Ok(None) => println!("[{}] (no properties)", i),
            Err(e) => println!("[{}] properties error: {}", i, e),
        }
    }

    // 2) Try to connect each named device
    println!("\n=== probing named devices ===");
    for p in &peripherals {
        let props = match p.properties().await {
            Ok(Some(pr)) => pr,
            _ => continue,
        };
        let name = match &props.local_name {
            Some(n) => n.clone(),
            None => continue,
        };

        println!("\n--- probing \"{}\" (id={:?}) ---", name, p.id());

        match timeout(Duration::from_secs(5), p.connect()).await {
            Ok(Ok(())) => println!("  connected"),
            Ok(Err(e)) => {
                println!("  connect error: {}", e);
                continue;
            }
            Err(_) => {
                println!("  connect timeout (5s)");
                continue;
            }
        }

        match timeout(Duration::from_secs(5), p.discover_services()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                println!("  discover_services error: {}", e);
                let _ = p.disconnect().await;
                continue;
            }
            Err(_) => {
                println!("  discover_services timeout (5s)");
                let _ = p.disconnect().await;
                continue;
            }
        }

        let services = p.services();
        println!("  services: {}", services.len());
        let mut has_mlink = false;
        for s in &services {
            let is_mlink = s.uuid == MLINK_SERVICE_UUID;
            if is_mlink {
                has_mlink = true;
            }
            println!(
                "    service {} {}",
                s.uuid,
                if is_mlink { "<-- MLINK_SERVICE_UUID" } else { "" }
            );
            for c in &s.characteristics {
                println!("      char {} props={:?}", c.uuid, c.properties);
            }
        }
        println!(
            "  MLINK_SERVICE_UUID present: {}",
            if has_mlink { "YES" } else { "no" }
        );

        if let Err(e) = p.disconnect().await {
            println!("  disconnect error: {}", e);
        } else {
            println!("  disconnected");
        }
    }

    println!("\ndone");
}
