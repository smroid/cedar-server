// Copyright (c) 2024 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use crate::detect_engine::{DetectEngine, DetectResult};

use std::cmp::max;
use std::ops::DerefMut;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use canonical_error::{CanonicalError, failed_precondition_error, invalid_argument_error};
use chrono::{DateTime, Local, Utc};
use image::{GenericImageView, GrayImage};
use imageproc::rect::Rect;
use log::{debug, error};
use tonic::transport::{Endpoint, Uri};
use tokio::net::UnixStream;
use tower::service_fn;

use crate::tetra3_server::{CelestialCoord, ImageCoord, SolveRequest,
                           SolveResult as SolveResultProto,
                           SolveStatus};
use crate::tetra3_server::tetra3_client::Tetra3Client;
use crate::tetra3_subprocess::Tetra3Subprocess;
use crate::value_stats::ValueStatsAccumulator;
use crate::cedar;
use cedar_detect::histogram_funcs::{average_top_values,
                                    get_level_for_fraction,
                                    remove_stars_from_histogram};
use crate::scale_image::scale_image_mut;
use crate::astro_util::{angular_separation, position_angle};

pub struct SolveEngine {
    tetra3_subprocess: Arc<Mutex<Tetra3Subprocess>>,

    // Our connection to the tetra3 gRPC server.
    client: Arc<tokio::sync::Mutex<Tetra3Client<tonic::transport::Channel>>>,

    // Our state, shared between SolveEngine methods and the worker thread.
    state: Arc<Mutex<SolveState>>,

    // Detect engine settings can be adjusted behind our back.
    detect_engine: Arc<tokio::sync::Mutex<DetectEngine>>,

    // Executes worker().
    worker_thread: Option<tokio::task::JoinHandle<()>>,

    // Called whenever worker() finishes an evaluation. Return value is sky coordinate
    // of slew target, if any.
    solution_callback: Arc<dyn Fn(Option<DetectResult>,
                                  Option<SolveResultProto>)
                                  -> Option<CelestialCoord> + Send + Sync>,
}

// State shared between worker thread and the SolveEngine methods.
struct SolveState {
    frame_id: Option<i32>,

    // Zero means go fast as star detections are computed.
    update_interval: Duration,

    // Required number of detected stars, below which we don't attempt a plate
    // solution.
    minimum_stars: i32,

    // Parameters for plate solver. See documentation of Tetra3's
    // solve_from_centroids() function for a description of these items.
    fov_estimate: Option<f32>,
    match_radius: f32,
    match_threshold: f32,
    solve_timeout: Duration,
    boresight_pixel: Option<ImageCoord>,
    distortion: f32,
    match_max_error: f32,
    return_matches: bool,

    // Set if currently slewing to a target.
    slew_target: Option<CelestialCoord>,

    solve_interval_stats: ValueStatsAccumulator,
    solve_latency_stats: ValueStatsAccumulator,
    solve_attempt_stats: ValueStatsAccumulator,
    solve_success_stats: ValueStatsAccumulator,

    // Estimated time at which `plate_solution` will next be updated.
    eta: Option<Instant>,

    plate_solution: Option<PlateSolution>,

    // Set by stop(); the worker thread exits when it sees this.
    stop_request: bool,
}

impl Drop for SolveEngine {
    fn drop(&mut self) {
        // https://stackoverflow.com/questions/71541765/rust-async-drop
        futures::executor::block_on(self.stop());
    }
}

