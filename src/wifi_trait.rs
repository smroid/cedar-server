// Copyright (c) 2024 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use canonical_error::CanonicalError;

pub trait WifiTrait {
    fn channel(&self) -> i32;
    fn ssid(&self) -> String;
    fn psk(&self) -> String;

    /// Updates the specified fields of this WiFi access point. Passing
    /// 'None' leaves the corresponding field unmodified.
    fn update_access_point(&mut self,
                           channel: Option<i32>,
                           ssid: Option<&str>,
                           psk: Option<&str>) -> Result<(), CanonicalError>;
}
