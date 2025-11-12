// Copyright (c) 2025 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use std::sync::Arc;
use std::time::SystemTime;

use log::debug;

use ascom_alpaca::{ASCOMError, ASCOMErrorCode, ASCOMResult, Server};
use ascom_alpaca::api::{AlignmentMode, Axis, CargoServerInfo,
                        Device, EquatorialSystem, Telescope};
use async_trait::async_trait;

#[derive(Default, Debug)]
pub struct TelescopePosition {
    // The telescope's boresight position is determined by Cedar.
    pub boresight_ra: f64,  // 0..360
    pub boresight_dec: f64, // -90..90
    // If true, boresight_ra/boresight_dec are current. If false, they are stale.
    pub boresight_valid: bool,

    // SkySafari calls right_ascension() followed by declination(). The
    // right_ascension() call saves the Dec corresponding to the RA it returns,
    // so the subsequent call to declination() will return a consistent value.
    pub snapshot_dec: Option<f64>,

    // A slew is initiated by SkySafari. The slew can be terminated either by
    // SkySafari or Cedar.
    pub slew_target_ra: f64,  // 0..360
    pub slew_target_dec: f64, // -90..90
    pub slew_active: bool,

    // The "Set Time & Location" option must be enabled in the SkySafari
    // telescope preset options. These values are set by SkySafari and are
    // consumed (set to None) by Cedar server.
    pub site_latitude: Option<f64>,  // -90..90
    pub site_longitude: Option<f64>,  // -180..180, positive east.

    // These values are set by SkySafari and are consumed (set to None) by
    // Cedar server.
    pub sync_ra: Option<f64>,  // 0..360
    pub sync_dec: Option<f64>,  // -90..90

    // SkySafari doesn't seem to use these.
    pub target_ra: f64,  // 0..360
    pub target_dec: f64,  // -90..90

    // SkySafari doesn't seem to use this.
    pub utc_date: Option<SystemTime>,
}

impl TelescopePosition {
    pub fn new() -> Self {
        // Sky Safari doesn't display (0.0, 0.0).
        TelescopePosition{boresight_ra: 180.0, boresight_dec: 0.0, ..Default::default()}
    }
}

pub struct Callback(pub Box<dyn Fn() + Send + Sync>);

impl std::fmt::Debug for Callback {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
      write!(f, "Callback")
  }
}

#[derive(Default, Debug)]
struct MyTelescope {
    telescope_position: Arc<tokio::sync::Mutex<TelescopePosition>>,

    // SkySafari does not provide a way to signal that the boresight ra/dec
    // values are not valid. We instead "animate" the reported ra/dec position
    // when it is invalid.
    updates_while_invalid: tokio::sync::Mutex<i32>,

    // Called whenever SkySafari obtains our right_ascension().
    callback: Option<Callback>,
}

impl MyTelescope {
    // cb: function to be called whenever SkySafari interrogates our position.
    pub fn new(telescope_position: Arc<tokio::sync::Mutex<TelescopePosition>>,
               cb: Box<dyn Fn() + Send + Sync>) -> Self {
        MyTelescope{ telescope_position,
                     updates_while_invalid: tokio::sync::Mutex::new(0),
                     callback: Some(Callback(cb)) }
    }

    fn value_not_set_error(msg: &str) -> ASCOMError {
        ASCOMError{code: ASCOMErrorCode::VALUE_NOT_SET,
                   message: std::borrow::Cow::Owned(msg.to_string())}
    }
}

#[async_trait]
impl Device for MyTelescope {
    fn static_name(&self) -> &str { "CedarTelescopeEmulator" }
    fn unique_id(&self) -> &str { "CedarTelescopeEmulator-42" }

    async fn connected(&self) -> ASCOMResult<bool> { Ok(true) }
    async fn set_connected(&self, _connected: bool) -> ASCOMResult { Ok(()) }
}

#[async_trait]
impl Telescope for MyTelescope {
    async fn alignment_mode(&self) -> ASCOMResult<AlignmentMode> {
	    debug!("alignment_mode");
        // TODO: update if settings is alt/az.
        Ok(AlignmentMode::Polar)
    }

    async fn equatorial_system(&self) -> ASCOMResult<EquatorialSystem> {
	    debug!("equatorial_system");
        Ok(EquatorialSystem::J2000)
    }

    // Hours.
    async fn right_ascension(&self) -> ASCOMResult<f64> {
	    debug!("right_ascension");
        if let Some(ref cb) = self.callback {
            cb.0();
        }
        let mut locked_position = self.telescope_position.lock().await;
        locked_position.snapshot_dec = Some(locked_position.boresight_dec);
        Ok(locked_position.boresight_ra / 15.0)
    }
    // Degrees.
    async fn declination(&self) -> ASCOMResult<f64> {
	    debug!("declination");
        let mut locked_position = self.telescope_position.lock().await;
        let snapshot_dec = locked_position.snapshot_dec.take();
        if locked_position.boresight_valid {
            return if snapshot_dec.is_some() {
                Ok(snapshot_dec.unwrap())
            } else {
                Ok(locked_position.boresight_dec)
            };
        }
        // Sky Safari does not respond to error returns. To indicate
        // the position data is stale, we "wiggle" the position.
        let mut locked_updates = self.updates_while_invalid.lock().await;
        *locked_updates += 1;
        if *locked_updates & 1 == 0 {
            if locked_position.boresight_dec > 0.0 {
                Ok(locked_position.boresight_dec - 0.1)
            } else {
                Ok(locked_position.boresight_dec + 0.1)
            }
        } else {
            Ok(locked_position.boresight_dec)
        }
    }