impl SolveEngine {
    async fn connect(tetra3_server_address: String)
                     -> Result<Tetra3Client<tonic::transport::Channel>, CanonicalError> {
        // Set up gRPC client, connect to a UDS socket. URL is ignored.
        let mut backoff = Duration::from_millis(100);
        loop {
            let addr = tetra3_server_address.clone();
            let channel = Endpoint::try_from("http://[::]:50051").unwrap()
                .connect_with_connector(service_fn(move |_: Uri| {
                    UnixStream::connect(addr.clone())
                })).await;
            match channel {
                Ok(ch) => {
                    return Ok(Tetra3Client::new(ch));
                },
                Err(e) => {
                    if backoff > Duration::from_secs(20) {
                        return Err(failed_precondition_error(
                            format!("Error connecting to Tetra server at {:?}: {:?}",
                                    tetra3_server_address, e).as_str()));
                    }
                    // Give time for tetra3_server binary to start up, load its
                    // pattern database, and start to accept connections.
                    tokio::time::sleep(backoff).await;
                    backoff = backoff.mul_f32(1.5);
                }
            }
        }
    }

    pub async fn new(tetra3_subprocess: Arc<Mutex<Tetra3Subprocess>>,
                     detect_engine: Arc<tokio::sync::Mutex<DetectEngine>>,
                     tetra3_server_address: String,
                     update_interval: Duration,
                     stats_capacity: usize,
                     solution_callback: Arc<dyn Fn(Option<DetectResult>,
                                                   Option<SolveResultProto>)
                                                   -> Option<CelestialCoord> + Send + Sync>)
                     -> Result<Self, CanonicalError> {
        let client = Self::connect(tetra3_server_address).await?;
        Ok(SolveEngine{
            tetra3_subprocess,
            client: Arc::new(tokio::sync::Mutex::new(client)),
            state: Arc::new(Mutex::new(SolveState{
                frame_id: None,
                update_interval,
                minimum_stars: 4,
                fov_estimate: None,
                match_radius: 0.01,
                match_threshold: 0.0001,  // TODO: pass in from cmdline arg.
                solve_timeout: Duration::from_secs(1),
                boresight_pixel: None,
                distortion: 0.0,
                match_max_error: 0.005,
                return_matches: true,
                slew_target: None,
                solve_interval_stats: ValueStatsAccumulator::new(stats_capacity),
                solve_latency_stats: ValueStatsAccumulator::new(stats_capacity),
                solve_attempt_stats: ValueStatsAccumulator::new(stats_capacity),
                solve_success_stats: ValueStatsAccumulator::new(stats_capacity),
                eta: None,
                plate_solution: None,
                stop_request: false,
            })),
            detect_engine,
            worker_thread: None,
            solution_callback,
        })
    }

    // Determines how often the detect engine operates (obtains a DetectResult,
    // produces a PlateSolution).
    // An interval of zero means run continuously-- as soon as a PlateSolution
    // is produced, the next one is started.
    pub fn set_update_interval(&mut self, update_interval: Duration)
                               -> Result<(), CanonicalError> {
        let mut locked_state = self.state.lock().unwrap();
        locked_state.update_interval = update_interval;
        // Don't need to do anything, worker thread will pick up the change when
        // it finishes the current interval.
        Ok(())
    }

    pub fn set_fov_estimate(&mut self, fov_estimate: Option<f32>)
                            -> Result<(), CanonicalError> {
        let mut locked_state = self.state.lock().unwrap();
        if fov_estimate.is_some() && fov_estimate.unwrap() <= 0.0 {
            return Err(invalid_argument_error(
                format!("fov_estimate must be positive; got {}",
                        fov_estimate.unwrap()).as_str()));
        }
        locked_state.fov_estimate = fov_estimate;
        // Don't need to do anything, worker thread will pick up the change when
        // it finishes the current interval.
        Ok(())
    }

    pub fn set_boresight_pixel(&mut self, boresight_pixel: Option<ImageCoord>)
                               -> Result<(), CanonicalError> {
        let mut locked_state = self.state.lock().unwrap();
        locked_state.boresight_pixel = boresight_pixel;
        // Don't need to do anything, worker thread will pick up the change when
        // it finishes the current interval.
        Ok(())
    }
    pub fn boresight_pixel(&self) -> Result<Option<ImageCoord>, CanonicalError> {
        let locked_state = self.state.lock().unwrap();
        Ok(locked_state.boresight_pixel.clone())
    }

