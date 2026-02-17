// Copyright (c) 2025 Omair Kamil
// See LICENSE file in root directory for license terms.

use std::{error::Error, str::FromStr, sync::Arc, time::Duration};

use bluer::agent::Agent;
use bluer::{Address, Session};
use log::{info, warn};
use tokio::{sync::mpsc, time::timeout};

pub struct BluetoothDevice {
    pub name: String,
    pub address: String,
}

/// Gets the Bluetooth adapter's current alias and address.
///
/// # Returns
/// * `Ok((alias, address))` - The adapter's current alias and Bluetooth address
/// * `Err(e)` - Error if the Bluetooth adapter cannot be accessed
pub async fn get_adapter_alias(
) -> Result<(String, String), Box<dyn Error + 'static>> {
    let session = Session::new().await?;
    let adapter = session.default_adapter().await?;
    let address = adapter.address().await?;
    let alias = adapter.alias().await?;

    Ok((alias, address.to_string()))
}

/// Sets the Bluetooth adapter's name/alias.
pub async fn set_adapter_name(
    desired_name: &str,
) -> Result<(), Box<dyn Error + 'static>> {
    let session = Session::new().await?;
    let adapter = session.default_adapter().await?;
    let alias = adapter.alias().await?;

    if alias != desired_name {
        match adapter.set_alias(desired_name.to_string()).await {
            Ok(_) => {
                info!("Successfully updated Bluetooth alias to '{}'", desired_name);
            }
            Err(e) => {
                warn!("Unable to update alias: {:?}", e);
                return Err(Box::new(e) as Box<dyn Error + 'static>);
            }
        }
    }

    Ok(())
}

/// Run pairing mode indefinitely, respecting the pairing_mode flag.
/// This function runs continuously and monitors the pairing_mode flag.
/// When pairing_mode is true, the adapter is set to discoverable and pairable.
/// When pairing_mode is false, the adapter is set to not discoverable and not pairable.
///
/// Registers a Bluetooth agent that auto-accepts pairing requests.
pub async fn run_pairing_mode(
    pairing_mode: Arc<tokio::sync::Mutex<bool>>,
) -> Result<(), Box<dyn Error + 'static>> {
    let session = Session::new().await.map_err(|e| {
        warn!("Failed to create Bluetooth session: {:?}", e);
        Box::new(e) as Box<dyn Error + 'static>
    })?;
    let adapter = session.default_adapter().await.map_err(|e| {
        warn!("Failed to get default Bluetooth adapter: {:?}", e);
        warn!("Make sure the BlueZ service (bluetoothd) is running. Start it with: sudo systemctl start bluetooth");
        Box::new(e) as Box<dyn Error + 'static>
    })?;
    adapter.set_powered(true).await.map_err(|e| {
        warn!("Failed to power on Bluetooth adapter: {:?}", e);
        Box::new(e) as Box<dyn Error + 'static>
    })?;

    let (tx, mut rx) = mpsc::channel::<Address>(1);

    // Register a Bluetooth agent that auto-accepts pairing confirmations.
    let tx_confirm = tx.clone();
    let agent = Agent {
        request_default: true,
        request_confirmation: Some(Box::new(move |req| {
            let tx = tx_confirm.clone();
            Box::pin(async move {
                info!("RequestConfirmation from {}: auto-accepting", req.device);
                let _ = tx.try_send(req.device);
                Ok(())
            })
        })),
        ..Default::default()
    };

    let _agent_handle = session.register_agent(agent).await.map_err(|e| {
        warn!("Failed to register Bluetooth agent: {:?}", e);
        Box::new(e) as Box<dyn Error + 'static>
    })?;

    info!("Registered Bluetooth pairing agent");

    let mut last_pairing_state: Option<bool> = None;

    loop {
        let current_pairing_state = *pairing_mode.lock().await;

        // Update adapter state if pairing mode changed or on first iteration.
        if last_pairing_state != Some(current_pairing_state) {
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
            last_pairing_state = Some(current_pairing_state);
        }

        // Check for incoming pairing requests with a timeout.
        match timeout(Duration::from_secs(5), rx.recv()).await {
            Ok(Some(address)) => {
                let device = adapter.device(address)?;
                let name = device.alias().await?;
                info!("Paired with client {}", name);
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