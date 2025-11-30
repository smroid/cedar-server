// Copyright (c) 2025 Omair Kamil
// See LICENSE file in root directory for license terms.

use std::{error::Error, str::FromStr, time::Duration};

use bluer::{
    agent::{Agent, ReqResult, RequestConfirmation},
    Address, Session,
};
use log::info;
use tokio::{sync::mpsc, time::timeout};

pub struct BluetoothDevice {
    pub name: String,
    pub address: String,
}

pub async fn get_adapter_alias() -> Result<String, Box<dyn Error + 'static>> {
    let session = Session::new().await?;
    let adapter = session.default_adapter().await?;
    let alias = adapter.alias().await?;
    info!("Current device alias: {}", alias);
    Ok(alias)
}

pub async fn start_bonding(
) -> Result<Option<(String, u32)>, Box<dyn Error + 'static>> {
    let session = Session::new().await?;
    let adapter = session.default_adapter().await?;
    adapter.set_powered(true).await?;

    // Use a channel to pass the address back to the main thread.
    let (tx, mut rx) = mpsc::channel(1);

    // Clone sender for the agent closure to capture
    let tx_req = tx.clone();

    // Start pairing, accepting any requests.
    let agent = Agent {
        request_default: true,
        request_confirmation: Some(Box::new(move |req| {
            let tx = tx_req.clone();
            Box::pin(request_confirmation(req, tx))
        })),
        ..Default::default()
    };

    // Keep the handle in scope to keep the agent active
    let _handle = session.register_agent(agent).await?;

    adapter.set_discoverable(true).await?;
    adapter.set_discoverable_timeout(55).await?;
    adapter.set_pairable(true).await?;

    info!("Accepting pairings - waiting up to 55 seconds...");

    // Wait for the address or timeout
    let result = timeout(Duration::from_secs(55), rx.recv()).await;

    let paired_info = match result {
        Ok(Some((address, passkey))) => {
            let device = adapter.device(address)?;
            let name = device.alias().await?;
            info!("Paired with {}", name);
            Some((name, passkey))
        }
        _ => {
            info!("Timeout reached");
            None
        }
    };

    adapter.set_pairable(false).await?;
    adapter.set_discoverable(false).await?;
    Ok(paired_info)
}

async fn request_confirmation(
    req: RequestConfirmation,
    tx: mpsc::Sender<(Address, u32)>,
) -> ReqResult<()> {
    info!("Confirming request from {} with key {}", req.device, req.passkey);
    let _ = tx.send((req.device, req.passkey)).await;
    Ok(())
}

pub async fn remove_bond(
    address: String,
) -> Result<(), Box<dyn Error + 'static>> {
    let session = Session::new().await?;
    let adapter = session.default_adapter().await?;
    adapter.set_powered(true).await?;

    let device = Address::from_str(&address)?;
    adapter.remove_device(device).await?;
    info!("Removed bond: {}", address);
    Ok(())
}

pub async fn get_bonded_devices(
) -> Result<Vec<BluetoothDevice>, Box<dyn Error + 'static>> {
    let session = Session::new().await?;
    let adapter = session.default_adapter().await?;
    adapter.set_powered(true).await?;

    let mut result: Vec<BluetoothDevice> = Vec::new();
    let devices = adapter.device_addresses().await?;
    for addr in devices {
        info!("Found bonded device: {}", addr);
        let device = adapter.device(addr)?;
        result.push(BluetoothDevice {
            name: device.alias().await?,
            address: addr.to_string(),
        });
    }
    Ok(result)
}