    pub fn set_distortion(&mut self, distortion: f32)
                               -> Result<(), CanonicalError> {
        if distortion < -0.2 || distortion > 0.2 {
            return Err(invalid_argument_error(
                format!("distortion must be in [-0.2, 0.2]; got {}",
                        distortion).as_str()));
        }
        let mut locked_state = self.state.lock().unwrap();
        locked_state.distortion = distortion;
        // Don't need to do anything, worker thread will pick up the change when
        // it finishes the current interval.
        Ok(())
    }

    pub fn set_match_max_error(&mut self, match_max_error: f32)
                               -> Result<(), CanonicalError> {
        if match_max_error < 0.0 {
            return Err(invalid_argument_error(
                format!("match_max_error must be non-negative; got {}",
                        match_max_error).as_str()));
        }
        let mut locked_state = self.state.lock().unwrap();
        locked_state.match_max_error = match_max_error;
        // Don't need to do anything, worker thread will pick up the change when
        // it finishes the current interval.
        Ok(())
    }

    pub fn set_minimum_stars(&mut self, minimum_stars: i32)
                             -> Result<(), CanonicalError> {
        if minimum_stars < 4 {
            return Err(invalid_argument_error(
                format!("minimum_stars must be at least 4; got {}",
                        minimum_stars).as_str()));
        }
        let mut locked_state = self.state.lock().unwrap();
        locked_state.minimum_stars = minimum_stars;
        // Don't need to do anything, worker thread will pick up the change when
        // it finishes the current interval.
        Ok(())
    }

    pub fn set_solve_timeout(&mut self, solve_timeout: Duration)
                             -> Result<(), CanonicalError> {
        let mut locked_state = self.state.lock().unwrap();
        locked_state.solve_timeout = solve_timeout;
        // Don't need to do anything, worker thread will pick up the change when
        // it finishes the current interval.
        Ok(())
    }

    // Note: we don't currently provide methods to change match_radius,
    // match_threshold, or return_matches. The defaults for these should be
    // fine.

    /// Obtains a result bundle, as configured above. The returned result is
    /// "fresh" in that we either wait to solve a new detect result or return
    /// the result of solving the most recently completed star detection.
    /// This function does not "consume" the information that it returns;
    /// multiple callers will receive the current solve result (or next solve
    /// result, if there is not yet a current result) if `prev_frame_id` is
    /// omitted.
    /// If `prev_frame_id` is supplied, the call blocks while the current result
    /// has the same id value.
    /// Returns: the processed result along with its frame_id value.
    pub async fn get_next_result(&mut self, prev_frame_id: Option<i32>) -> PlateSolution {
        // Start worker thread if terminated or not yet started.
        self.start().await;

        // Get the most recently posted result; wait if there is none yet or the
        // currently posted result is the same as the one the caller has already
        // obtained.
        loop {
            let mut sleep_duration = Duration::from_millis(1);
            {
                let locked_state = self.state.lock().unwrap();
                if locked_state.plate_solution.is_some() &&
                    (prev_frame_id.is_none() ||
                     prev_frame_id.unwrap() !=
                     locked_state.plate_solution.as_ref().unwrap().detect_result.frame_id)
                {
                    // Don't consume it, other clients may want it.
                    return locked_state.plate_solution.clone().unwrap();
                }
                if let Some(eta) = locked_state.eta {
                    let time_to_eta = eta.saturating_duration_since(Instant::now());
                    if time_to_eta > sleep_duration {
                        sleep_duration = time_to_eta;
                    }
                }
            }
            tokio::time::sleep(sleep_duration).await;
        }
    }

    pub fn reset_session_stats(&mut self) {
        let mut state = self.state.lock().unwrap();
        state.solve_interval_stats.reset_session();
        state.solve_latency_stats.reset_session();
        state.solve_attempt_stats.reset_session();
        state.solve_success_stats.reset_session();
    }

