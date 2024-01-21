use crate::detect_engine::{DetectEngine, DetectResult};

use std::ops::DerefMut;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use canonical_error::{CanonicalError, failed_precondition_error, invalid_argument_error};
use chrono::{DateTime, Local, Utc};
use image::GrayImage;
use log::{error, info};
use tonic::transport::{Endpoint, Uri};
use tokio::net::UnixStream;
use tower::service_fn;

use crate::tetra3_server::{ImageCoord, SolveRequest, SolveResult as SolveResultProto};
use crate::tetra3_server::tetra3_client::Tetra3Client;
use crate::value_stats::ValueStatsAccumulator;
use crate::cedar;

pub struct SolveEngine {
    // Our connection to the tetra3 gRPC server.
    client: Arc<Mutex<Tetra3Client<tonic::transport::Channel>>>,

    // Our state, shared between SolveEngine methods and the worker thread.
    state: Arc<Mutex<SolveState>>,

    // Detect engine settings can be adjusted behind our back.
    detect_engine: Arc<Mutex<DetectEngine>>,

    // Condition variable signalled whenever `state.plate_solution` is populated.
    // Also signalled when the worker thread exits.
    plate_solution_available: Arc<Condvar>,
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
    fov_max_error: Option<f32>,
    match_radius: f32,
    match_threshold: f32,
    solve_timeout: Duration,
    target_pixel: Option<ImageCoord>,
    distortion: f32,
    return_matches: bool,
    match_max_error: f32,

    solve_interval_stats: ValueStatsAccumulator,
    solve_latency_stats: ValueStatsAccumulator,
    solve_attempt_stats: ValueStatsAccumulator,
    solve_success_stats: ValueStatsAccumulator,

    plate_solution: Option<PlateSolution>,

    // Set by stop(); the worker thread exits when it sees this.
    stop_request: bool,

    worker_thread: Option<thread::JoinHandle<()>>,
}

impl Drop for SolveEngine {
    fn drop(&mut self) {
        self.stop();
    }
}

