// Copyright (c) 2025 Omair Kamil
// See LICENSE file in root directory for license terms.

use std::{
    error::Error,
    str::FromStr,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

use bluer::{agent::Agent, Address, Session};
use log::{info, warn};
use tokio::{sync::mpsc, time::timeout};

/// Timestamp of the last completed hard BT stack reset. Concurrent BT
/// connection closes can each independently observe a wedged controller
/// and try to run the heavy recovery. The second one is redundant and
/// disruptive (it fires against a freshly-reset stack).
fn last_hard_reset() -> &'static Mutex<Option<Instant>> {
    static LAST_HARD_RESET: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
    LAST_HARD_RESET.get_or_init(|| Mutex::new(None))
}

/// Minimum interval between successive hard BT stack resets. If a caller
/// finds that the last reset finished within this window, it skips.
const HARD_RESET_COOLDOWN: Duration = Duration::from_secs(15);

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
                info!(
                    "Successfully updated Bluetooth alias to '{}'",
                    desired_name
                );
            }
            Err(e) => {
                warn!("Unable to update alias: {:?}", e);
                return Err(Box::new(e) as Box<dyn Error + 'static>);
            }
        }
    }

    Ok(())
}

/// Sets the BCM43430A1 link policy to role-switch only, disabling sniff mode.
///
/// The kernel periodically issues HCI_Sniff_Mode commands on active ACL links;
/// on BCM43430A1 these time out while RFCOMM traffic is flowing, wedging the
/// UART. Must be called on startup and after every HCI reset.
fn apply_lp_rswitch() {
    let result = std::process::Command::new("sudo")
        .args(["hciconfig", "hci0", "lp", "rswitch"])
        .output();
    match result {
        Ok(o) if o.status.success() => {
            info!("Disabled BT sniff mode via hciconfig lp rswitch")
        }
        Ok(o) => {
            warn!(
                "Failed to disable BT sniff mode: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            )
        }
        Err(e) => warn!("Failed to disable BT sniff mode: {:?}", e),
    }
}

/// Outcome of a call to `reset_hci_controller`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetOutcome {
    /// Tier-1 light reset succeeded; the HCI controller is healthy and any
    /// existing `bluer::Session` and profile registration remain valid.
    LightResetOk,
    /// Tier-1 failed, so tier-2 was invoked - either running the hard reset
    /// (bluetoothd stop + hci_uart unbind + GPIO 42 toggle + rebind +
    /// bluetoothd start) or skipping it because a concurrent caller ran one
    /// within `HARD_RESET_COOLDOWN`. Either way the `bluer::Session` and
    /// profile registration are invalidated and callers must exit and
    /// rebuild them.
    HardReset,
}