    // TODO: arg specifying directory to save to.
    pub async fn save_image(&self) -> Result<(), CanonicalError> {
        // Grab most recent image.
        let mut locked_detect_engine = self.detect_engine.lock().await;
        let captured_image =
            &locked_detect_engine.get_next_result(/*frame_id=*/None).await.captured_image;
        let image: &GrayImage = &captured_image.image;
        let readout_time: &SystemTime = &captured_image.readout_time;
        let exposure_duration_ms =
            captured_image.capture_params.exposure_duration.as_millis();

        let seconds_since_epoch =
            readout_time.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
        let datetime_utc: DateTime<Utc> =
            DateTime::from_timestamp(seconds_since_epoch as i64, 0).unwrap();
        let datetime_local: DateTime<Local> = DateTime::from(datetime_utc);

        // Generate file name.
        let filename = format!("img_{}ms_{}.bmp",
                               exposure_duration_ms, datetime_local.format("%Y%m%d_%H%M%S"));
        // Write to current directory.
        match image.save(filename) {
            Ok(()) => Ok(()),
            Err(x) => {
            return Err(failed_precondition_error(
                format!("Error saving file: {:?}", x).as_str()));
            }
        }
    }

    pub async fn solve(&self, solve_request: SolveRequest)
             -> Result<SolveResultProto, CanonicalError> {
        Self::solve_with_client(self.client.clone(), solve_request).await
    }

    async fn solve_with_client(
        client: Arc<tokio::sync::Mutex<Tetra3Client<tonic::transport::Channel>>>,
        solve_request: SolveRequest)
        -> Result<SolveResultProto, CanonicalError> {
        match client.lock().await.solve_from_centroids(solve_request).await {
            Ok(response) => {
                return Ok(response.into_inner());
            },
            Err(e) => {
                return Err(failed_precondition_error(
                    format!("Error invoking plate solver: {:?}", e).as_str()));
            },
        }
    }

    pub async fn start(&mut self) {
        // Has the worker terminated for some reason?
        if self.worker_thread.is_some() &&
            self.worker_thread.as_ref().unwrap().is_finished()
        {
            self.worker_thread.take().unwrap().await.unwrap();
        }
        if self.worker_thread.is_none() {
            let cloned_client = self.client.clone();
            let cloned_state = self.state.clone();
            let cloned_detect_engine = self.detect_engine.clone();
            let cloned_callback = self.solution_callback.clone();
            self.worker_thread = Some(tokio::task::spawn(async move {
                SolveEngine::worker(cloned_client, cloned_state,
                                    cloned_detect_engine, cloned_callback).await;
            }));
        }
    }

    /// Shuts down the worker thread; this can save power if get_next_result()
    /// will not be called soon. A subsequent call to get_next_result() will
    /// re-start processing, at the expense of that first get_next_result() call
    /// taking longer than usual.
    pub async fn stop(&mut self) {
        if self.worker_thread.is_some() {
            self.tetra3_subprocess.lock().unwrap().send_interrupt_signal();
            self.state.lock().unwrap().stop_request = true;
            self.worker_thread.take().unwrap().await.unwrap();
        }
    }

