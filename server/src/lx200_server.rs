// Implementation of the Meade LX200 protocol for Cedar.
//
// References for LX200 command set include:
//    https://www.astro.louisville.edu/software/xmtel/archive/xmtel-indi-6.0/xmtel-6.0l/support/lx200/CommandSet.html
//    https://interactiveastronomy.com/lx-200gps_telescope_protocol_2010-10.pdf
//    https://skymtn.com/mapug-astronomy/ragreiner/LX200Commands.html
//
// Copyright (c) 2025 Omair Kamil
// See LICENSE file in root directory for license terms.

use std::{
    error::Error,
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
    async fn serve_requests(&mut self) -> Result<(), Box<dyn Error + 'static>>;
}

#[derive(Default, Debug)]
struct Lx200WifiTelescope {
    controller: Lx200Controller,
}

#[async_trait]
impl Lx200Telescope for Lx200WifiTelescope {
    async fn serve_requests(&mut self) -> Result<(), Box<dyn Error + 'static>> {
        let listener = TcpListener::bind("0.0.0.0:4030").unwrap();
        info!("Running LX200 server on: {}", listener.local_addr().unwrap());

        for stream in listener.incoming() {
            let stream = stream.unwrap();
            self.handle_connection(stream).await;
        }
        Ok(())
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
    async fn serve_requests(&mut self) -> Result<(), Box<dyn Error + 'static>> {
        let session = Session::new().await?;
        let adapter = session.default_adapter().await?;
        adapter.set_powered(true).await?;

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

        let mut profile_handle = session.register_profile(profile).await?;
        info!("Running LX200 server using SPP: {}", adapter.address().await?);

        loop {
            adapter.set_discoverable(true).await?;
            let req = profile_handle.next().await;
            adapter.set_discoverable(false).await?;
            if req.is_none() {
                return Ok(());
            }
            match req.unwrap().accept() {
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

    // Animate the reported ra/dec position when it is invalid
    wiggle_phase: bool,

    // Called whenever client obtains our right ascension
    callback: Option<Callback>,

    // Pending target coordinates (in jnow_epoch) for sync/slew
    target_ra: Option<f64>,
    target_dec: Option<f64>,

    // Pending time
    // Timezone format is sHH:MM as the offset from GMT
    timezone: Option<String>,
    // Time format is HH:MM:SS
    time: Option<String>,

    // Local time, possibly provided by the client
    datetime: DateTime<FixedOffset>,

    // Current site set by the client. These values are stored here in the same
    // format provided by the client, for possible retrieval by the client.
    // Latitude format: sDD:MM where s is +/-
    latitude: String,
    // Longitude format: DDD:MM where DDD is defined as degrees west of the
    //                   Prime Meridian, from 0-359
    longitude: String,

    // Current epoch to the closest tenth of a year
    jnow_epoch: f64,
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
            callback: Some(Callback(cb)),
            datetime: dt.with_timezone(dt.offset()),
            // Ideally these should be the current location in Cedar's settings
            latitude: "+00:00".to_string(),
            longitude: "000:00".to_string(),
            jnow_epoch: jnow,
            ..Default::default()
        }
    }

    // Returns the command portion of the input string
    fn extract_command(s: &str) -> Option<String> {
        // We expect LX200 commands to be in the format ":[Command][Payload]#".
        // The command portion consists of the alphabetical string that follows
        // the colon until the first non-alphabetic character is found. The
        // payload is the remainder of the string (if any) until the hash mark.
        // Supported commands are between 1-3 letters in length.
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
            precess(ra.to_radians(), dec.to_radians(), self.jnow_epoch, 2000.0);
        (ra_rad.to_degrees(), dec_rad.to_degrees())
    }

    fn convert_to_jnow(&self, ra: f64, dec: f64) -> (f64, f64) {
        let (ra_rad, dec_rad) =
            precess(ra.to_radians(), dec.to_radians(), 2000.0, self.jnow_epoch);
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

        if !locked_position.boresight_valid {
            // Wiggle the position to indicate that it's invalid
            self.wiggle_phase = !self.wiggle_phase;
            if self.wiggle_phase {
                dec += if dec > 0.0 { -0.1 } else { 0.1 };
            }
        }

        let sign = if dec < 0.0 {
            dec = dec.abs();
            "-"
        } else {
            "+"
        };

        let (h, m, s) = Self::to_hms(dec);
        format!("{sign}{h:02}*{m:02}'{s:02}#")
    }

