use camera_service::abstract_camera::{AbstractCamera, CapturedImage};

use std::ops::DerefMut;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use canonical_error::CanonicalError;
use image::{GenericImageView, GrayImage};
use imageproc::contrast;
use imageproc::rect::Rect;
use log::{debug, error, info};
use cedar_detect::algorithm::{StarDescription, estimate_noise_from_image,
                              get_stars_from_image, summarize_region_of_interest};
use crate::value_stats::ValueStatsAccumulator;
use crate::cedar;

pub struct DetectEngine {
    // Bounds the range of exposure durations to be set by setup mode
    // auto-exposure.
    // The set_exposure_time() function is not bound by these limits, nor
    // is the operating mode auto-exposure.
    min_exposure_duration: Duration,
    max_exposure_duration: Duration,

    // Our state, shared between DetectEngine methods and the worker thread.
    state: Arc<Mutex<DetectState>>,

    // Note: camera settings can be adjusted behind our back.
    camera: Arc<Mutex<dyn AbstractCamera>>,

    // Condition variable signalled whenever `state.detect_result` is populated.
    // Also signalled when the worker thread exits.
    detect_result_available: Arc<Condvar>,
}

// State shared between worker thread and the DetectEngine methods.
struct DetectState {
    frame_id: Option<i32>,

    // If true, use auto exposure.
    auto_exposure: bool,

    // Zero means go fast as images are captured.
    update_interval: Duration,

    // Parameters for star detection algorithm.
    detection_sigma: f32,
    detection_max_size: i32,

    // True means populate `DetectResult.focus_aid` info.
    focus_mode_enabled: bool,

    // In operate mode (`focus_mode_enabled` is false), the auto-exposure
    // algorithm uses this as the desired number of detected stars. The
    // algorithm allows the number of detected stars to vary around the goal by
    // a large amount to the high side (is OK to have more stars than needed)
    // but only a small amount to the low side.
    star_count_goal: i32,

    // When using auto exposure in operate mode, this is the exposure duration
    // determined (by calibration) to yield `star_count_goal` detected stars.
    // Auto exposure logic will only deviate from this by a bounded amount.
    // None if calibration result is not yet available.
    calibrated_exposure_duration: Option<Duration>,

    detect_latency_stats: ValueStatsAccumulator,

    detect_result: Option<DetectResult>,

    // Set by stop(); the worker thread exits when it sees this.
    stop_request: bool,

    worker_thread: Option<thread::JoinHandle<()>>,
}

impl Drop for DetectEngine {
    fn drop(&mut self) {
        self.stop();
    }
}

impl DetectEngine {
    pub fn new(min_exposure_duration: Duration,
               max_exposure_duration: Duration,
               camera: Arc<Mutex<dyn AbstractCamera>>,
               update_interval: Duration, auto_exposure: bool,
               focus_mode_enabled: bool, star_count_goal: i32, stats_capacity: usize)
               -> Self {
        DetectEngine{
            min_exposure_duration,
            max_exposure_duration,
            state: Arc::new(Mutex::new(DetectState{
                frame_id: None,
                auto_exposure,
                update_interval,
                detection_sigma: 8.0,
                detection_max_size: 3,
                focus_mode_enabled,
                star_count_goal,
                calibrated_exposure_duration: None,
                detect_latency_stats: ValueStatsAccumulator::new(stats_capacity),
                detect_result: None,
                stop_request: false,
                worker_thread: None,
            })),
            camera: camera.clone(),
            detect_result_available: Arc::new(Condvar::new()),
        }
    }

    // If `exp_time` is zero, enables auto exposure. In setup mode, auto
    // exposure is based on a histogram of the central region, and aims to make
    // the brightest part of the central region bright but not saturated. In
    // operate mode, auto exposure is based on the number of detected stars in
    // the entire image.
    pub fn set_exposure_time(&mut self, exp_time: Duration)
                             -> Result<(), CanonicalError> {
        if !exp_time.is_zero() {
            let mut locked_camera = self.camera.lock().unwrap();
            locked_camera.set_exposure_duration(exp_time)?
        }
        let mut locked_state = self.state.lock().unwrap();
        locked_state.auto_exposure = exp_time.is_zero();
        // Don't need to do anything, worker thread will pick up the change when
        // it finishes the current interval.
        Ok(())
    }

    pub fn set_update_interval(&mut self, update_interval: Duration)
                               -> Result<(), CanonicalError> {
        let mut locked_state = self.state.lock().unwrap();
        locked_state.update_interval = update_interval;
        // Don't need to do anything, worker thread will pick up the change when
        // it finishes the current interval.
        Ok(())
    }

