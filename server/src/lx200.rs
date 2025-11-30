// Copyright (c) 2025 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

// bring in the reference to the telescope poisition structure
use crate::position_reporter::TelescopePosition;

use std::sync::Arc;
use log::info;

// lx200 code
//use tokio::time::{sleep, Duration};
use tokio::net::TcpListener;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
// lx200 code

// lx200 functions here ------------------------------------------------------
//Create lx200 server
pub async fn lx200_create(lx200_server_addr: &str) -> tokio::net::TcpListener {
    // async bind
    let listener = TcpListener::bind(&lx200_server_addr).await;
    info!("LX200 server listenening on: {}", &lx200_server_addr);
    listener.unwrap()
}

//LX200 server task
//takes clone of TelescopePosition to grab RA/Dec
pub async fn lx200_start(lx200_listener: tokio::net::TcpListener, cedar_pos: &Arc<tokio::sync::Mutex<TelescopePosition>>) {
    loop {
        let (mut stream, addr) = lx200_listener.accept().await.unwrap();  // async accept
        println!("New lx200 client: {}", addr);
        //let pos_locked = tele_pos.lock().await;
        //println!("Tele_Pos: {:?}", &pos_locked);

        let cedar_pos_clone = Arc::clone(&cedar_pos);

        tokio::spawn(async move { // spawn async task for each lx200 client
            let mut buf = [0; 30]; //buffer for tcp stream might need to be bigger at some point
            loop {
                let n= match stream.read(&mut buf).await { // async read the tcp buffer
                    Ok(n) if n == 0 => return, // connection is closed
                    Ok(n) => n,
                    Err(e) => {
                        eprintln!("ERROR: failed to read LX200 server socket; err = {:?}", e);
                        return;
                    }
                };

                let pos_locked = cedar_pos_clone.lock().await;

                match &buf[0..(n)] {
                    [35,06] => { // LX200 alignment ack received
                        let _ = stream.write_all(b"A").await;
                    }

                    b"#:GW#" => { // LX200 scope status
                                  // 1st char 1-AltAlz mount, P-Polar mount, G-GEM
                                  // 2nd char T-Tracking, N-Not Tracking, S-Sleeping
                                  // 3rd char 0,1,2,3,H Alignment type, P-Parked
                        let _ = stream.write_all(b"ANH#").await;
                    }

                    b"#:GR#" => { // LX200 get telescope RA
                        // here we grab the position from Cedar
                        //
                        let cedar_ra: f64 = pos_locked.boresight_ra;
                        //
                        //let cedar_ra: f64 = 3.08333;    // RA for Polaris - test value

                        //Convert floating point RA to HH:MM:SS
                        let ra_normalized = cedar_ra / 15.0;
                        //Extract hours, minutes, seconds
                        let ra_hour = ra_normalized.trunc();
                        let ra_min_dec = (ra_normalized - ra_hour) * 60.0;
                        let ra_min = ra_min_dec.trunc();
                        let ra_sec_dec = (ra_min_dec - ra_min) * 60.0;
                        let ra_sec = ra_sec_dec.trunc();
                        // convert to string value
                        let ra_string = format!("{:02}:{:02}:{:02}#", ra_hour, ra_min, ra_sec);
                        //println!("RA value: {}", cedar_ra);
                        //println!("RA string: {}", ra_string);
                        let _ = stream.write_all(&ra_string.into_bytes()).await;
                    }

                    b"#:GD#" => { //LX200 get telescope dec
                        // grab cedar position
                        let dec_dec: f64 = pos_locked.boresight_dec;
                        //
                        //let dec_dec: f64 = 89.372917;    //polaris test position

                        //Convert to +/- deg min sec
                        let sign = if dec_dec.is_sign_negative() { "-" } else { "+" };
                        let dec_abs = dec_dec.abs();
                        let degrees = dec_abs.trunc();
                        let tot_min = (dec_abs - degrees) * 60.0;
                        let minutes = tot_min.trunc();
                        let tot_sec = (tot_min - minutes) * 60.0;
                        let seconds = tot_sec.trunc();
                        let dec_string = format!("{}{:02}*{:02}'{:02}#", sign, degrees, minutes, seconds);
                        //println!("DEC string: {}", dec_string);
                        let _ = stream.write_all(&dec_string.into_bytes()).await;
                    }

                    b"#:GVP#" => { //LX200 get telescope product name
                        let _ = stream.write_all(b"Cedar#").await;
                    }

                    b"#:GVN#" => { //LX200 get telescope firmware version
                        let _ = stream.write_all(b"0.9.3").await;   //should grab this from cedar
                    }

                    b"#:GVT#" => { //LX200 get telescope firmware time
                        let _ = stream.write_all(b"12:00:00#").await;
                    }

                    b"#:GVD#" => { //LX200 get telescope firmware date
                        let _ = stream.write_all(b"Nov 05 2025").await;  //grab this ???
                    }

                    b"#:Gg#" => { //LX200 get location longitude
                        //ignore for now
                    }

                    b"#:D#" => { //LX200 distance bars - ignore
                    }

                    _ if buf.starts_with(b"#:St") => { //LX200 current latitude from Stellarium
                        // need to add code to obtain and set cedar
                        let _ = stream.write_all(b"1#").await;
                    }

                    _ if buf.starts_with(b"#:sg") => { //LX200 current longitude
                        // need to add code
                    }

                    _ if buf.starts_with(b"#:SG") => { //LX200 current UTC offset
                        //silently drop
                    }

                    _ if buf.starts_with(b"##") => { // unknown command silently drop
                    }

                    _ => { // catch everything else print and drop
                        println!("Other data received: {:?}", str::from_utf8(&buf[0..(n)]));
                        println!("Full buffer: {:?}", &buf);
                    }
                }
            }
        });
    }
}

//LX200 server code end -----------------------------------------------------

