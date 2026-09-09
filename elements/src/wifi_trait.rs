// Copyright (c) 2026 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

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

pub trait WifiTrait {
    // Access point mode functions.

    fn channel(&self) -> i32;
    fn ssid(&self) -> String;
    fn psk(&self) -> String;

    /// Updates the specified fields of this WiFi access point. Passing
    /// 'None' leaves the corresponding field unmodified.
    fn update_access_point(
        &mut self,
        channel: Option<i32>,
        ssid: Option<&str>,
        psk: Option<&str>,
    ) -> Result<(), CanonicalError>;

    /// Enables or disables the WiFi access point connection. When disabled,
    /// the AP is brought down until explicitly re-enabled or the server
    /// reboots.
    fn set_enabled(&self, enabled: bool) -> Result<(), CanonicalError>;

    /// Returns whether the WiFi access point connection is currently active.
    fn is_enabled(&self) -> bool;

    // Client mode functions.

    /// Scans for visible WiFi networks, strongest signal first.
    fn scan_wifi(&self) -> Result<Vec<WifiNetwork>, CanonicalError>;
}