    // Note that `detection_max_size` is in the 2x2 binned image.
    pub fn set_detection_params(&mut self, detection_sigma: f32,
                                detection_max_size: i32)
                                -> Result<(), CanonicalError> {
        let mut locked_state = self.state.lock().unwrap();
        locked_state.detection_sigma = detection_sigma;
        locked_state.detection_max_size = detection_max_size;
        // Don't need to do anything, worker thread will pick up the change when
        // it finishes the current interval.
        Ok(())
    }

    pub fn set_focus_mode(&mut self, enabled: bool) {
        let mut locked_state = self.state.lock().unwrap();
        locked_state.focus_mode_enabled = enabled;
        // Don't need to do anything, worker thread will pick up the change when
        // it finishes the current interval.
    }

    pub fn get_star_count_goal(&self) -> i32 {
        return self.state.lock().unwrap().star_count_goal;
    }
    // TODO: set_star_count_goal()?

    pub fn set_calibrated_exposure_duration(
        &mut self, calibrated_exposure_duration: Duration) {
        let mut locked_state = self.state.lock().unwrap();
        locked_state.calibrated_exposure_duration = Some(calibrated_exposure_duration);
        // Don't need to do anything, worker thread will pick up the change when
        // it finishes the current interval.
    }

    /// Obtains a result bundle, as configured above. The returned result is
    /// "fresh" in that we either wait to process a new exposure or return the
    /// result of processing the most recently completed exposure.
    /// This function does not "consume" the information that it returns;
    /// multiple callers will receive the current result bundle (or next result,
    /// if there is not yet a current result) if `prev_frame_id` is omitted. If
    /// `prev_frame_id` is supplied, the call blocks while the current result
    /// has the same id value.
    /// Returns: the processed result along with its frame_id value.
    pub fn get_next_result(&mut self, prev_frame_id: Option<i32>) -> DetectResult {
        let mut locked_state = self.state.lock().unwrap();
        // Start worker thread if not yet started.
        if locked_state.worker_thread.is_none() {
            let min_exposure_duration = self.min_exposure_duration;
            let max_exposure_duration = self.max_exposure_duration;
            let cloned_state = self.state.clone();
            let cloned_camera = self.camera.clone();
            let cloned_condvar = self.detect_result_available.clone();
            locked_state.worker_thread = Some(thread::spawn(move || {
                DetectEngine::worker(min_exposure_duration,
                                     max_exposure_duration,
                                     cloned_state, cloned_camera, cloned_condvar);
            }));
        }
        // Get the most recently posted result.
        loop {
            if locked_state.detect_result.is_none() {
                locked_state = self.detect_result_available.wait(locked_state).unwrap();
                continue;
            }
            // Wait if the posted result is the same as the one the caller has
            // already obtained.
            if prev_frame_id.is_some() &&
                locked_state.detect_result.as_ref().unwrap().frame_id == prev_frame_id.unwrap()
            {
                locked_state = self.detect_result_available.wait(locked_state).unwrap();
                continue;
            }
            break;
        }
        // Don't consume it, other clients may want it.
        locked_state.detect_result.clone().unwrap()
    }

