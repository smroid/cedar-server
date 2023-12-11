use std::sync::{Arc, Mutex};

use ascom_alpaca::{ASCOMResult, Server};
use ascom_alpaca::api::{AlignmentMode, CargoServerInfo, Device, EquatorialSystem, Telescope};
use async_trait::async_trait;

#[derive(Debug)]
pub struct CelestialPosition {
    // Both in degrees.
    pub ra: f64,  // 0..360
    pub dec: f64, // -90..90
}

#[derive(Debug)]
pub struct MyTelescope {
    position: Arc<Mutex<CelestialPosition>>
}

impl MyTelescope {
    pub fn new(position: Arc<Mutex<CelestialPosition>>) -> Self {
        MyTelescope{ position }
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
        let locked_position = self.position.lock().unwrap();
        Ok(locked_position.dec)
    }

    // Hours.
    async fn right_ascension(&self) -> ASCOMResult<f64> {
        let locked_position = self.position.lock().unwrap();
        Ok(locked_position.ra / 15.0)
    }

    async fn tracking(&self) -> ASCOMResult<bool> {
        Ok(false)
    }
}

pub fn create_alpaca_server(position: Arc<Mutex<CelestialPosition>>) -> Server {
    let mut server = Server {
        info: CargoServerInfo!(),
        ..Default::default()
    };
    server.listen_addr.set_port(11111);
    server.devices.register(MyTelescope::new(position));
    server
}
