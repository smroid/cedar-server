// Copyright (c) 2025 Omair Kamil
// See LICENSE file in root directory for license terms.

use std::net::{TcpListener, TcpStream};
use std::io::{Read, Write};

use std::sync::Arc;

use log::{debug, info, warn};

use crate::{
    position_reporter::{Callback, TelescopePosition},
};

#[derive(Default, Debug)]
pub struct Lx200Telescope {
    telescope_position: Arc<tokio::sync::Mutex<TelescopePosition>>,
    // Called whenever SkySafari obtains our right ascension
    callback: Option<Callback>,
    // Pending target coordinates for sync/slew
    target_ra: Option<f64>,
    target_dec: Option<f64>,
}

impl Lx200Telescope {
    pub fn new(telescope_position: Arc<tokio::sync::Mutex<TelescopePosition>>,
               cb: Box<dyn Fn() + Send + Sync>) -> Self {
        Lx200Telescope{ telescope_position,
                        callback: Some(Callback(cb)),
                        target_ra: None,
                        target_dec: None, }
    }

    fn extract_command(s: &str) -> Option<String> {
        // We expect LX200 commands to be in the format ":[Command][Payload]#". The command is
        // is composed of one or two letters. Commands to set RA or Declination have a payload
        // consisting of coordinates in HH:MM:SS format, with Dec including a leading sign.
        let colon_index = s.find(':')?; 
        if colon_index + 1 >= s.len() {
            return None;
        }

        let command: String = s[colon_index+1..].chars().take_while(|c| c.is_alphabetic()).collect();
        if command.is_empty() {
            None
        } else {
            Some(command)
        }
    }

    fn write(mut stream: &TcpStream, s: &str) {
        debug!("Writing to SkySafari: {}", s);
        if let Err(e) = stream.write_all(s.as_bytes()) {
            warn!("Failed to send data to client: {}", e);
        }
    }

    async fn handle_get_ra(&self, stream: &TcpStream) {
        if let Some(ref cb) = self.callback {
            cb.0();
        }
        let mut locked_position = self.telescope_position.lock().await;
        // Keep a snapshot of the current dec for the next retrieval
        locked_position.snapshot_dec = Some(locked_position.boresight_dec);
        // RA degrees need to be converted to hours before the conversion to HH:MM:SS
        Self::write(stream, Self::to_coordinates("", locked_position.boresight_ra / 15.0).as_str());
    }

    async fn handle_get_dec(&self, stream: &TcpStream) {
        let mut locked_position = self.telescope_position.lock().await;
        let snapshot_dec = locked_position.snapshot_dec.take();

        let mut dec = if snapshot_dec.is_some() {
            snapshot_dec.unwrap()
        } else {
            locked_position.boresight_dec
        };

        let sign = if dec < 0.0 {
            dec = dec.abs();
            "-"
        } else {
            "+"
        };
        Self::write(stream, Self::to_coordinates(sign, dec).as_str());
    }

    async fn handle_set_ra(&mut self, stream: &TcpStream, cmd: &str) {
        info!("Set ra command: {}", cmd);
        // The command is expected to be ":SrHH:MM:SS#"
        if cmd.len() < 12 {
            warn!("Unexpected ra length");
            // 0 indicates failure
            Self::write(&stream, "0");
            return;
        }
        let degrees = Self::parse_coordinates(&cmd[3..5], &cmd[6..8], &cmd[9..11]);
        match degrees {
            Some(n) => {
                info!("Set target ra to {}", n * 15.0);
                // Convert hours to 360-degree format
                self.target_ra = Some(n * 15.0);
                Self::write(&stream, "1");
            }
            None => {
                // 0 indicates failure
                Self::write(&stream, "0");
            }
        }
    }