    pub fn reset_session_stats(&mut self) {
        let mut state = self.state.lock().unwrap();
        state.detect_latency_stats.reset_session();
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
            state = self.detect_result_available.wait(state).unwrap();
        }
    }

    fn worker(min_exposure_duration: Duration,
              max_exposure_duration: Duration,
              state: Arc<Mutex<DetectState>>,
              camera: Arc<Mutex<dyn AbstractCamera>>,
              detect_result_available: Arc<Condvar>) {
        info!("Starting detect engine");
        // Keep track of when we started the detect cycle.
        let mut last_result_time: Option<Instant> = None;
        loop {
            let auto_exposure: bool;
            let update_interval: Duration;
            let sigma: f32;
            let max_size: i32;
            let focus_mode_enabled: bool;
            let star_count_goal: i32;
            let calibrated_exposure_duration: Option<Duration>;
            {
                let mut locked_state = state.lock().unwrap();
                auto_exposure = locked_state.auto_exposure;
                update_interval = locked_state.update_interval;
                sigma = locked_state.detection_sigma;
                max_size = locked_state.detection_max_size;
                focus_mode_enabled = locked_state.focus_mode_enabled;
                star_count_goal = locked_state.star_count_goal;
                calibrated_exposure_duration =
                    locked_state.calibrated_exposure_duration;
                if locked_state.stop_request {
                    info!("Stopping detect engine");
                    locked_state.stop_request = false;
                    break;
                }
                // TODO: another stopping condition can be: if no
                // get_next_result() calls are seen for more than N seconds,
                // stop. The next get_next_result() call will restart the worker
                // thread.
            }
            // Is it time to generate the next DetectResult?
            let now = Instant::now();
            if last_result_time.is_some() {
                let next_update_time = last_result_time.unwrap() + update_interval;
                if next_update_time > now {
                    thread::sleep(next_update_time - now);
                    continue;
                }
            }

            // Time to do a detect processing cycle.
            last_result_time = Some(now);

            let frame_id;
            {
                let locked_state = state.lock().unwrap();
                frame_id = locked_state.frame_id;
            }
            let captured_image;
            let mut locked_camera = camera.lock().unwrap();
            match locked_camera.capture_image(frame_id) {
                Ok((img, id)) => {
                    drop(locked_camera);
                    captured_image = img;
                    let mut locked_state = state.lock().unwrap();
                    let locked_state_mut = locked_state.deref_mut();
                    locked_state_mut.frame_id = Some(id);
                }
                Err(e) => {
                    error!("Error capturing image: {}", &e.to_string());
                    break;  // Abandon thread execution!
                }
            }

            // Process the just-acquired image.
            let process_start_time = Instant::now();
            let image: &GrayImage = &captured_image.image;
            let (width, height) = image.dimensions();
            let center_size = std::cmp::min(width, height) / 3;
            let center_region = Rect::at(((width - center_size) / 2) as i32,
                                         ((height - center_size) / 2) as i32)
                .of_size(center_size, center_size);
            let noise_estimate = estimate_noise_from_image(&image);
            let prev_exposure_duration_secs =
                captured_image.capture_params.exposure_duration.as_secs_f32();
            let mut new_exposure_duration_secs = prev_exposure_duration_secs;

            let mut focus_aid: Option<FocusAid> = None;
            if focus_mode_enabled {
                let roi_summary = summarize_region_of_interest(
                    &image, &center_region, noise_estimate, sigma);
                let mut peak_value = 1_u8;  // Avoid div0 below.
                let histogram = &roi_summary.histogram;
                for bin in 2..256 {
                    if histogram[bin] > 0 {
                        peak_value = bin as u8;
                    }
                }
                if auto_exposure {
                    // Adjust exposure time based on peak value of center_region.
                    let peak_value_goal = 220;
                    // Compute how much to scale the previous exposure
                    // integration time to move towards the goal. Assumes linear
                    // detector response.
                    let correction_factor =
                        if peak_value == 255 {
                            // We don't know how overexposed we are. Cut the
                            // exposure time in half.
                            0.5
                        } else {
                            // Move proportionally towards the goal.
                            peak_value_goal as f32 / peak_value as f32
                        };
                    // Don't adjust exposure time too often, is a bit janky
                    // because the camera re-initializes.
                    if correction_factor < 0.7 || correction_factor > 1.3 {
                        new_exposure_duration_secs =
                            prev_exposure_duration_secs * correction_factor;
                        // Bound focus mode exposure duration to 0.01ms..1s.
                        new_exposure_duration_secs = f32::max(
                            new_exposure_duration_secs,
                            min_exposure_duration.as_secs_f32());
                        new_exposure_duration_secs = f32::min(
                            new_exposure_duration_secs,
                            max_exposure_duration.as_secs_f32());
                        // Camera exposure duration is updated below.
                    }
                }  // auto_exposure

                let image_rect = Rect::at(0, 0).of_size(width, height);
                // Get a small sub-image centered on the peak coordinates.
                let peak_position = (roi_summary.peak_x, roi_summary.peak_y);
                let sub_image_size = 30;
                let peak_region =
                    Rect::at((peak_position.0 as i32 - sub_image_size/2) as i32,
                             (peak_position.1 as i32 - sub_image_size/2) as i32)
                    .of_size(sub_image_size as u32, sub_image_size as u32);
                let peak_region = peak_region.intersect(image_rect).unwrap();
                debug!("peak {} at x/y {}/{}",
                       peak_value, peak_region.left(), peak_region.top());
                // We scale up the pixel values in the peak_image for good
                // display visibility.
                let mut peak_image = image.view(peak_region.left() as u32,
                                                peak_region.top() as u32,
                                                sub_image_size as u32,
                                                sub_image_size as u32).to_image();
                contrast::stretch_contrast_mut(&mut peak_image, 0, peak_value);
                focus_aid = Some(FocusAid{
                    center_region,
                    center_peak_position: peak_position,
                    peak_image,
                    peak_image_region: peak_region,
                });
            }  // focus_mode_enabled

            // Run CedarDetect on the image.
            let (stars, hot_pixel_count, binned_image) =
                get_stars_from_image(&image, noise_estimate,
                                     sigma, max_size as u32,
                                     /*use_binned_image=*/true,
                                     /*return_binned_image=*/true);
            let elapsed = process_start_time.elapsed();
            let mut locked_state = state.lock().unwrap();
            locked_state.detect_latency_stats.add_value(elapsed.as_secs_f64());

            if !focus_mode_enabled && auto_exposure &&
                calibrated_exposure_duration.is_some()
            {
                let num_stars_detected = stars.len();
                // >1 if we have more stars than goal; <1 if fewer stars than
                // goal.
                let star_goal_fraction =
                    f32::max(num_stars_detected as f32, 1.0) / star_count_goal as f32;
                // Don't adjust exposure time too often, is a bit janky because the
                // camera re-initializes. Allow number of detected stars to greatly
                // exceed goal, but don't allow much of a shortfall.
                if star_goal_fraction < 0.8 || star_goal_fraction > 2.0 {
                    // What is the relationship between exposure time and number
                    // of stars detected?
                    // * If we increase the exposure time by 2.5x, we'll be able
                    //   to detect stars 40% as bright. This corresponds to an
                    //   increase of one stellar magnitude.
                    // * Per https://www.hnsky.org/star_count, at mag=5 a one
                    //   magnitude increase corresponds to around 3x the number
                    //   of stars.
                    // * 2.5x and 3x are "close enough", so we model the number
                    //   of detectable stars as being simply proportional to the
                    //   exposure time. This is OK because we'll only be varying
                    //   the exposure time a modest amount relative to the
                    //   calibrated_exposure_duration.
                    new_exposure_duration_secs =
                        prev_exposure_duration_secs / star_goal_fraction;
                    // Bound exposure duration to be within two stops of
                    // calibrated_exposure_duration.
                    new_exposure_duration_secs = f32::max(
                        new_exposure_duration_secs,
                        (calibrated_exposure_duration.unwrap() / 4).as_secs_f32());
                    new_exposure_duration_secs = f32::min(
                        new_exposure_duration_secs,
                        (calibrated_exposure_duration.unwrap() * 4).as_secs_f32());
                }
            }

            // Update camera exposure time if auto-exposure calls for an
            // adjustment.
            if prev_exposure_duration_secs != new_exposure_duration_secs {
                debug!("Setting new exposure duration {}s",
                       new_exposure_duration_secs);
                let mut locked_camera = camera.lock().unwrap();
                match locked_camera.set_exposure_duration(
                    Duration::from_secs_f32(new_exposure_duration_secs)) {
                    Ok(()) => (),
                    Err(e) => {
                        error!("Error updating exposure duration: {}",
                               &e.to_string());
                        break;  // Abandon thread execution!
                    }
                }
            }

            // Post the result.
            locked_state.detect_result = Some(DetectResult{
                frame_id: locked_state.frame_id.unwrap(),
                captured_image: captured_image,
                binned_image: Arc::new(binned_image.unwrap()),
                star_candidates: stars,
                hot_pixel_count: hot_pixel_count as i32,
                focus_aid,
                processing_duration: elapsed,
                detect_latency_stats:
                locked_state.detect_latency_stats.value_stats.clone(),
            });
            detect_result_available.notify_all();
        }  // loop.
        let mut locked_state = state.lock().unwrap();
        locked_state.worker_thread = None;
        detect_result_available.notify_all();
    }
}