/// Resets the BT stack after a session ends. Tries a light HCI reset
/// first; if that fails (typically because the UART is deeply wedged and
/// `HCI_Reset` itself times out), escalates to a hard stack reset.
///
/// See `ResetOutcome` for the meaning of each variant.
///
/// Blocking: runs subprocess calls (and, on escalation, `std::thread::sleep`
/// pauses totaling ~1s). Callers running on the async runtime should invoke
/// this via `tokio::task::spawn_blocking` to avoid stalling an async worker.
pub fn reset_hci_controller() -> ResetOutcome {
    let reset_result = std::process::Command::new("sudo")
        .args(["hciconfig", "hci0", "reset"])
        .output();
    let light_ok = match reset_result {
        Ok(o) if o.status.success() => {
            info!("HCI controller reset after BT session");
            true
        }
        Ok(o) => {
            warn!(
                "HCI reset failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            );
            false
        }
        Err(e) => {
            warn!("HCI reset failed: {:?}", e);
            false
        }
    };
    if light_ok {
        apply_lp_rswitch();
        return ResetOutcome::LightResetOk;
    }
    hard_reset_bt_stack();
    ResetOutcome::HardReset
}

/// Heavy-hammer recovery for a wedged BT stack. Fully tears down and
/// rebuilds the kernel side: unbinds hci_uart_bcm, hardware-resets the
/// BCM43430A1 via its shutdown GPIO (GPIO 42 on Pi Zero 2 W), rebinds
/// the driver, and restarts bluetoothd.
///
/// Skipped (no-op) if another caller ran a hard reset within
/// `HARD_RESET_COOLDOWN`. Only callable via `reset_hci_controller`,
/// which owns the tiered escalation policy.
fn hard_reset_bt_stack() {
    // Serialize with any in-flight or recently completed reset. Holding
    // the lock across the whole reset means concurrent callers block
    // rather than issuing overlapping resets; after the first one
    // releases, latecomers see a fresh timestamp and skip.
    let mut last = last_hard_reset().lock().unwrap();
    if let Some(when) = *last {
        if when.elapsed() < HARD_RESET_COOLDOWN {
            info!(
                "Skipping hard BT stack reset; another completed {:.1}s ago",
                when.elapsed().as_secs_f32()
            );
            return;
        }
    }

    warn!(
        "Escalating to hard BT stack reset (bluetoothd stop + \
            hci_uart unbind + GPIO 42 toggle + rebind + bluetoothd start)"
    );

    // Stop bluetoothd so it releases the HCI socket cleanly and doesn't
    // race with the unbind/rebind cycle.
    run_step(&["sudo", "systemctl", "stop", "bluetooth"], "stop bluetooth");

    // Unbind the serdev driver. This releases the UART and lets the
    // kernel forget any state associated with hci0.
    run_step(
        &[
            "sudo",
            "sh",
            "-c",
            "echo serial0-0 > /sys/bus/serial/drivers/hci_uart_bcm/unbind",
        ],
        "unbind hci_uart_bcm",
    );

    // Hardware-reset the BCM43430A1 chip via its shutdown GPIO (BT_ON,
    // GPIO 42). Drive high to shut down, then low to power back on. Sleeps
    // between are for the chip's power-up settling.
    run_step(&["sudo", "raspi-gpio", "set", "42", "op", "dh"], "BT_ON high");
    std::thread::sleep(Duration::from_millis(200));
    run_step(&["sudo", "raspi-gpio", "set", "42", "op", "dl"], "BT_ON low");
    std::thread::sleep(Duration::from_millis(200));

    // Rebind the serdev driver so the kernel re-runs the BCM firmware
    // load and re-attaches hci0.
    run_step(
        &[
            "sudo",
            "sh",
            "-c",
            "echo serial0-0 > /sys/bus/serial/drivers/hci_uart_bcm/bind",
        ],
        "rebind hci_uart_bcm",
    );

    // Restart bluetoothd so it re-registers on the new hci0.
    run_step(&["sudo", "systemctl", "start", "bluetooth"], "start bluetooth");

    // Give bluetoothd a moment to attach to hci0 before re-applying link
    // policy. Without this the lp rswitch call can race against the
    // adapter coming up.
    std::thread::sleep(Duration::from_millis(500));

    apply_lp_rswitch();
    *last = Some(Instant::now());
    warn!("Hard BT stack reset complete");
}

fn run_step(argv: &[&str], label: &str) {
    let (bin, args) = argv.split_first().expect("argv non-empty");
    let result = std::process::Command::new(bin).args(args).output();
    match result {
        Ok(o) if o.status.success() => info!("{}: ok", label),
        Ok(o) => warn!(
            "{}: failed: {}",
            label,
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => warn!("{}: exec failed: {:?}", label, e),
    }
}

/// Run pairing mode indefinitely, respecting the pairing_mode flag.
/// This function runs continuously and monitors the pairing_mode flag.
/// When pairing_mode is true, the adapter is set to discoverable and pairable.
/// When pairing_mode is false, the adapter is set to not discoverable and not
/// pairable.
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
        warn!(
            "Make sure the BlueZ service (bluetoothd) is running. \
                Start it with: sudo systemctl start bluetooth"
        );
        Box::new(e) as Box<dyn Error + 'static>
    })?;
    adapter.set_powered(true).await.map_err(|e| {
        warn!("Failed to power on Bluetooth adapter: {:?}", e);
        Box::new(e) as Box<dyn Error + 'static>
    })?;
    apply_lp_rswitch();

    let (tx, mut rx) = mpsc::channel::<Address>(1);

    // Register a Bluetooth agent that auto-accepts pairing confirmations.
    let tx_confirm = tx.clone();
    let agent = Agent {
        request_default: true,
        request_confirmation: Some(Box::new(move |req| {
            let tx = tx_confirm.clone();
            Box::pin(async move {
                info!(
                    "RequestConfirmation from {}: auto-accepting",
                    req.device
                );
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

    // Zero timeout means "discoverable never auto-expires". Set once at
    // startup; the loop below toggles the discoverable flag itself.
    adapter.set_discoverable_timeout(0).await.map_err(|e| {
        warn!("set_discoverable_timeout(0) failed: {:?}", e);
        e
    })?;

    let mut last_pairing_state: Option<bool> = None;

    loop {
        let current_pairing_state = *pairing_mode.lock().await;

        // Update adapter state if pairing mode changed or on first iteration.
        // We toggle both pairable and discoverable so the adapter appears in
        // scan results only while the user has opted into pairing mode.
        if last_pairing_state != Some(current_pairing_state) {
            if current_pairing_state {
                info!("Enabling Bluetooth pairing (pairable + discoverable)");
                adapter.set_pairable(true).await.map_err(|e| {
                    warn!("set_pairable(true) failed: {:?}", e);
                    e
                })?;
                adapter
                    .set_discoverable(true)
                    .await
                    .map_err(|e| {
                        warn!("set_discoverable(true) failed: {:?}", e)
                    })
                    .ok();
            } else {
                info!(
                    "Disabling Bluetooth pairing (pairable + discoverable off)"
                );
                adapter
                    .set_pairable(false)
                    .await
                    .map_err(|e| warn!("set_pairable(false) failed: {:?}", e))
                    .ok();
                adapter
                    .set_discoverable(false)
                    .await
                    .map_err(|e| {
                        warn!("set_discoverable(false) failed: {:?}", e)
                    })
                    .ok();
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
