use std::sync::{Arc, Mutex};

use log::info;
use ascom_alpaca::{ASCOMResult, Server};
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

    // A slew is initiated by SkySafari. The slew can be terminated either by
    // SkySafari or Cedar.
    pub slew_target_ra: f64,  // 0..360
    pub slew_target_dec: f64, // -90..90
    pub slew_active: bool,
}

impl TelescopePosition {
    pub fn new() -> Self {
        // Sky Safari doesn't display (0.0, 0.0).
        TelescopePosition{boresight_ra: 180.0, boresight_dec: 0.0, ..Default::default()}
    }
}

#[derive(Default, Debug)]
struct MyTelescope {
    telescope_position: Arc<Mutex<TelescopePosition>>,

    // SkySafari does not provide a way to signal that the boresight ra/dec
    // values are not valid. We instead "animate" the reported ra/dec position
    // when it is invalid.
    updates_while_invalid: Mutex<i32>,
}

impl MyTelescope {
    pub fn new(telescope_position: Arc<Mutex<TelescopePosition>>) -> Self {
        MyTelescope{ telescope_position, updates_while_invalid: Mutex::new(0) }
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
        Ok(AlignmentMode::Polar)
    }

    async fn equatorial_system(&self) -> ASCOMResult<EquatorialSystem> {
        Ok(EquatorialSystem::J2000)
    }

    // Degrees.
    async fn declination(&self) -> ASCOMResult<f64> {
        let locked_position = self.telescope_position.lock().unwrap();
        if locked_position.boresight_valid {
            return Ok(locked_position.boresight_dec);
        }
        // Sky Safari does not respond to error returns. To indicate
        // the position data is stale, we "wiggle" the position.
        let mut locked_updates = self.updates_while_invalid.lock().unwrap();
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

    // Hours.
    async fn right_ascension(&self) -> ASCOMResult<f64> {
        let locked_position = self.telescope_position.lock().unwrap();
        Ok(locked_position.boresight_ra / 15.0)
    }

    async fn can_move_axis(&self, _axis: Axis) -> ASCOMResult<bool> {
        info!("can_move_axis");  // TEMPORARY
        Ok(false)
    }

    async fn can_slew_async(&self) -> ASCOMResult<bool> {
        info!("can_slew_async");  // TEMPORARY
        Ok(true)
    }

    async fn slew_to_coordinates_async(&self, right_ascension: f64, declination: f64)
                                       -> ASCOMResult {
        info!("slew_to_coordinates_async ra {} dec {}",
              right_ascension, declination);  // TEMPORARY
        let mut locked_position = self.telescope_position.lock().unwrap();
        locked_position.slew_target_ra = right_ascension * 15.0;
        locked_position.slew_target_dec = declination;
        locked_position.slew_active = true;
        Ok(())
    }

    async fn slewing(&self) -> ASCOMResult<bool> {
        let locked_position = self.telescope_position.lock().unwrap();
        // info!("slewing: {}", locked_position.slew_active);  // TEMPORARY
        Ok(locked_position.slew_active)
    }

    async fn abort_slew(&self) -> ASCOMResult {
        info!("abort_slew");  // TEMPORARY
        let mut locked_position = self.telescope_position.lock().unwrap();
        locked_position.slew_active = false;
        Ok(())
    }

    async fn tracking(&self) -> ASCOMResult<bool> {
        // TODO: sense whether solve results are fixed or moving at sideral rate.
        Ok(false)
    }
    async fn can_set_tracking(&self) -> ASCOMResult<bool> {
        Ok(false)
    }
}

pub fn create_alpaca_server(telescope_position: Arc<Mutex<TelescopePosition>>)
                            -> Server {
    let mut server = Server {
        info: CargoServerInfo!(),
        ..Default::default()
    };
    server.listen_addr.set_port(11111);
    server.devices.register(MyTelescope::new(telescope_position));
    server
}
