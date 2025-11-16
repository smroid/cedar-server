// Copyright (c) 2025 Omair Kamil
// See LICENSE file in root directory for license terms.

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::Arc,
    time::SystemTime,
};

use async_trait::async_trait;
use bluer::{
    rfcomm::{Profile, Role, Stream},
    Session, Uuid,
};
use cedar_elements::astro_util::precess;
use chrono::{DateTime, Datelike, FixedOffset, Local};
use futures::StreamExt;
use log::{debug, info, warn};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::position_reporter::{Callback, TelescopePosition};

#[async_trait]
pub trait Lx200Telescope {
    async fn start(&mut self);
}

#[derive(Default, Debug)]
struct Lx200WifiTelescope {
    controller: Lx200Controller,
}

#[async_trait]
impl Lx200Telescope for Lx200WifiTelescope {
    async fn start(&mut self) {
        let listener = TcpListener::bind("0.0.0.0:4030").unwrap();
        info!("Running LX200 server on: {}", listener.local_addr().unwrap());

        for stream in listener.incoming() {
            let stream = stream.unwrap();
            self.handle_connection(stream).await;
        }
    }
}

impl Lx200WifiTelescope {
    pub fn new(
        telescope_position: Arc<tokio::sync::Mutex<TelescopePosition>>,
        cb: Box<dyn Fn() + Send + Sync>,
    ) -> Self {
        Lx200WifiTelescope {
            controller: Lx200Controller::new(telescope_position, cb),
        }
    }

    async fn handle_connection(&mut self, mut stream: TcpStream) {
        let mut buffer = [0; 1024];

        debug!("Starting to read from LX200 connection");
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => {
                    debug!("Client closed connection");
                    break;
                }
                Ok(n) => {
                    match self.controller.process_input(&buffer[..n]).await {
                        Some(result) => {
                            debug!("Writing to client: {}", result);
                            if let Err(e) = stream.write_all(result.as_bytes())
                            {
                                warn!("Failed to send data to client: {}", e);
                            }
                        }
                        None => {}
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {
                    // Interruption is recoverable, try reading again
                    continue;
                }
                Err(e) => {
                    // An actual network error occurred
                    debug!("Error reading from stream: {}", e);
                    break;
                }
            }
        }
    }
}

#[derive(Default, Debug)]
struct Lx200BtTelescope {
    controller: Lx200Controller,
}

#[async_trait]
impl Lx200Telescope for Lx200BtTelescope {
    // TODO: SkySafari on Android requires a bond, handle that here with either
    //       just works pairing or a canned PIN code
    async fn start(&mut self) {
        let session = Session::new().await.unwrap();
        let adapter = session.default_adapter().await.unwrap();
        // TODO: Handle the results from these calls
        adapter.set_powered(true).await;
        adapter.set_discoverable(true).await;

        let profile = Profile {
            uuid: Uuid::parse_str("00001101-0000-1000-8000-00805F9B34FB")
                .unwrap(),
            name: Some("Serial Port Profile".to_string()),
            role: Some(Role::Server),
            require_authentication: Some(false),
            require_authorization: Some(false),
            auto_connect: Some(false),
            ..Default::default()
        };

        let mut profile_handle =
            session.register_profile(profile).await.unwrap();
        info!(
            "Running LX200 server using SPP: {}",
            adapter.address().await.unwrap()
        );

        while let Some(req) = profile_handle.next().await {
            match req.accept() {
                Ok(stream) => {
                    self.handle_connection(stream).await;
                }
                Err(e) => {
                    warn!("Failed to accept connection: {}", e);
                }
            }
        }
    }
}

impl Lx200BtTelescope {
    pub fn new(
        telescope_position: Arc<tokio::sync::Mutex<TelescopePosition>>,
        cb: Box<dyn Fn() + Send + Sync>,
    ) -> Self {
        Lx200BtTelescope {
            controller: Lx200Controller::new(telescope_position, cb),
        }
    }