impl SolveEngine {
    async fn connect(tetra3_server_address: String)
                     -> Result<Tetra3Client<tonic::transport::Channel>, CanonicalError> {
        // Set up gRPC client, connect to a UDS socket. URL is ignored.
        let mut backoff = Duration::from_millis(1);
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
                    if backoff > Duration::from_secs(5) {
                        return Err(failed_precondition_error(
                            format!("Error connecting to Tetra server at {:?}: {:?}",
                                    tetra3_server_address, e).as_str()));
                    }
                    thread::sleep(backoff);
                    backoff *= 2;
                }
            }
        }
    }

    pub async fn new(detect_engine: Arc<Mutex<DetectEngine>>,
                     tetra3_server_address: String,
                     update_interval: Duration, stats_capacity: usize)
                     -> Result<Self, CanonicalError> {
        let client = Self::connect(tetra3_server_address).await?;
        Ok(SolveEngine{
            client: Arc::new(Mutex::new(client)),
            state: Arc::new(Mutex::new(SolveState{
                frame_id: None,
                update_interval,
                minimum_stars: 4,
                fov_estimate: None,
                fov_max_error: None,
                match_radius: 0.01,
                match_threshold: 0.001,
                // solve_timeout: Duration::from_secs(5),
                solve_timeout: Duration::from_secs(1),
                target_pixel: None,
                distortion: 0.0,
                return_matches: true,
                match_max_error: 0.005,
                solve_interval_stats: ValueStatsAccumulator::new(stats_capacity),
                solve_latency_stats: ValueStatsAccumulator::new(stats_capacity),
                solve_attempt_stats: ValueStatsAccumulator::new(stats_capacity),
                solve_success_stats: ValueStatsAccumulator::new(stats_capacity),
                plate_solution: None,
                stop_request: false,
                worker_thread: None,
            })),
            detect_engine: detect_engine.clone(),
            plate_solution_available: Arc::new(Condvar::new()),
        })
    }

    pub fn set_update_interval(&mut self, update_interval: Duration)
                               -> Result<(), CanonicalError> {
        let mut locked_state = self.state.lock().unwrap();
        locked_state.update_interval = update_interval;
        // Don't need to do anything, worker thread will pick up the change when
        // it finishes the current interval.
        Ok(())
    }

    pub fn set_fov_estimate(&mut self, fov_estimate: Option<f32>,
                            fov_max_error: Option<f32>)
                               -> Result<(), CanonicalError> {
        let mut locked_state = self.state.lock().unwrap();
        if fov_estimate.is_some() && fov_estimate.unwrap() <= 0.0 {
            return Err(invalid_argument_error(
                format!("fov_estimate must be positive; got {}",
                        fov_estimate.unwrap()).as_str()));
        }
        if fov_max_error.is_some() && fov_max_error.unwrap() <= 0.0 {
            return Err(invalid_argument_error(
                format!("fov_max_error must be positive; got {}",
                        fov_max_error.unwrap()).as_str()));
        }
        if fov_estimate.is_none() && fov_max_error.is_some() {
            return Err(invalid_argument_error(
                "Cannot provide fov_max_error without fov_estimate"));
        }
        locked_state.fov_estimate = fov_estimate;
        locked_state.fov_max_error = fov_max_error;
        // Don't need to do anything, worker thread will pick up the change when
        // it finishes the current interval.
        Ok(())
    }

    pub fn set_target_pixel(&mut self, target_pixel: Option<ImageCoord>)
                               -> Result<(), CanonicalError> {
        let mut locked_state = self.state.lock().unwrap();
        locked_state.target_pixel = target_pixel;
        // Don't need to do anything, worker thread will pick up the change when
        // it finishes the current interval.
        Ok(())
    }
    pub fn target_pixel(&self) -> Result<Option<ImageCoord>, CanonicalError> {
        let locked_state = self.state.lock().unwrap();
        Ok(locked_state.target_pixel.clone())
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

    // TODO: drop this method and state var, we determine match_max_error based
    // on whether we have fov_estimate.
    pub fn set_match_max_error(&mut self, match_max_error: f32)
                               -> Result<(), CanonicalError> {
        if match_max_error < 0.0 || match_max_error >= 0.1 {
            return Err(invalid_argument_error(
                format!("match_max_error must be in [0, 0.1); got {}",
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
    pub fn get_next_result(&mut self, prev_frame_id: Option<i32>) -> PlateSolution {
        let mut state = self.state.lock().unwrap();
        // Get the most recently posted result.
        loop {
            // Start worker thread if not yet started (or exited).
            if state.worker_thread.is_none() {
                // Give time for tetra3_server binary to start up and accept connections.
                thread::sleep(Duration::from_secs(1));
                let cloned_client = self.client.clone();
                let cloned_state = self.state.clone();
                let cloned_detect_engine = self.detect_engine.clone();
                let cloned_condvar = self.plate_solution_available.clone();
                state.worker_thread = Some(thread::spawn(|| {
                    let rt = tokio::runtime::Runtime::new().unwrap();
                    rt.block_on(SolveEngine::worker(
                        cloned_client, cloned_state,
                        cloned_detect_engine, cloned_condvar));
                }));
            }
            if state.plate_solution.is_none() {
                state = self.plate_solution_available.wait(state).unwrap();
                continue;
            }
            // Wait if the posted result is the same as the one the caller has
            // already obtained.
            if prev_frame_id.is_some() &&
                (state.plate_solution.as_ref().unwrap().detect_result.frame_id ==
                 prev_frame_id.unwrap())
            {
                state = self.plate_solution_available.wait(state).unwrap();
                continue;
            }
            break;
        }
        // Don't consume it, other clients may want it.
        state.plate_solution.clone().unwrap()
    }

    pub fn reset_session_stats(&mut self) {
        let mut state = self.state.lock().unwrap();
        state.solve_interval_stats.reset_session();
        state.solve_latency_stats.reset_session();
        state.solve_attempt_stats.reset_session();
        state.solve_success_stats.reset_session();
    }

    // TODO: arg specifying directory to save to.
    pub fn save_image(&self) -> Result<(), CanonicalError> {
        // Grab most recent image.
        let mut locked_detect_engine = self.detect_engine.lock().unwrap();
        let captured_image =
            &locked_detect_engine.get_next_result(/*frame_id=*/None).captured_image;
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

    /// Shuts down the worker thread; this can save power if get_next_result()
    /// will not be called soon. A subsequent call to get_next_result() will
    /// re-start processing, at the expense of that first get_next_result() call
    /// taking longer than usual.
    pub fn stop(&mut self) {
        let mut state = self.state.lock().unwrap();
        if state.worker_thread.is_none() {
            return;
        }
        state.stop_request = true;
        while state.worker_thread.is_some() {
            state = self.plate_solution_available.wait(state).unwrap();
        }
    }

    async fn worker(client: Arc<Mutex<Tetra3Client<tonic::transport::Channel>>>,
                    state: Arc<Mutex<SolveState>>,
                    detect_engine: Arc<Mutex<DetectEngine>>,
                    plate_solution_available: Arc<Condvar>) {
        // Keep track of when we started the solve cycle.
        let mut last_result_time: Option<Instant> = None;
        loop {
            let update_interval: Duration;
            {
                let mut locked_state = state.lock().unwrap();
                update_interval = locked_state.update_interval;
                if locked_state.stop_request {
                    info!("Stopping solve engine");
                    locked_state.stop_request = false;
                    break;
                }
                // TODO: another stopping condition can be: if no
                // get_next_result() calls are seen for more than N seconds,
                // stop. The next get_next_result() call will restart the worker
                // thread.
            }
            // Is it time to generate the next PlateSolution?
            let now = Instant::now();
            if last_result_time.is_some() {
                let next_update_time = last_result_time.unwrap() + update_interval;
                if next_update_time > now {
                    info!("sleeping for {:?}", next_update_time - now);
                    thread::sleep(next_update_time - now);
                    continue;
                }
            }

            // Time to do a solve processing cycle.
            if last_result_time.is_some() {
                let elapsed = last_result_time.unwrap().elapsed();
                let mut locked_state = state.lock().unwrap();
                locked_state.solve_interval_stats.add_value(elapsed.as_secs_f64());
            }
            last_result_time = Some(now);

            let detect_result: DetectResult;
            let mut solve_request = SolveRequest::default();
            let minimum_stars;
            let frame_id;
            {
                let locked_state = state.lock().unwrap();
                minimum_stars = locked_state.minimum_stars;

                // Set up SolveRequest.
                solve_request.fov_estimate = locked_state.fov_estimate;
                solve_request.fov_max_error = locked_state.fov_max_error;
                solve_request.match_radius = Some(locked_state.match_radius);
                solve_request.match_threshold = Some(locked_state.match_threshold);

                let solve_timeout = locked_state.solve_timeout.as_secs_f64();
                let solve_timeout_int = solve_timeout as i64;
                let solve_timeout_frac = solve_timeout - solve_timeout_int as f64;
                solve_request.solve_timeout = Some(prost_types::Duration {
                    seconds: solve_timeout_int,
                    nanos: (solve_timeout_frac * 1000000000.0) as i32,
                });

                if locked_state.target_pixel.is_some() {
                    solve_request.target_pixels.push(
                        locked_state.target_pixel.as_ref().unwrap().clone());
                }
                solve_request.distortion = Some(locked_state.distortion);
                solve_request.return_matches = locked_state.return_matches;
                solve_request.match_max_error = Some(locked_state.match_max_error);
                frame_id = locked_state.frame_id;
            }
            {
                // Get the most recent star detection result.
                let mut locked_detect_engine = detect_engine.lock().unwrap();
                detect_result = locked_detect_engine.get_next_result(frame_id);
            }
            {
                let mut locked_state = state.lock().unwrap();
                let locked_state_mut = locked_state.deref_mut();
                locked_state_mut.frame_id = Some(detect_result.frame_id);
            }

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
                match client.lock().unwrap().solve_from_centroids(solve_request).await {
                    Err(e) => {
                        error!("Unexpected error {:?}", e);
                        break;  // Exit the worker thread.
                    },
                    Ok(response) => {
                        tetra3_solve_result = Some(response.into_inner());
                    }
                }
                solve_finish_time = Some(SystemTime::now());
            }

            let elapsed = process_start_time.elapsed();
            let mut locked_state = state.lock().unwrap();
            if tetra3_solve_result.is_none() {
                locked_state.solve_attempt_stats.add_value(0.0);
            } else {
                locked_state.solve_attempt_stats.add_value(1.0);
                if tetra3_solve_result.as_ref().unwrap().matches.is_some() {
                    locked_state.solve_success_stats.add_value(1.0);
                } else {
                    locked_state.solve_success_stats.add_value(0.0);
                }
                locked_state.solve_latency_stats.add_value(elapsed.as_secs_f64());
            }
            // Post the result.
            locked_state.plate_solution = Some(PlateSolution{
                detect_result,
                tetra3_solve_result,
                solve_finish_time,
                processing_duration: process_start_time.elapsed(),
                solve_interval_stats: locked_state.solve_interval_stats.value_stats.clone(),
                solve_latency_stats: locked_state.solve_latency_stats.value_stats.clone(),
                solve_attempt_stats: locked_state.solve_attempt_stats.value_stats.clone(),
                solve_success_stats: locked_state.solve_success_stats.value_stats.clone(),
            });
            plate_solution_available.notify_all();
        }  // loop.
        let mut locked_state = state.lock().unwrap();
        locked_state.worker_thread = None;
        plate_solution_available.notify_all();
    }
}

#[derive(Clone)]
pub struct PlateSolution {
    // The detect result used to produce the information in this solve result.
    pub detect_result: DetectResult,

    // The plate solution for `detect_result`. Omitted if a solve was not
    // attempted.
    pub tetra3_solve_result: Option<SolveResultProto>,

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