    async fn handle_set_dec(&mut self, stream: &TcpStream, cmd: &str) {
        info!("Set dec command: {}", cmd);
        // The command is expected to be ":Sd+HH:MM:SS#" or ":Sd-HH:MM:SS#"
        if cmd.len() < 13 {
            warn!("Unexpected dec length");
            // 0 indicates failure
            Self::write(&stream, "0");
            return;
        }
        let degrees = Self::parse_coordinates(&cmd[3..6], &cmd[7..9], &cmd[10..12]);
        match degrees {
            Some(n) => {
                info!("Set target dec to {}", n);
                self.target_dec = Some(n);
                Self::write(&stream, "1");
            }
            None => {
                // 0 indicates failure
                Self::write(&stream, "0");
            }
        }
    }

    async fn handle_slew(&mut self) {
        // Check if there is a pending position set. SkySafari will issue an Sr command, followed
        // by an Sd command, followed by MS to initiate the movement. This applies to GoTo mode.
        // SkySafari also issues slew commands upon initial connection which should be ignored.
        if let (Some(ra), Some(dec)) = (self.target_ra.take(), self.target_dec.take()) {
            let mut locked_position = self.telescope_position.lock().await;
            locked_position.slew_target_ra = ra;
            locked_position.slew_target_dec = dec;
            locked_position.slew_active = true;
        }
    }

    async fn handle_sync(&mut self) {
        // Check if there is a pending position set. SkySafari will issue an Sr command, followed
        // by an Sd command, followed by CM to sync/align. This applies to both GoTo and PushTo
        // modes.
        if let (Some(ra), Some(dec)) = (self.target_ra.take(), self.target_dec.take()) {
            let mut locked_position = self.telescope_position.lock().await;
            locked_position.sync_ra = Some(ra);
            locked_position.sync_dec = Some(dec);
        }
    }

    async fn handle_abort(&mut self) {
        let mut locked_position = self.telescope_position.lock().await;
        locked_position.slew_active = false;
    }

    fn to_coordinates(prefix: &str, n: f64) -> String {
        let hours = n.trunc() as i64;
    
        let minutes_float = (n - hours as f64) * 60.0;
        let minutes = minutes_float.trunc() as i64;

        let seconds = ((minutes_float - minutes as f64) * 60.0).trunc() as i64;

        format!("{prefix}{hours:02}:{minutes:02}:{seconds:02}#")
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
        Some(hours.unwrap() as f64 + minutes.unwrap() as f64 / 60.0 + seconds.unwrap() as f64 / 3600.0)
    }

    pub async fn start(&mut self) {
        let listener = TcpListener::bind("0.0.0.0:4030").unwrap();
        info!("Running LX200 server on: {}", listener.local_addr().unwrap());

        for stream in listener.incoming() {
            let stream = stream.unwrap();
            self.handle_connection(stream).await;
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
                    let in_data = String::from_utf8_lossy(&buffer[..n]);

                    match Self::extract_command(&in_data).as_deref() {
                        Some("CM") => {
                            info!("Received sync command");
                            self.handle_sync().await;
                            // AutoStar always responds with this, so we'll do the same
                            Self::write(&stream, " M31 EX GAL MAG 3.5 SZ178.0'#");
                        }
                        Some("GD") => {
                            debug!("Received get declination command");
                            self.handle_get_dec(&stream).await;
                        }
                        Some("GR") => {
                            debug!("Received get ra command");
                            self.handle_get_ra(&stream).await;
                        }
                        Some("RS") => {
                            info!("Received slew command");
                            self.handle_slew().await;
                        }
                        Some("MS") => {
                            info!("Received slew to object command");
                            Self::write(&stream, "0");
                        }
                        Some("Sd") => {
                            info!("Received set declination command");
                            self.handle_set_dec(&stream, &in_data).await;
                        }
                        Some("Sr") => {
                            info!("Received set ra command");
                            self.handle_set_ra(&stream, &in_data).await;
                        }
                        Some("Q") => {
                            info!("Received abort command");
                            self.handle_abort().await
                        }
                        Some(command) => {
                            info!("Unknown command: {}", command);
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

pub fn create_lx200_server(telescope_position: Arc<tokio::sync::Mutex<TelescopePosition>>,
                           cb: Box<dyn Fn() + Send + Sync>) -> Lx200Telescope {
    Lx200Telescope::new(telescope_position, cb)
}