    fn set_target_ra(&mut self, cmd: &str) -> String {
        // The command is expected to be ":SrHH:MM:SS"
        if cmd.len() < 11 {
            warn!("Unexpected ra length, cmd: {}", cmd);
            return Self::get_failure();
        }
        let hours =
            Self::parse_coordinates(&cmd[3..5], &cmd[6..8], &cmd[9..11]);
        match hours {
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

    fn set_target_dec(&mut self, cmd: &str) -> String {
        // The command is expected to be ":SdsDD*MM:SS" where s is +/-
        if cmd.len() < 12 {
            warn!("Unexpected dec length, cmd: {}", cmd);
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
        // Check if there is a pending target set. The client will issue an Sr
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
        // Check if there is a pending target set. The client will issue an Sr
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
        info!("Stopping slew");
    }

    async fn set_latitude(&mut self, cmd: &str) -> String {
        // The command is expected to be ":StsDD:MM" where s is +/-
        if cmd.len() < 9 {
            warn!("Unexpected lat length, cmd: {}", cmd);
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
            warn!("Unexpected lon length, cmd: {}", cmd);
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
                locked_position.site_longitude = Some(longitude);
                Self::get_success()
            }
            None => Self::get_failure(),
        }
    }

    fn set_timezone(&mut self, cmd: &str) -> String {
        // The command is expected to be ":SGsXX.X" where s is +/-
        if cmd.len() < 8 {
            warn!("Unexpected tz length, cmd: {}", cmd);
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
            warn!("Unexpected time length, cmd: {}", cmd);
            return Self::get_failure();
        }
        self.time = Some(cmd[3..11].to_string());
        Self::get_success()
    }

    async fn set_date(&mut self, cmd: &str) -> String {
        // The command is expected to be ":SCMM/DD/YY"
        if cmd.len() < 11 {
            warn!("Unexpected date length, cmd: {}", cmd);
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
                    self.jnow_epoch = ((dt.year() as f64
                        + dt.ordinal0() as f64 / 365.0)
                        * 10.0)
                        .round()
                        / 10.0;
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
        let n_abs = n.abs();
        let mut hours = n_abs.trunc() as i64;
        let h_rem = n_abs.fract() * 60.0;
        let mut minutes = h_rem.trunc() as i64;
        let m_rem = h_rem.fract() * 60.0;
        let mut seconds = m_rem.round() as i64;
        if seconds == 60 {
            seconds = 0;
            minutes += 1;
            if minutes == 60 {
                minutes = 0;
                hours += 1;
            }
        }
        if n < 0.0 {
            hours = -hours;
        }
        (hours, minutes, seconds)
    }

    fn parse_coordinates(d: &str, m: &str, s: &str) -> Option<f64> {
        let degrees: Result<i32, _> = d.parse();
        let is_negative = match degrees {
            Err(e) => {
                warn!("Error parsing degrees: {}", e);
                return None;
            }
            Ok(deg) => deg < 0,
        };
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
        let deg = degrees.unwrap().abs() as f64
            + minutes.unwrap() as f64 / 60.0
            + seconds.unwrap() as f64 / 3600.00;
        Some(if is_negative { -deg } else { deg })
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
                Some("Me") | Some("Mn") | Some("Ms") | Some("Mw") => {
                    debug!("Received movement command");
                }
                Some("Q") => {
                    debug!("Received abort command");
                    self.abort().await;
                }
                Some("Qe") | Some("Qn") | Some("Qs") | Some("Qw") => {
                    debug!("Received stop movement command");
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
                    result.push_str(&self.set_target_dec(&in_data));
                }
                Some("Sg") => {
                    debug!("Received set longitude command: {}", in_data);
                    result.push_str(&self.set_longitude(&in_data).await);
                }
                Some("Sr") => {
                    debug!("Received set ra command: {}", in_data);
                    result.push_str(&self.set_target_ra(&in_data));
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
                None => {
                    // Special case for ack command not prefixed by ":"
                    if in_data == "\x06" {
                        info!("Received ack command");
                        result.push_str("A");
                        // Only Stellarium uses this command. Set the epoch to
                        // J2000 since Stellarium appears to use J2000.
                        self.jnow_epoch = 2000.0;
                    }
                }
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

#[cfg(test)]
mod tests {
    extern crate approx;

    use approx::assert_abs_diff_eq;
    use chrono::TimeZone;
    use tokio::sync::Mutex;

    use super::*;

    async fn setup_controller(
    ) -> (Lx200Controller, Arc<Mutex<TelescopePosition>>) {
        let telescope_position =
            Arc::new(Mutex::new(TelescopePosition::default()));
        let controller = Lx200Controller::new(
            telescope_position.clone(),
            Box::new(move || {}),
        );
        (controller, telescope_position)
    }

    fn assert_approx_eq(x: f64, y: f64) {
        assert_abs_diff_eq!(x, y, epsilon = 0.0001);
    }

    // --- Utility Function Tests ---

    #[test]
    fn test_extract_command() {
        assert_eq!(
            Lx200Controller::extract_command(":GR#").as_deref(),
            Some("GR")
        );
        assert_eq!(
            Lx200Controller::extract_command(":RS#:GR#").as_deref(),
            Some("RS")
        );
        assert_eq!(
            Lx200Controller::extract_command(":Sd+00*00:00#").as_deref(),
            Some("Sd")
        );
        assert_eq!(
            Lx200Controller::extract_command(":Sr00:00:00#").as_deref(),
            Some("Sr")
        );
        // Stellarium format
        assert_eq!(
            Lx200Controller::extract_command("#:GR#").as_deref(),
            Some("GR")
        );
        // Invalid cases - commands start with : and contain letters
        assert_eq!(Lx200Controller::extract_command(":123#"), None);
        assert_eq!(Lx200Controller::extract_command(":#"), None);
        assert_eq!(Lx200Controller::extract_command(":"), None);
        assert_eq!(Lx200Controller::extract_command("GR#"), None);
    }

    #[test]
    fn test_to_hms() {
        assert_eq!(Lx200Controller::to_hms(0.0), (0, 0, 0));
        assert_eq!(Lx200Controller::to_hms(1.5), (1, 30, 0));
        assert_eq!(Lx200Controller::to_hms(-10.5083), (-10, 30, 30));
        assert_eq!(Lx200Controller::to_hms(23.99972), (23, 59, 59));
        // Floating point math is hard
        assert_eq!(Lx200Controller::to_hms(10.1), (10, 6, 0));
        // Make sure we don't end up with 60 minutes or seconds
        assert_eq!(Lx200Controller::to_hms(23.999859), (23, 59, 59));
        // Checking 2 possible values due to floating point imprecision
        let v = Lx200Controller::to_hms(23.999861);
        assert!(v == (23, 59, 59) || v == (24, 0, 0), "Incorrect: {:?}", v);
        assert_eq!(Lx200Controller::to_hms(23.999862), (24, 0, 0));
        assert_eq!(Lx200Controller::to_hms(-23.999999), (-24, 0, 0));
    }

    #[test]
    fn test_parse_coordinates() {
        assert_approx_eq(
            Lx200Controller::parse_coordinates("01", "30", "00").unwrap(),
            1.5,
        );
        assert_approx_eq(
            Lx200Controller::parse_coordinates("10", "30", "30").unwrap(),
            10.5083,
        );
        assert_approx_eq(
            Lx200Controller::parse_coordinates("-15", "30", "45").unwrap(),
            -15.5125,
        );
        // Invalid
        assert_eq!(Lx200Controller::parse_coordinates("xx", "30", "00"), None);
        assert_eq!(Lx200Controller::parse_coordinates("01", "xx", "00"), None);
        assert_eq!(Lx200Controller::parse_coordinates("01", "30", "xx"), None);
    }

    #[test]
    fn test_parse_location() {
        assert_eq!(Lx200Controller::parse_location("01", "30"), Some(1.5));
        assert_approx_eq(
            Lx200Controller::parse_location("+37", "46").unwrap(),
            37.7667,
        );
        assert_approx_eq(
            Lx200Controller::parse_location("-90", "30").unwrap(),
            -90.5,
        );
        assert_eq!(Lx200Controller::parse_location("a0", "00"), None);
    }

    // --- Get RA/Dec Command Tests ---

    #[tokio::test]
    async fn test_get_ra_and_dec() {
        let (mut controller, position_arc) = setup_controller().await;
        // Set epoch to J2000 for no change
        controller.jnow_epoch = 2000.0;

        let ra_j2000 = 1.5 * 15.0;
        let dec_j2000 = 30.5;

        // Set position in a block to release the lock at the end
        {
            let mut locked_position = position_arc.lock().await;
            locked_position.boresight_ra = ra_j2000;
            locked_position.boresight_dec = dec_j2000;
            locked_position.boresight_valid = true;
        }

        let mut result_ra = controller.process_input(b":GR#").await;
        assert_eq!(result_ra.as_deref(), Some("01:30:00#"));

        // Dec snapshot should be set
        {
            let mut locked_position = position_arc.lock().await;
            assert_eq!(locked_position.snapshot_dec, Some(30.5));
            // Update the boresight dec to check the snapshot is used later
            locked_position.boresight_dec = 40.0;
        }

        let mut result_dec = controller.process_input(b":GD#").await;
        assert_eq!(result_dec.as_deref(), Some("+30*30'00#"));

        {
            let locked_position = position_arc.lock().await;
            // Snapshot should be consumed
            assert!(locked_position.snapshot_dec.is_none());
        }

        // Set epoch to J2025.9 and test with known object (M31)
        controller.jnow_epoch = 2025.9;
        // M31 is actually at (00:44:09, +41*24'40) - conversion is not exact
        let (ra_j2025_9, dec_j2025_9) = ("00:44:10#", "+41*24'39#");
        {
            let mut locked_position = position_arc.lock().await;
            locked_position.boresight_ra = 0.7123056 * 15.0;
            locked_position.boresight_dec = 41.2691667;
            locked_position.boresight_valid = true;
        }

        result_ra = controller.process_input(b":GR#").await;
        result_dec = controller.process_input(b":GD#").await;
        assert_eq!(result_ra.as_deref(), Some(ra_j2025_9));
        assert_eq!(result_dec.as_deref(), Some(dec_j2025_9));
    }

    #[tokio::test]
    async fn test_get_dec_invalid_wiggles() {
        let (mut controller, position_arc) = setup_controller().await;
        controller.jnow_epoch = 2000.0;

        {
            let mut locked_position = position_arc.lock().await;
            locked_position.boresight_dec = 10.0;
            locked_position.boresight_valid = false;
        }

        let is_wiggled = controller.wiggle_phase;
        // Also checks that boresight dec is used when there is no snapshot
        let dec1 = controller.process_input(b":GD#").await;
        assert_ne!(is_wiggled, controller.wiggle_phase);
        let dec2 = controller.process_input(b":GD#").await;

        // We don't care about the order, just that it wiggled by 0.1
        let mut results = [dec1.unwrap(), dec2.unwrap()];
        results.sort();
        assert_eq!(results, ["+09*54'00#", "+10*00'00#"]);

        // Now check that we don't wiggle when valid
        {
            let mut locked_position = position_arc.lock().await;
            locked_position.boresight_valid = true;
        }

        let dec3 = controller.process_input(b":GD#").await;
        assert_eq!(dec3.as_deref(), Some("+10*00'00#"));
        let dec4 = controller.process_input(b":GD#").await;
        assert_eq!(dec4.as_deref(), Some("+10*00'00#"));
    }

    // --- Sync/Slew Flow Tests ---

    #[tokio::test]
    async fn test_set_ra_dec_slew_abort() {
        let (mut controller, position_arc) = setup_controller().await;
        controller.jnow_epoch = 2000.0;

        let set_ra = controller.process_input(b":Sr10:30:00#").await;
        assert_eq!(set_ra.as_deref(), Some("1"));
        assert_approx_eq(controller.target_ra.unwrap(), 157.5);
        assert!(controller.target_dec.is_none());

        let set_dec = controller.process_input(b":Sd-15*30:00#").await;
        assert_eq!(set_dec.as_deref(), Some("1"));
        assert_approx_eq(controller.target_dec.unwrap(), -15.5);

        let slew_result = controller.process_input(b":MS#").await;
        // 0 is success for slew
        assert_eq!(slew_result.as_deref(), Some("0"));

        // Ensure target coordinates are consumed
        assert!(controller.target_ra.is_none());
        assert!(controller.target_dec.is_none());

        // Check telescope position state
        {
            let locked_position = position_arc.lock().await;
            assert_approx_eq(locked_position.slew_target_ra, 157.5);
            assert_approx_eq(locked_position.slew_target_dec, -15.5);
            assert!(locked_position.slew_active);
        }

        let abort = controller.process_input(b":Q#").await;
        assert_eq!(abort, None);

        {
            let locked_position = position_arc.lock().await;
            assert!(!locked_position.slew_active);
        }

        // Slew should fail if no target
        let slew_fail_result = controller.process_input(b":MS#").await;
        assert_eq!(slew_fail_result.as_deref(), Some("1No object#"));
    }

    #[tokio::test]
    async fn test_set_ra_dec_sync() {
        let (mut controller, position_arc) = setup_controller().await;
        controller.jnow_epoch = 2000.0;

        // Even with no target we should still return canned result
        let mut sync = controller.process_input(b":CM#").await;
        assert_eq!(sync.as_deref(), Some(" M31 EX GAL MAG 3.5 SZ178.0'#"));

        controller.process_input(b":Sr01:00:00#").await;
        controller.process_input(b":Sd+20*00:00#").await;

        sync = controller.process_input(b":CM#").await;
        assert_eq!(sync.as_deref(), Some(" M31 EX GAL MAG 3.5 SZ178.0'#"));

        assert!(controller.target_ra.is_none());
        assert!(controller.target_dec.is_none());

        // Check telescope position state
        let locked_position = position_arc.lock().await;
        assert_approx_eq(locked_position.sync_ra.unwrap(), 15.0);
        assert_approx_eq(locked_position.sync_dec.unwrap(), 20.0);
    }

    // --- Location and Date Tests ---
    #[tokio::test]
    async fn test_set_get_location() {
        let (mut controller, position_arc) = setup_controller().await;

        let set_lat_n = controller.process_input(b":St+37:46#").await;
        assert_eq!(set_lat_n.as_deref(), Some("1"));
        assert_eq!(controller.latitude, "+37:46");
        let lat_n = position_arc.lock().await.site_latitude.unwrap();
        assert_approx_eq(lat_n, 37.7667);

        let set_lat_s = controller.process_input(b":St-37:46#").await;
        assert_eq!(set_lat_s.as_deref(), Some("1"));
        assert_eq!(controller.latitude, "-37:46");
        let lat_s = position_arc.lock().await.site_latitude.unwrap();
        assert_approx_eq(lat_s, -37.7667);

        // Less than 180 is West for longitude
        let set_lon_w = controller.process_input(b":Sg122:25#").await;
        assert_eq!(set_lon_w.as_deref(), Some("1"));
        assert_eq!(controller.longitude, "122:25");
        let lon_w = position_arc.lock().await.site_longitude.unwrap();
        assert_approx_eq(lon_w, -122.4167);

        // Longitude values are > 180 for East
        let set_lon_e = controller.process_input(b":Sg300:00#").await;
        assert_eq!(set_lon_e.as_deref(), Some("1"));
        assert_eq!(controller.longitude, "300:00");
        let lon_e = position_arc.lock().await.site_longitude.unwrap();
        // 360 - 300 = 60.0
        assert_approx_eq(lon_e, 60.0);

        let get_lat_result = controller.process_input(b":Gt#").await;
        assert_eq!(get_lat_result.as_deref(), Some("-37:46#"));
        let get_lon_result = controller.process_input(b":Gg#").await;
        assert_eq!(get_lon_result.as_deref(), Some("300:00#"));
    }

    #[tokio::test]
    async fn test_set_get_date_time_timezone() {
        let (mut controller, position_arc) = setup_controller().await;

        let set_tz = controller.process_input(b":SG-08.0#").await;
        assert_eq!(set_tz.as_deref(), Some("1"));
        assert_eq!(controller.timezone, Some("-0800".to_string()));

        let set_time = controller.process_input(b":SL10:30:00#").await;
        assert_eq!(set_time.as_deref(), Some("1"));
        assert_eq!(controller.time, Some("10:30:00".to_string()));

        let set_date = controller.process_input(b":SC11/15/25#").await;
        assert_eq!(set_date.as_deref(), Some("1Updating Planetary Data# #"));

        // Pending time and timezone should be consumed
        assert!(controller.timezone.is_none());
        assert!(controller.time.is_none());

        // Check the controller's time
        let expected_dt = FixedOffset::west_opt(8 * 3600)
            .unwrap()
            .with_ymd_and_hms(2025, 11, 15, 10, 30, 0)
            .unwrap();
        assert_eq!(controller.datetime, expected_dt);

        // Check the TelescopePosition's time
        {
            let locked_position = position_arc.lock().await;
            assert_eq!(
                locked_position.utc_date,
                Some(SystemTime::from(expected_dt))
            );
        }

        assert_eq!(
            controller.process_input(b":GL#").await.as_deref(),
            Some("10:30:00#")
        );
        assert_eq!(
            controller.process_input(b":GC#").await.as_deref(),
            Some("11/15/25#")
        );
        assert_eq!(
            controller.process_input(b":GG#").await.as_deref(),
            Some("-08.0#")
        );
    }

    // --- Tests for Remaining Get Commands ---

    #[tokio::test]
    async fn test_canned_get_commands() {
        let (mut controller, _) = setup_controller().await;

        // Firmware date should be something like "Nov 14 2025#"
        assert_eq!(controller.process_input(b":GVD#").await.unwrap().len(), 12);
        // Firmware version should be something like "01.0#"
        assert_eq!(controller.process_input(b":GVN#").await.unwrap().len(), 5);
        // Model shouldn't change
        assert_eq!(
            controller.process_input(b":GVP#").await.as_deref(),
            Some("Cedar#")
        );
        // Firmware time should be something like "23:00:00#"
        assert_eq!(controller.process_input(b":GVT#").await.unwrap().len(), 9);
        // Status command indicates A for Alt-Az, T for tracking state, and 1
        // for 1-star aligned
        assert_eq!(
            controller.process_input(b":GW#").await.as_deref(),
            Some("AT1")
        );
        // Ack command is unterminated should result in A for Alt-Az mode
        assert_eq!(
            controller.process_input(b"\x06").await.as_deref(),
            Some("A")
        );
    }

    #[tokio::test]
    async fn test_get_distance_bars() {
        let (mut controller, position_arc) = setup_controller().await;

        {
            let mut locked_position = position_arc.lock().await;
            locked_position.slew_active = false;
        }
        assert_eq!(
            controller.process_input(b":D#").await.as_deref(),
            Some("#")
        );

        {
            let mut locked_position = position_arc.lock().await;
            locked_position.slew_active = true;
        }
        assert_eq!(
            controller.process_input(b":D#").await.as_deref(),
            Some("\x7f#")
        );
    }

    // --- Special Case Tests ---

    #[tokio::test]
    async fn test_process_input_leading_hash() {
        let (mut controller, _) = setup_controller().await;
        controller.jnow_epoch = 2000.0;

        // Stellarium sends a leading #
        let result = controller.process_input(b"#:GR#").await;
        assert_eq!(result.as_deref(), Some("00:00:00#"));
    }

    #[tokio::test]
    async fn test_process_input_multiple_commands() {
        let (mut controller, position_arc) = setup_controller().await;
        controller.jnow_epoch = 2000.0;

        {
            let mut locked_position = position_arc.lock().await;
            locked_position.boresight_valid = true;
        }

        // SkySafari in Bluetooth mode sets slew rate and gets RA together
        let result = controller.process_input(b":RS#:GR#").await;
        assert_eq!(result.as_deref(), Some("00:00:00#"));

        // Make sure 2 commands that need responses get a concatenated response
        // although no client does this
        let result2 = controller.process_input(b":GR#:GD#").await;
        assert_eq!(result2.as_deref(), Some("00:00:00#+00*00'00#"));
    }
}