    async fn worker(
        client: Arc<tokio::sync::Mutex<Tetra3Client<tonic::transport::Channel>>>,
        state: Arc<Mutex<SolveState>>,
        detect_engine: Arc<tokio::sync::Mutex<DetectEngine>>,
        solution_callback: Arc<dyn Fn(Option<DetectResult>,
                                      Option<SolveResultProto>)
                                      -> Option<CelestialCoord> + Send + Sync>) {
        debug!("Starting solve engine");
        // Keep track of when we started the solve cycle.
        let mut last_result_time: Option<Instant> = None;
        loop {
            let update_interval: Duration;
            {
                let mut locked_state = state.lock().unwrap();
                update_interval = locked_state.update_interval;
                if locked_state.stop_request {
                    debug!("Stopping solve engine");
                    locked_state.stop_request = false;
                    solution_callback(None, None);
                    return;  // Exit thread.
                }
            }
            // Is it time to generate the next PlateSolution?
            let now = Instant::now();
            if let Some(lrt) = last_result_time {
                let next_update_time = lrt + update_interval;
                if next_update_time > now {
                    let delay = next_update_time - now;
                    state.lock().unwrap().eta = Some(Instant::now() + delay);
                    tokio::time::sleep(delay).await;
                    continue;
                }
                state.lock().unwrap().eta = None;
            }

            // Time to do a solve processing cycle.
            if let Some(lrt) = last_result_time {
                let elapsed = lrt.elapsed();
                let mut locked_state = state.lock().unwrap();
                locked_state.solve_interval_stats.add_value(elapsed.as_secs_f64());
            }
            last_result_time = Some(now);

            let detect_result: DetectResult;
            let mut solve_request = SolveRequest::default();
            let minimum_stars;
            let frame_id;
            let mut slew_request = None;
            let mut boresight_image: Option<GrayImage> = None;
            let mut boresight_image_region: Option<Rect> = None;
            {
                let locked_state = state.lock().unwrap();
                minimum_stars = locked_state.minimum_stars;

                // Set up SolveRequest.
                solve_request.fov_estimate = locked_state.fov_estimate;
                match locked_state.fov_estimate {
                    Some(fov) => {
                        solve_request.fov_max_error = Some(fov / 10.0);
                        solve_request.match_max_error = None;
                    }
                    None => {
                        solve_request.fov_max_error = None;
                        solve_request.match_max_error = Some(0.005);
                    }
                };

                solve_request.match_radius = Some(locked_state.match_radius);
                solve_request.match_threshold = Some(locked_state.match_threshold);

                let solve_timeout = locked_state.solve_timeout.as_secs_f64();
                let solve_timeout_int = solve_timeout as i64;
                let solve_timeout_frac = solve_timeout - solve_timeout_int as f64;
                solve_request.solve_timeout = Some(prost_types::Duration {
                    seconds: solve_timeout_int,
                    nanos: (solve_timeout_frac * 1000000000.0) as i32,
                });

                if let Some(boresight_pixel) = &locked_state.boresight_pixel {
                    solve_request.target_pixels.push(boresight_pixel.clone());
                }
                if let Some(slew_target) = &locked_state.slew_target {
                    slew_request = Some(cedar::SlewRequest{
                        target: Some(slew_target.clone()), ..Default::default()});
                    solve_request.target_sky_coords.push(slew_target.clone());
                }
                solve_request.distortion = Some(locked_state.distortion);
                solve_request.match_max_error = Some(locked_state.match_max_error);
                solve_request.return_matches = locked_state.return_matches;
                frame_id = locked_state.frame_id;
            }
            // Get the most recent star detection result.
            if let Some(delay_est) = detect_engine.lock().await.estimate_delay(frame_id) {
                state.lock().unwrap().eta = Some(Instant::now() + delay_est);
            }
            detect_result = detect_engine.lock().await.get_next_result(frame_id).await;
            state.lock().unwrap().deref_mut().frame_id = Some(detect_result.frame_id);

            let image: &GrayImage = &detect_result.captured_image.image;
            let (width, height) = image.dimensions();

            // Plate-solve using the recently detected stars.
            let process_start_time = Instant::now();

            for sc in &detect_result.star_candidates {
                solve_request.star_centroids.push(ImageCoord{x: sc.centroid_x,
                                                             y: sc.centroid_y});
            }
            solve_request.image_width = width as i32;
            solve_request.image_height = height as i32;

            let mut tetra3_solve_result: Option<SolveResultProto> = None;
            let mut solve_finish_time: Option<SystemTime> = None;
            if detect_result.star_candidates.len() >= minimum_stars as usize {
                {
                    let mut locked_state = state.lock().unwrap();
                    if let Some(recent_stats) =
                        &locked_state.solve_latency_stats.value_stats.recent
                    {
                        let solve_duration = Duration::from_secs_f64(recent_stats.min);
                        locked_state.eta = Some(Instant::now() + solve_duration);
                    }
                }
                match Self::solve_with_client(client.clone(), solve_request).await {
                    Err(e) => {
                        error!("Unexpected error {:?}", e);
                        return;  // Abandon thread execution!
                    },
                    Ok(response) => {
                        tetra3_solve_result = Some(response);
                    }
                }
                solve_finish_time = Some(SystemTime::now());
            }

            let elapsed = process_start_time.elapsed();
            let mut locked_state = state.lock().unwrap();
            if tetra3_solve_result.is_none() {
                locked_state.solve_attempt_stats.add_value(0.0);
                solution_callback(Some(detect_result.clone()), None);
            } else {
                locked_state.solve_attempt_stats.add_value(1.0);
                let tsr = tetra3_solve_result.as_ref().unwrap();
                if tsr.status.unwrap() == SolveStatus::MatchFound as i32 {
                    locked_state.solve_success_stats.add_value(1.0);
                    // Let integration layer pass solution to SkySafari telescope
                    // interface and MotionEstimator. Integration layer returns current
                    // slew target, if any.
                    locked_state.slew_target =
                        solution_callback(Some(detect_result.clone()), Some(tsr.clone()));

                    if let Some(ref mut slew_req) = slew_request {
                        let coords;
                        if tsr.target_coords.len() > 0 {
                            coords = tsr.target_coords[0].clone();
                        } else {
                            coords = tsr.image_center_coords.as_ref().unwrap().clone();
                        }
                        let bs_ra = coords.ra.to_radians() as f64;
                        let bs_dec = coords.dec.to_radians() as f64;
                        let st_ra =
                            slew_req.target.as_ref().unwrap().ra.to_radians() as f64;
                        let st_dec =
                            slew_req.target.as_ref().unwrap().dec.to_radians() as f64;
                        slew_req.target_distance = Some(angular_separation(
                            bs_ra, bs_dec, st_ra, st_dec).to_degrees() as f32);

                        let mut angle = (position_angle(
                            bs_ra, bs_dec, st_ra, st_dec).to_degrees() as f32 +
                                         tsr.roll.unwrap()) % 360.0;
                        // Arrange for angle to be 0..360.
                        if angle < 0.0 {
                            angle += 360.0;
                        }
                        slew_req.target_angle = Some(angle);

                        if tsr.target_sky_to_image_coords.len() > 0 {
                            let img_coord = &tsr.target_sky_to_image_coords[0];
                            if img_coord.x >= 0.0 {
                                let target_image_coord =
                                    cedar::ImageCoord{x: img_coord.x, y: img_coord.y};
                                slew_req.image_pos = Some(target_image_coord.clone());
                                if img_coord.x > detect_result.center_region.left() as f32 &&
                                    img_coord.x < detect_result.center_region.right() as f32 &&
                                    img_coord.y > detect_result.center_region.top() as f32 &&
                                    img_coord.y < detect_result.center_region.bottom() as f32
                                {
                                    slew_req.target_within_center_region = true;
                                }
                                // Is the target's image_pos close to the boresight's
                                // image position?
                                let boresight_pos;
                                if let Some(bp) = &locked_state.boresight_pixel {
                                    boresight_pos = bp.clone();
                                } else {
                                    boresight_pos = ImageCoord{
                                        x: width as f32 / 2.0, y: height as f32 / 2.0};
                                }
                                let target_close_threshold =
                                    std::cmp::min(width, height) as f32 / 16.0;
	                        let target_boresight_distance =
                                    ((target_image_coord.x - boresight_pos.x) *
                                     (target_image_coord.x - boresight_pos.x) +
                                     (target_image_coord.y - boresight_pos.y) *
                                     (target_image_coord.y - boresight_pos.y)).sqrt();
                                if target_boresight_distance < target_close_threshold {
                                    let image_rect = Rect::at(0, 0).of_size(width, height);
                                    // Get a sub-image centered on the boresight.
                                    let bs_image_size = std::cmp::min(width, height) / 6;
                                    boresight_image_region = Some(Rect::at(
                                        boresight_pos.x as i32 - bs_image_size as i32/2,
                                        boresight_pos.y as i32 - bs_image_size as i32/2)
                                                                  .of_size(
                                                                      bs_image_size as u32,
                                                                      bs_image_size as u32));
                                    boresight_image_region =
                                        Some(boresight_image_region.
                                             unwrap().intersect(image_rect).unwrap());
                                    // We scale up the pixel values in the sub_image for good
                                    // display visibility.
                                    boresight_image = Some(
                                        image.view(boresight_image_region.unwrap().left() as u32,
                                                   boresight_image_region.unwrap().top() as u32,
                                                   bs_image_size as u32,
                                                   bs_image_size as u32).to_image());
                                    let mut histogram: [u32; 256] = [0_u32; 256];
                                    for pixel_value in boresight_image.as_ref().unwrap().pixels() {
                                        histogram[pixel_value.0[0] as usize] += 1;
                                    }
                                    // Compute peak_value as the average of the 5 brightest pixels.
                                    let peak_pixel_value = max(average_top_values(&histogram, 5), 64);
                                    remove_stars_from_histogram(&mut histogram, /*sigma=*/8.0);
                                    let min_pixel_value = get_level_for_fraction(&histogram, 0.9);
                                    scale_image_mut(boresight_image.as_mut().unwrap(),
                                                    min_pixel_value as u8,
                                                    peak_pixel_value as u8,
                                                    /*gamma=*/0.7);
                                }
                            }
                        }
                    }
                } else {
                    locked_state.solve_success_stats.add_value(0.0);
                    solution_callback(Some(detect_result.clone()), None);
                }
                locked_state.solve_latency_stats.add_value(elapsed.as_secs_f64());
            }
            // Post the result.
            locked_state.plate_solution = Some(PlateSolution{
                detect_result,
                tetra3_solve_result,
                slew_request,
                boresight_image,
                boresight_image_region,
                solve_finish_time,
                processing_duration: elapsed,
                solve_interval_stats: locked_state.solve_interval_stats.value_stats.clone(),
                solve_latency_stats: locked_state.solve_latency_stats.value_stats.clone(),
                solve_attempt_stats: locked_state.solve_attempt_stats.value_stats.clone(),
                solve_success_stats: locked_state.solve_success_stats.value_stats.clone(),
            });
        }  // loop.
    }
}

