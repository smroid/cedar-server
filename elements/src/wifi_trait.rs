// Copyright (c) 2026 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use std::sync::Arc;

use canonical_error::CanonicalError;

/// A WiFi network seen by a scan.
#[derive(Debug, Clone)]
pub struct WifiNetwork {
    pub ssid: String,

    /// Signal quality, 0-100. Absent if not reported.
    pub signal_strength: Option<i32>,

    /// True if the network requires a passphrase (any of WPA/WPA2/WPA3);
    /// false for an open network.
    pub secured: bool,
}

/// What the WiFi radio is currently doing, or is being switched to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WifiMode {
    /// Serving our own access point.
    AccessPoint,

    /// Joined (or joining) another network as a client.
    Client { ssid: String },

    /// No WiFi connection is up. The radio may also be powered down.
    Inactive,
}

/// Progress/outcome of joining an outside network in client mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WifiClientState {
    /// Association and/or DHCP in progress.
    Connecting,

    /// Joined and has an IP address.
    Connected,

    /// The network was not found on the air.
    NetworkNotFound,

    /// Association failed; typically a wrong passphrase.
    AuthFailed,

    /// Associated but did not obtain an IP address before timing out.
    NoIp,
}

/// Status of the current (or most recent) client-mode join.
#[derive(Debug, Clone)]
pub struct ClientStatus {
    pub ssid: String,
    pub state: WifiClientState,
    /// The assigned IPv4 address; absent if `state` is not `Connected`.
    pub ip_address: Option<String>,
}

/// Configuration of the access point this device puts up.
#[derive(Debug, Clone)]
pub struct AccessPointConfig {
    pub ssid: String,
    pub psk: String,
    pub channel: i32,
}

/// Notified by a `WifiTrait` implementation whenever its mode changes. Lets
/// callers react to WiFi state.
pub trait WifiModeObserver: Send + Sync {
    fn on_mode_changed(&self, old_mode: &WifiMode, new_mode: &WifiMode);
}

pub trait WifiTrait {
    // -- Access point --

    /// The configured access point, or None if this Cedar server has no
    /// access point.
    fn access_point(&self) -> Option<AccessPointConfig>;

    /// Updates the specified fields of the access point profile. Passing
    /// `None` leaves the corresponding field unchanged. Error if no access
    /// point is configured.
    fn update_access_point(
        &mut self,
        channel: Option<i32>,
        ssid: Option<&str>,
        psk: Option<&str>,
    ) -> Result<(), CanonicalError>;

    // -- Mode control --

    /// The mode the radio is currently in.
    fn mode(&self) -> WifiMode;

    /// Switches the WiFi mode.
    ///
    /// Returns once the switch has been *initiated*. For `Client` mode the
    /// join then proceeds asynchronously; poll `client_status()` for its
    /// outcome. An error is returned synchronously only when the request
    /// itself cannot be honored:
    ///   - `Client` mode with an SSID or passphrase that fails validation;
    ///   - `AccessPoint` mode when no access point is configured.
    ///
    /// `client_psk` is meaningful only for `Client` mode, where `None` means
    /// the network is open (unsecured).
    ///
    /// `client_join_timeout` is how long to keep retrying a `Client` join
    /// before giving up; `None` makes a single attempt.
    fn set_mode(
        &self,
        mode: WifiMode,
        client_psk: Option<&str>,
        client_join_timeout: Option<std::time::Duration>,
    ) -> Result<(), CanonicalError>;

    /// Registers (or replaces) the observer notified when the mode changes.
    ///
    /// Default no-op, since an implementation with nothing to observe (tests,
    /// a build with no activity indicator) need not override this.
    fn set_mode_observer(&self, _observer: Arc<dyn WifiModeObserver>) {}

    // -- Client mode --

    /// Status of the most recent `set_mode(Client, ..)` attempt: `Connecting`
    /// while it is in progress, then `Connected` / `AuthFailed` / `NoIp`. The
    /// terminal status is retained after the fact -- including after a failed
    /// join has fallen back to another mode -- so a caller polling for the
    /// outcome can still see it. None only if client mode has not been
    /// attempted this session.
    fn client_status(&self) -> Option<ClientStatus>;

    /// Scans for visible WiFi networks, strongest signal first.
    fn scan_wifi(&self) -> Result<Vec<WifiNetwork>, CanonicalError>;

    /// Called whenever an RPC is received, regardless of its kind.
    fn received_rpc(&self) {}
}