#[derive(Clone)]
pub struct DetectResult {
    // See the corresponding field in cedar.FrameResult proto message.
    pub frame_id: i32,

    // The full resolution camera image used to produce the information in this
    // detect result.
    pub captured_image: CapturedImage,

    // The 2x2 binned image computed (with hot pixel removal) from
    // `captured_image`.
    pub binned_image: Arc<GrayImage>,

    // The star candidates detected by CedarDetect; ordered by highest
    // StarDescription.mean_brightness first.
    pub star_candidates: Vec<StarDescription>,

    // The number of hot pixels detected by CedarDetect.
    pub hot_pixel_count: i32,

    // Included if `focus_mode` is enabled.
    pub focus_aid: Option<FocusAid>,

    // Time taken to produce this DetectResult, excluding the time taken to
    // acquire the image.
    pub processing_duration: std::time::Duration,

    // Distribution of `processing_duration` values.
    pub detect_latency_stats: cedar::ValueStats,
}

#[derive(Clone)]
pub struct FocusAid {
    // See the corresponding field in FrameResult.
    pub center_region: Rect,

    // See the corresponding field in FrameResult.
    pub center_peak_position: (f32, f32),

    // A small full resolution crop of `captured_image` centered at
    // `center_peak_position`.
    pub peak_image: GrayImage,

    // The location of `peak_image`.
    pub peak_image_region: Rect,
}