#[derive(Clone)]
pub struct PlateSolution {
    // The detect result used to produce the information in this solve result.
    pub detect_result: DetectResult,

    // The plate solution for `detect_result`. Omitted if a solve was not
    // attempted.
    pub tetra3_solve_result: Option<SolveResultProto>,

    // If the TelescopePosition has an active slew request, we populate
    // `slew_request` with its information.
    pub slew_request: Option<cedar::SlewRequest>,

    // A small crop of the full resolution `detect_result.captured_image`
    // centered at the boresight. Brightness scaled to full range for
    // visibility. This is present if `slew_request` is present and the slew
    // target is close to the boresight.
    pub boresight_image: Option<GrayImage>,

    // The location of `boresight_image`. Omitted if `boresight_image` is
    // omitted.
    pub boresight_image_region: Option<Rect>,

    // Time at which the plate solve completed. Omitted if a solve was not
    // attempted.
    pub solve_finish_time: Option<SystemTime>,

    // Time taken to produce this PlateSolution, excluding the time taken to
    // detect stars.
    pub processing_duration: std::time::Duration,

    // Seconds per plate solve cycle.
    pub solve_interval_stats: cedar::ValueStats,

    // Distribution of `processing_duration` values.
    pub solve_latency_stats: cedar::ValueStats,

    // Fraction of cycles in which a plate solve was attempted.
    pub solve_attempt_stats: cedar::ValueStats,

    // Fraction of attempted plate solves succeeded.
    pub solve_success_stats: cedar::ValueStats,
}