    async fn handle_connection(&mut self, mut stream: Stream) {
        let mut buffer = [0; 1024];

        info!("Starting to read from LX200 connection");
        loop {
            info!("Waiting for data from stream");
            match stream.read(&mut buffer).await {
                Ok(0) => {
                    info!("Client closed connection");
                    break;
                }
                Ok(n) => {
                    match self.controller.process_input(&buffer[..n]).await {
                        Some(result) => {
                            debug!("Writing to client: {}", result);
                            if let Err(e) =
                                stream.write_all(result.as_bytes()).await
                            {
                                warn!("Failed to send data to client: {}", e);
                            }
                        }
                        None => {}
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {
                    // Interruption is recoverable, try reading again
                    info!("Interrupted: {}", e);
                    continue;
                }
                Err(e) => {
                    // An actual network error occurred
                    info!("Error reading from stream: {}", e);
                    break;
                }
            }
        }
    }
}

#[derive(Default, Debug)]
struct Lx200Controller {
    telescope_position: Arc<tokio::sync::Mutex<TelescopePosition>>,

    // Animate the reported ra/dec position when it is invalid.
    animate_while_invalid: bool,

    // Called whenever client obtains our right ascension
    callback: Option<Callback>,

    // Pending target coordinates for sync/slew
    target_ra: Option<f64>,
    target_dec: Option<f64>,

    // Pending time
    timezone: Option<String>,
    time: Option<String>,

    // Local time, possibly provided by the client
    datetime: DateTime<FixedOffset>,

    // Current site set by the client
    latitude: String,
    longitude: String,

    // Current epoch to the closest tenth of a year
    epoch: f64,
}

impl Lx200Controller {
    pub fn new(
        telescope_position: Arc<tokio::sync::Mutex<TelescopePosition>>,
        cb: Box<dyn Fn() + Send + Sync>,
    ) -> Self {
        let dt = Local::now();
        let jnow = ((dt.year() as f64 + dt.ordinal0() as f64 / 365.0) * 10.0)
            .round()
            / 10.0;
        info!("Using now epoch: {}", jnow);
        Lx200Controller {
            telescope_position,
            animate_while_invalid: false,
            callback: Some(Callback(cb)),
            target_ra: None,
            target_dec: None,
            timezone: None,
            time: None,
            datetime: dt.with_timezone(dt.offset()),
            // Ideally these should be the current location in Cedar's settings
            latitude: "+00:00".to_string(),
            longitude: "000:00".to_string(),
            epoch: jnow,
        }
    }

    fn extract_command(s: &str) -> Option<String> {
        // We expect LX200 commands to be in the format ":[Command][Payload]#".
        // The command is is typically composed of one or two letters.
        let colon_index = s.find(':')?;
        if colon_index + 1 >= s.len() {
            return None;
        }

        let command: String = s[colon_index + 1..]
            .chars()
            .take_while(|c| c.is_alphabetic())
            .collect();
        if command.is_empty() {
            None
        } else {
            Some(command)
        }
    }

    fn get_failure() -> String {
        "0".to_string()
    }

    fn get_success() -> String {
        "1".to_string()
    }

    fn convert_to_j2000(&self, ra: f64, dec: f64) -> (f64, f64) {
        let (ra_rad, dec_rad) =
            precess(ra.to_radians(), dec.to_radians(), self.epoch, 2000.0);
        (ra_rad.to_degrees(), dec_rad.to_degrees())
    }

    fn convert_to_jnow(&self, ra: f64, dec: f64) -> (f64, f64) {
        let (ra_rad, dec_rad) =
            precess(ra.to_radians(), dec.to_radians(), 2000.0, self.epoch);
        (ra_rad.to_degrees(), dec_rad.to_degrees())
    }

    async fn get_ra(&self) -> String {
        if let Some(ref cb) = self.callback {
            cb.0();
        }
        let mut locked_position = self.telescope_position.lock().await;
        let (ra, dec) = self.convert_to_jnow(
            locked_position.boresight_ra,
            locked_position.boresight_dec,
        );
        // Keep a snapshot of the current dec for the next retrieval
        locked_position.snapshot_dec = Some(dec);
        // RA degrees need to be converted to hours before the conversion to
        // HH:MM:SS
        let (h, m, s) = Self::to_hms(ra / 15.0);
        format!("{h:02}:{m:02}:{s:02}#")
    }

    async fn get_dec(&mut self) -> String {
        let mut locked_position = self.telescope_position.lock().await;
        let snapshot_dec = locked_position.snapshot_dec.take();

        let mut dec = if snapshot_dec.is_some() {
            snapshot_dec.unwrap()
        } else {
            let (_, dec) = self.convert_to_jnow(
                locked_position.boresight_ra,
                locked_position.boresight_dec,
            );
            dec
        };

        let sign = if dec < 0.0 {
            dec = dec.abs();
            "-"
        } else {
            "+"
        };

        if !locked_position.boresight_valid {
            // Wiggle the position to indicate that it's invalid
            self.animate_while_invalid = !self.animate_while_invalid;
            if self.animate_while_invalid {
                dec += if dec > 0.0 { 0.1 } else { -0.1 };
            }
        }

        let (h, m, s) = Self::to_hms(dec);
        format!("{sign}{h:02}*{m:02}'{s:02}#")
    }

    fn set_ra(&mut self, cmd: &str) -> String {
        // The command is expected to be ":SrHH:MM:SS"
        if cmd.len() < 11 {
            warn!("Unexpected ra length");
            return Self::get_failure();
        }
        let degrees =
            Self::parse_coordinates(&cmd[3..5], &cmd[6..8], &cmd[9..11]);
        match degrees {
            Some(n) => {
                // Convert hours to 360-degree format
                let ra = n * 15.0;
                debug!("Set target ra to {}", ra);
                self.target_ra = Some(ra);
                Self::get_success()
            }
            None => Self::get_failure(),
        }
    }

    fn set_dec(&mut self, cmd: &str) -> String {
        // The command is expected to be ":SdsHH*MM:SS" where s is +/-
        if cmd.len() < 12 {
            warn!("Unexpected dec length");
            return Self::get_failure();
        }
        let degrees =
            Self::parse_coordinates(&cmd[3..6], &cmd[7..9], &cmd[10..12]);
        match degrees {
            Some(n) => {
                debug!("Set target dec to {}", n);
                self.target_dec = Some(n);
                Self::get_success()
            }
            None => Self::get_failure(),
        }
    }

    async fn slew(&mut self) -> String {
        // Check if there is a pending position set. The client will issue an Sr
        // command, followed by an Sd command, followed by MS to
        // initiate the movement. This applies to GoTo mode.
        if let (Some(ra), Some(dec)) =
            (self.target_ra.take(), self.target_dec.take())
        {
            let (j2000_ra, j2000_dec) = self.convert_to_j2000(ra, dec);
            info!("Slewing to {}, {}", j2000_ra, j2000_dec);
            let mut locked_position = self.telescope_position.lock().await;
            locked_position.slew_target_ra = j2000_ra;
            locked_position.slew_target_dec = j2000_dec;
            locked_position.slew_active = true;
            "0".to_string()
        } else {
            "1No object#".to_string()
        }
    }

    async fn sync(&mut self) -> String {
        // Check if there is a pending position set. The client will issue an Sr
        // command, followed by an Sd command, followed by CM to
        // sync/align. This applies to both GoTo and PushTo modes.
        if let (Some(ra), Some(dec)) =
            (self.target_ra.take(), self.target_dec.take())
        {
            let (j2000_ra, j2000_dec) = self.convert_to_j2000(ra, dec);
            info!("Syncing to {}, {}", j2000_ra, j2000_dec);
            let mut locked_position = self.telescope_position.lock().await;
            locked_position.sync_ra = Some(j2000_ra);
            locked_position.sync_dec = Some(j2000_dec);
        }
        // AutoStar always responds with this, so we'll do the same
        " M31 EX GAL MAG 3.5 SZ178.0'#".to_string()
    }

    async fn abort(&mut self) {
        let mut locked_position = self.telescope_position.lock().await;
        locked_position.slew_active = false;
    }

    async fn set_latitude(&mut self, cmd: &str) -> String {
        // The command is expected to be ":StsDD:MM" where s is +/-
        if cmd.len() < 9 {
            warn!("Unexpected lat length");
            return Self::get_failure();
        }
        let location = Self::parse_location(&cmd[3..6], &cmd[7..9]);
        match location {
            Some(n) => {
                debug!("Set latitude {}", n);
                self.latitude = cmd[3..9].to_string();
                let mut locked_position = self.telescope_position.lock().await;
                locked_position.site_latitude = Some(n);
                Self::get_success()
            }
            None => Self::get_failure(),
        }
    }

    async fn set_longitude(&mut self, cmd: &str) -> String {
        // The command is expected to be ":StDDD:MM"
        if cmd.len() < 9 {
            warn!("Unexpected lon length");
            return Self::get_failure();
        }
        let location = Self::parse_location(&cmd[3..6], &cmd[7..9]);
        match location {
            Some(n) => {
                self.longitude = cmd[3..9].to_string();
                // Cedar expects -180..180. The client will give 0..360, with
                // East being > 180
                let longitude = if n > 180.0 { 360.0 - n } else { -n };
                debug!("Set longitude {}", longitude);
                let mut locked_position = self.telescope_position.lock().await;
                locked_position.site_latitude = Some(longitude);
                Self::get_success()
            }
            None => Self::get_failure(),
        }
    }

    fn set_timezone(&mut self, cmd: &str) -> String {
        // The command is expected to be ":SGsXX.X" where s is +/-
        if cmd.len() < 8 {
            warn!("Unexpected tz length");
            return Self::get_failure();
        }
        let mut timezone = cmd[3..6].to_string();
        match &cmd[6..8] {
            ".0" => timezone.push_str("00"),
            ".2" => timezone.push_str("15"),
            ".5" => timezone.push_str("30"),
            ".8" => timezone.push_str("45"),
            _ => {
                warn!("Error parsing timezone: {}", &cmd[3..8]);
                return Self::get_failure();
            }
        }
        self.timezone = Some(timezone);
        Self::get_success()
    }

    fn set_time(&mut self, cmd: &str) -> String {
        // The command is expected to be ":SLHH:MM:SS"
        if cmd.len() < 11 {
            warn!("Unexpected time length");
            return Self::get_failure();
        }
        self.time = Some(cmd[3..11].to_string());
        Self::get_success()
    }

    async fn set_date(&mut self, cmd: &str) -> String {
        // The command is expected to be ":SCMM/DD/YY"
        if cmd.len() < 11 {
            warn!("Unexpected time length");
            return Self::get_failure();
        }
        if let (Some(timezone), Some(time)) =
            (self.timezone.take(), self.time.take())
        {
            const FMT: &str = "%z%H:%M:%S%D";
            let dtstr = timezone.clone() + &time + &cmd[3..11];
            let datetime: Result<DateTime<FixedOffset>, _> =
                DateTime::parse_from_str(&dtstr, FMT);
            match datetime {
                Ok(dt) => {
                    info!("Set date/time to {}", dt);
                    self.datetime = dt;
                    let mut locked_position =
                        self.telescope_position.lock().await;
                    locked_position.utc_date = Some(SystemTime::from(dt));
                    return "1Updating Planetary Data# #".to_string();
                }
                Err(e) => {
                    warn!("Error parsing date/time: {}", e);
                }
            }
        }
        Self::get_failure()
    }

    fn get_timezone(&self) -> String {
        let timezone = self.datetime.format("%z").to_string();
        let hours = &timezone[0..3];
        let partial = match &timezone[3..] {
            "15" => ".2",
            "30" => ".5",
            "45" => ".8",
            _ => ".0",
        };
        format!("{hours}{partial}#")
    }

    fn get_time(&self) -> String {
        self.datetime.format("%H:%M:%S#").to_string()
    }

    fn get_date(&self) -> String {
        self.datetime.format("%D#").to_string()
    }

    fn get_latitude(&self) -> String {
        format!("{0}#", self.latitude)
    }

    fn get_longitude(&self) -> String {
        format!("{0}#", self.longitude)
    }

    async fn get_distance_bars(&self) -> String {
        let locked_position = self.telescope_position.lock().await;
        if locked_position.slew_active {
            "\x7f#".to_string()
        } else {
            "#".to_string()
        }
    }

    fn to_hms(n: f64) -> (i64, i64, i64) {
        let hours = n.trunc() as i64;
        let minutes_float = (n - hours as f64) * 60.0;
        let minutes = minutes_float.trunc() as i64;
        let seconds = ((minutes_float - minutes as f64) * 60.0).round() as i64;
        (hours, minutes, seconds)
    }

    fn parse_coordinates(h: &str, m: &str, s: &str) -> Option<f64> {
        let hours: Result<i32, _> = h.parse();
        match hours {
            Err(e) => {
                warn!("Error parsing hours: {}", e);
                return None;
            }
            Ok(_) => {}
        }
        let minutes: Result<i32, _> = m.parse();
        match minutes {
            Err(e) => {
                warn!("Error parsing minutes: {}", e);
                return None;
            }
            Ok(_) => {}
        }
        let seconds: Result<i32, _> = s.parse();
        match seconds {
            Err(e) => {
                warn!("Error parsing seconds: {}", e);
                return None;
            }
            Ok(_) => {}
        }
        Some(
            hours.unwrap() as f64
                + minutes.unwrap() as f64 / 60.0
                + seconds.unwrap() as f64 / 3600.0,
        )
    }

    fn parse_location(deg: &str, min: &str) -> Option<f64> {
        Self::parse_coordinates(deg, min, "0")
    }

    async fn process_input(&mut self, buffer: &[u8]) -> Option<String> {
        let mut result = String::new();
        // Stellarium provides a leading #
        let start = if buffer[0] == b'#' { 1 } else { 0 };
        let in_data = String::from_utf8_lossy(&buffer[start..]);
        // SkySafari in Bluetooth mode sometimes sends more than 1 command
        for cmd in in_data.split("#") {
            match Self::extract_command(&cmd).as_deref() {
                Some("\x06") => {
                    info!("Received ack command");
                    result.push_str("A");
                }
                Some("CM") => {
                    debug!("Received sync command");
                    result.push_str(&self.sync().await);
                }
                Some("D") => {
                    debug!("Received distance bars command");
                    result.push_str(&self.get_distance_bars().await);
                }
                Some("GC") => {
                    debug!("Received get date command");
                    result.push_str(&self.get_date());
                }
                Some("GD") => {
                    debug!("Received get declination command");
                    result.push_str(&self.get_dec().await);
                }
                Some("GG") => {
                    debug!("Received get timezone command");
                    result.push_str(&self.get_timezone());
                }
                Some("GL") => {
                    debug!("Received get time command");
                    result.push_str(&self.get_time());
                }
                Some("GR") => {
                    debug!("Received get ra command");
                    result.push_str(&self.get_ra().await);
                }
                Some("GVD") => {
                    debug!("Received get firmware date command");
                    result.push_str("Nov 14 2025#");
                }
                Some("GVN") => {
                    debug!("Received get firmware version command");
                    result.push_str("01.0#");
                }
                Some("GVP") => {
                    debug!("Received get product command");
                    result.push_str("Cedar#");
                }
                Some("GVT") => {
                    debug!("Received get firmware time command");
                    result.push_str("23:00:00#");
                }
                Some("GW") => {
                    debug!("Received get status command");
                    result.push_str("AT1");
                }
                Some("Gg") => {
                    debug!("Received get longitude command");
                    result.push_str(&self.get_longitude());
                }
                Some("Gt") => {
                    debug!("Received get latitude command");
                    result.push_str(&self.get_latitude());
                }
                Some("MS") => {
                    debug!("Received slew to object command");
                    result.push_str(&self.slew().await);
                }
                Some("Q") => {
                    debug!("Received abort command");
                    self.abort().await;
                }
                Some("RS") => {
                    debug!("Received set rate to slew command");
                }
                Some("SC") => {
                    debug!("Received set date command: {}", in_data);
                    result.push_str(&self.set_date(&in_data).await);
                }
                Some("SG") => {
                    debug!("Received set timezone command: {}", in_data);
                    result.push_str(&self.set_timezone(&in_data));
                }
                Some("SL") => {
                    debug!("Received set time command: {}", in_data);
                    result.push_str(&self.set_time(&in_data));
                }
                Some("Sd") => {
                    debug!("Received set declination command: {}", in_data);
                    result.push_str(&self.set_dec(&in_data));
                }
                Some("Sg") => {
                    debug!("Received set longitude command: {}", in_data);
                    result.push_str(&self.set_longitude(&in_data).await);
                }
                Some("Sr") => {
                    debug!("Received set ra command: {}", in_data);
                    result.push_str(&self.set_ra(&in_data));
                }
                Some("St") => {
                    debug!("Received set latitude command: {}", in_data);
                    result.push_str(&self.set_latitude(&in_data).await);
                }
                Some("U") => {
                    debug!("Received precision toggle command");
                }
                Some(_) => {
                    info!("Unknown command: {}", in_data);
                }
                None => {}
            }
        }
        if result.is_empty() {
            None
        } else {
            Some(result)
        }
    }
}

pub fn create_lx200_server(
    telescope_position: Arc<tokio::sync::Mutex<TelescopePosition>>,
    cb: Box<dyn Fn() + Send + Sync>,
    use_bluetooth: bool,
) -> Box<dyn Lx200Telescope + Send> {
    if use_bluetooth {
        Box::new(Lx200BtTelescope::new(telescope_position, cb))
    } else {
        Box::new(Lx200WifiTelescope::new(telescope_position, cb))
    }
}
