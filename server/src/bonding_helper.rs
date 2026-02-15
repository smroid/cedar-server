// Copyright (c) 2025 Omair Kamil
// See LICENSE file in root directory for license terms.

use std::{error::Error, str::FromStr, sync::Arc, time::Duration};

use bluer::{
    agent::{Agent, ReqResult, RequestConfirmation},
    Address, Session,
};
use log::{info, warn};
use tokio::{sync::mpsc, time::timeout};

const BT_NAME_PREFIX: &str = "cedar-";

pub struct BluetoothDevice {
    pub name: String,
    pub address: String,
}

pub async fn get_adapter_info(
    serial: &str,
) -> Result<(String, String), Box<dyn Error + 'static>> {
    let session = Session::new().await?;
    let adapter = session.default_adapter().await?;
    let address = adapter.address().await?;
    let mut alias = adapter.alias().await?;
    let expected_alias = if serial.len() < 3 {
        warn!("Unexpected length for serial number: {}", serial);
        "cedar".to_string()
    } else {
        format!("{}{}", BT_NAME_PREFIX, &serial[serial.len() - 3..])
    };
    if alias != expected_alias {
        info!("Updating Bluetooth alias");
        match adapter.set_alias(expected_alias.clone()).await {
            Ok(_) => {
                alias = expected_alias.to_string();
            }
            Err(e) => {
                warn!("Unable to update alias: {:?}", e);
            }
        }
    }

    info!("Current device alias: {}", alias);
    Ok((alias, address.to_string()))
}

/// Run pairing mode indefinitely, respecting the pairing_mode flag.
/// This function runs continuously and monitors the pairing_mode flag.
/// When pairing_mode is true, the adapter is set to discoverable and pairable.
/// When pairing_mode is false, the adapter is set to not discoverable and not pairable.
pub async fn run_pairing_mode(
    pairing_mode: Arc<tokio::sync::Mutex<bool>>,
) -> Result<(), Box<dyn Error + 'static>> {
    let session = Session::new().await?;
    let adapter = session.default_adapter().await?;
    adapter.set_powered(true).await?;

    // Use a channel to pass device info from the agent.
    let (tx, mut rx) = mpsc::channel(10);

    // Clone sender for the agent closure to capture.
    let tx_req = tx.clone();

    // Start pairing agent that accepts any requests.
    let agent = Agent {
        request_default: true,
        request_confirmation: Some(Box::new(move |req| {
            let tx = tx_req.clone();
            Box::pin(request_confirmation(req, tx))
        })),
        ..Default::default()
    };

    // Keep the handle in scope to keep the agent active.
    let _handle = session.register_agent(agent).await?;

    info!("Bluetooth pairing mode loop started");

    let mut last_pairing_state = false;

    loop {
        let current_pairing_state = *pairing_mode.lock().await;

        // Update adapter state if pairing mode changed.
        if current_pairing_state != last_pairing_state {
            if current_pairing_state {
                info!("Enabling Bluetooth pairing");
                adapter.set_discoverable(true).await?;
                adapter.set_discoverable_timeout(0).await?; // 0 = indefinite
                adapter.set_pairable(true).await?;
            } else {
                info!("Disabling Bluetooth pairing");
                adapter.set_pairable(false).await?;
                adapter.set_discoverable(false).await?;
            }
            last_pairing_state = current_pairing_state;
        }

        // Check for incoming pairing requests with a timeout.
        match timeout(Duration::from_secs(5), rx.recv()).await {
            Ok(Some((address, passkey))) => {
                let device = adapter.device(address)?;
                let name = device.alias().await?;
                info!("Paired with client {} (passkey: {})", name, passkey);
            }
            Ok(None) => {
                // Channel closed, exit.
                break;
            }
            Err(_) => {
                // Timeout, continue loop to check pairing_mode flag.
            }
        }
    }

    info!("Pairing mode loop ended");
    Ok(())
}

async fn request_confirmation(
    req: RequestConfirmation,
    tx: mpsc::Sender<(Address, u32)>,
) -> ReqResult<()> {
    info!(
        "Pairing attempt from device {} with passkey {}",
        req.device, req.passkey
    );
    match tx.send((req.device, req.passkey)).await {
        Ok(_) => {
            info!("Accepted pairing request from client {}", req.device);
        }
        Err(e) => {
            warn!("Failed to send pairing confirmation for {}: {:?}",
                  req.device, e);
        }
    }
    Ok(())
}

pub async fn remove_bond(
    address: String,
) -> Result<(), Box<dyn Error + 'static>> {
    let session = Session::new().await?;
    let adapter = session.default_adapter().await?;
    adapter.set_powered(true).await?;

    let device = Address::from_str(&address)?;
    match adapter.remove_device(device).await {
        Ok(_) => {
            info!("Bond removed for device {}", address);
            Ok(())
        }
        Err(e) => {
            warn!("Failed to remove bond for device {}: {:?}", address, e);
            Err(Box::new(e))
        }
    }
}

pub async fn get_bonded_devices(
) -> Result<Vec<BluetoothDevice>, Box<dyn Error + 'static>> {
    let session = Session::new().await?;
    let adapter = session.default_adapter().await?;
    adapter.set_powered(true).await?;

    let mut result: Vec<BluetoothDevice> = Vec::new();
    let devices = adapter.device_addresses().await?;
    for addr in devices {
        info!("Found bonded client device: {}", addr);
        let device = adapter.device(addr)?;
        result.push(BluetoothDevice {
            name: device.alias().await?,
            address: addr.to_string(),
        });
    }
    Ok(result)
}