    async fn can_move_axis(&self, _axis: Axis) -> ASCOMResult<bool> {
	    debug!("can_move_axis");
        Ok(false)
    }
    // Even though we define 'can_move_axis()' as false, SkySafari still
    // offers axis movement UI that calls move_axis().
    async fn move_axis(&self, _axis: Axis, _rate: f64) -> ASCOMResult {
	    debug!("move_axis");
        Ok(())  // Silently ignore.
    }

    async fn set_site_latitude(&self, site_lat: f64) -> ASCOMResult {
        debug!("set_site_latitude {}", site_lat);
        let mut locked_position = self.telescope_position.lock().await;
        locked_position.site_latitude = Some(site_lat);
        Ok(())
    }
    async fn site_latitude(&self) -> ASCOMResult<f64> {
        debug!("site_latitude");
        let locked_position = self.telescope_position.lock().await;
        match locked_position.site_latitude {
            Some(sl) => { Ok(sl) },
            None => { Err(Self::value_not_set_error("")) }
        }
    }
    async fn set_site_longitude(&self, site_lon: f64) -> ASCOMResult {
        debug!("set_site_longitude {}", site_lon);
        let mut locked_position = self.telescope_position.lock().await;
        locked_position.site_longitude = Some(site_lon);
        Ok(())
    }
    async fn site_longitude(&self) -> ASCOMResult<f64> {
        debug!("site_longitude");
        let locked_position = self.telescope_position.lock().await;
        match locked_position.site_longitude {
            Some(sl) => { Ok(sl) },
            None => { Err(Self::value_not_set_error("")) }
        }
    }

    // SkySafari doesn't seem to use the utc date methods..
    async fn set_utc_date(&self, utc_date: SystemTime) -> ASCOMResult {
        debug!("set_utc_date {:?}", utc_date);
        let mut locked_position = self.telescope_position.lock().await;
        locked_position.utc_date = Some(utc_date);
        Ok(())
    }
    async fn utc_date(&self) -> ASCOMResult<SystemTime> {
        debug!("utc_date");
        let locked_position = self.telescope_position.lock().await;
        match locked_position.utc_date {
            Some(ud) => { Ok(ud) },
            None => { Err(Self::value_not_set_error("")) }
        }
    }

    // SkySafari doesn't seem to use the 'target' methods.
    async fn set_target_declination(&self, target_dec: f64) -> ASCOMResult {
        debug!("set_target_declination {}", target_dec);
        let mut locked_position = self.telescope_position.lock().await;
        locked_position.target_dec = target_dec;
        Ok(())
    }
    async fn target_declination(&self) -> ASCOMResult<f64> {
        debug!("target_declination");
        let locked_position = self.telescope_position.lock().await;
        Ok(locked_position.target_dec)
    }
    async fn set_target_right_ascension(&self, target_ra: f64) -> ASCOMResult {
        debug!("set_target_right_ascension {}", target_ra);
        let mut locked_position = self.telescope_position.lock().await;
        locked_position.target_ra = target_ra * 15.0;
        Ok(())
    }
    async fn target_right_ascension(&self) -> ASCOMResult<f64> {
        debug!("target_right_ascension");
        let locked_position = self.telescope_position.lock().await;
        Ok(locked_position.target_ra / 15.0)
    }
    async fn slew_to_target_async(&self) -> ASCOMResult {
        debug!("slew_to_target_async");
        let mut locked_position = self.telescope_position.lock().await;
        locked_position.slew_active = true;
        Ok(())
    }

    async fn can_slew_async(&self) -> ASCOMResult<bool> {
        debug!("can_slew_async");
        Ok(true)
    }
    async fn slew_to_coordinates_async(&self, right_ascension: f64, declination: f64)
                                       -> ASCOMResult {
        debug!("slew_to_coordinates_async {} {}", right_ascension, declination);
        let mut locked_position = self.telescope_position.lock().await;
        locked_position.slew_target_ra = right_ascension * 15.0;
        locked_position.slew_target_dec = declination;
        locked_position.slew_active = true;
        Ok(())
    }
    async fn slewing(&self) -> ASCOMResult<bool> {
        let locked_position = self.telescope_position.lock().await;
        Ok(locked_position.slew_active)
    }
    async fn abort_slew(&self) -> ASCOMResult {
        debug!("abort_slew");
        let mut locked_position = self.telescope_position.lock().await;
        locked_position.slew_active = false;
        Ok(())
    }

    async fn can_sync(&self) -> ASCOMResult<bool> {
        debug!("can_sync");
        Ok(true)
    }
    async fn sync_to_coordinates(&self, right_ascension: f64, declination: f64)
                                 -> ASCOMResult {
        debug!("sync_to_coordinates {} {}", right_ascension, declination);
        let mut locked_position = self.telescope_position.lock().await;
        locked_position.sync_ra = Some(right_ascension * 15.0);
        locked_position.sync_dec = Some(declination);
        Ok(())
    }

    async fn tracking(&self) -> ASCOMResult<bool> {
        debug!("tracking");
        // TODO: sense whether solve results are fixed or moving at sideral rate.
        Ok(false)
    }
    async fn can_set_tracking(&self) -> ASCOMResult<bool> {
        debug!("can_set_tracking");
        Ok(false)
    }
}

// cb: function to be called whenever SkySafari interrogates our position.
pub fn create_alpaca_server(telescope_position: Arc<tokio::sync::Mutex<TelescopePosition>>,
                            cb: Box<dyn Fn() + Send + Sync>) -> Server {
    let mut server = Server {
        info: CargoServerInfo!(),
        ..Default::default()
    };
    server.listen_addr.set_port(11111);
    server.devices.register(MyTelescope::new(telescope_position, cb));
    server
}
