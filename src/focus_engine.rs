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
use star_gate::algorithm::{StarDescription, estimate_noise_from_image,
                           get_stars_from_image, summarize_region_of_interest};

pub struct FocusEngine {
    // Our state, shared between FocusEngine methods and the worker thread.
    state: Arc<Mutex<SharedState>>,

    // Condition variable signalled whenever `state.focus_result` is populated.
    // Also signalled when the worker thread exits.
    focus_result_available: Arc<Condvar>,
}

// State shared between worker thread and the FocusEngine methods.
struct SharedState {
    // Note: camera settings can be adjusted behind our back.
    camera: Arc<Mutex<dyn AbstractCamera>>,
    frame_id: Option<i32>,

    // If zero, use auto exposure. If positive, this is the camera's exposure
    // integration time.
    exposure_time: Duration,

    // Zero means go fast as images are captured.
    update_interval: Duration,

    // The `frame_id` to use for the next posted `focus_result`.
    next_focus_result_id: i32,

    focus_result: Option<FocusResult>,

    // Set by stop(); the worker thread exits when it sees this.
    stop_request: bool,

    worker_thread: Option<thread::JoinHandle<()>>,
}

impl Drop for FocusEngine {
    fn drop(&mut self) {
        self.stop();
    }
}

impl FocusEngine {
    pub fn new(camera: Arc<Mutex<dyn AbstractCamera>>,
               update_interval: Duration, exposure_time: Duration)
               -> FocusEngine {
        FocusEngine{
            state: Arc::new(Mutex::new(SharedState{
                camera: camera.clone(),
                frame_id: None,
                exposure_time,
                update_interval,
                next_focus_result_id: 0,
                focus_result: None,
                stop_request: false,
                worker_thread: None,
            })),
            focus_result_available: Arc::new(Condvar::new()),
        }
    }

    pub fn set_exposure_time(&mut self, exp_time: Duration)
                             -> Result<(), CanonicalError> {
        let mut locked_state = self.state.lock().unwrap();
        if exp_time != locked_state.exposure_time {
            locked_state.exposure_time = exp_time;
            if !exp_time.is_zero() {
                let mut locked_camera = locked_state.camera.lock().unwrap();
                locked_camera.set_exposure_duration(exp_time)?
            }
            // Don't need to invalidate `focus_result` state.
        }
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

    /// Obtains a result bundle, as configured above. The returned result is
    /// "fresh" in that we either wait to process a new exposure or return the
    /// result of processing the most recently completed exposure.
    /// This function does not "consume" the information that it returns;
    /// multiple callers will receive the current result bundle (or next result,
    /// if there is not yet a current result) if `prev_frame_id` is omitted. If
    /// `prev_frame_id` is supplied, the call blocks while the current result
    /// has the same id value.
    /// Returns: the processed result along with its frame_id value.
    pub fn get_next_result(&mut self, prev_frame_id: Option<i32>) -> FocusResult {
        let mut state = self.state.lock().unwrap();
        // Start worker thread if not yet started.
        if state.worker_thread.is_none() {
            let cloned_state = self.state.clone();
            let cloned_condvar = self.focus_result_available.clone();
            state.worker_thread = Some(thread::spawn(|| {
                FocusEngine::worker(cloned_state, cloned_condvar);
            }));
        }
        // Get the most recently posted result.
        loop {
            if state.focus_result.is_none() {
                state = self.focus_result_available.wait(state).unwrap();
                continue;
            }
            // Wait if the posted result is the same as the one the caller has
            // already obtained.
            if prev_frame_id.is_some() &&
                state.focus_result.as_ref().unwrap().frame_id == prev_frame_id.unwrap()
            {
                state = self.focus_result_available.wait(state).unwrap();
                continue;
            }
            break;
        }
        // Don't consume it, other clients may want it.
        state.focus_result.clone().unwrap()
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
            state = self.focus_result_available.wait(state).unwrap();
        }
    }

    fn worker(state: Arc<Mutex<SharedState>>,
              focus_result_available: Arc<Condvar>) {
        // Keep track of when we started the focus cycle.
        let mut last_result_time: Option<Instant> = None;
        loop {
            let exp_time: Duration;
            let update_interval: Duration;
            // In focus engine, we use hardwired values for StarGate parameters.
            // TODO(smr): revise.
            let sigma = 8.0;
            let max_size = 5;
            {
                let mut locked_state = state.lock().unwrap();
                exp_time = locked_state.exposure_time;
                update_interval = locked_state.update_interval;
                if locked_state.stop_request {
                    info!("Stopping focus engine");
                    locked_state.stop_request = false;
                    break;
                }
                // TODO: another stopping condition can be: if no
                // get_next_result() calls are seen for more than N seconds,
                // stop. The next get_next_result() call will restart the worker
                // thread.
            }
            // Is it time to generate the next FocusResult?
            let now = Instant::now();
            if last_result_time.is_some() {
                let next_update_time = last_result_time.unwrap() + update_interval;
                if next_update_time > now {
                    thread::sleep(next_update_time - now);
                    continue;
                }
            }

            // Time to do a focus processing cycle.
            last_result_time = Some(now);

            let captured_image: Arc<CapturedImage>;
            {
                let mut locked_state = state.lock().unwrap();
                let locked_state_mut = locked_state.deref_mut();
                let mut locked_camera = locked_state_mut.camera.lock().unwrap();
                match locked_camera.capture_image(locked_state_mut.frame_id) {
                    Ok((img, id)) => {
                        captured_image = img;
                        locked_state_mut.frame_id = Some(id);
                    }
                    Err(e) => {
                        error!("Error capturing image: {}", &e.to_string());
                        break;  // Abandon thread execution!
                    }
                }
            }
            // Process the just-acquired image.
            let image: &GrayImage = &captured_image.image;
            let (width, height) = image.dimensions();
            let center_size = std::cmp::min(width, height) / 3;
            let center_region = Rect::at(((width - center_size) / 2) as i32,
                                         ((height - center_size) / 2) as i32)
                .of_size(center_size, center_size);

            // TODO(smr): move histogram computation into autoexp conditional block.
            let noise_estimate = estimate_noise_from_image(image);
            let roi_summary = summarize_region_of_interest(
                image, &center_region, noise_estimate, sigma);
            let mut peak_value = 1_u8;  // Avoid div0 below.
            let histogram = &roi_summary.histogram;
            for bin in 2..256 {
                if histogram[bin] > 0 {
                    peak_value = bin as u8;
                }
            }
            let peak_value_goal = 200;
            if exp_time.is_zero() {
                // Adjust exposure time based on histogram of center_region. We
                // scale up the pixel values in the zoomed_peak_image for good
                // display visibility.
                // Compute how much to scale the previous exposure integration
                // time to move towards the goal.
                let correction_factor =
                    if peak_value == 255 {
                        // We don't know how overexposed we are. Cut the
                        // exposure time in half.
                        0.5
                    } else {
                        // Move proportionally towards the goal.
                        peak_value_goal as f32 / peak_value as f32
                    };
                if correction_factor < 0.8 || correction_factor > 1.2 {
                    let prev_exposure_duration_secs =
                        captured_image.capture_params.exposure_duration.as_secs_f32();
                    let mut new_exposure_duration_secs =
                        prev_exposure_duration_secs * correction_factor;
                    // Bound exposure duration to 0.01ms..1s.
                    new_exposure_duration_secs = f32::max(
                        new_exposure_duration_secs, 0.00001);
                    new_exposure_duration_secs = f32::min(
                        new_exposure_duration_secs, 1.0);
                    if prev_exposure_duration_secs != new_exposure_duration_secs {
                        debug!("Setting new exposure duration {}s",
                               new_exposure_duration_secs);
                        let locked_state = state.lock().unwrap();
                        let mut locked_camera = locked_state.camera.lock().unwrap();
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
                }
            }
            // Get a small sub-image centered on the peak coordinates.
            let peak_position = (roi_summary.peak_x, roi_summary.peak_y);
            let sub_image_size = 30;
            let peak_region = Rect::at((peak_position.0 - sub_image_size/2) as i32,
                                       (peak_position.1 - sub_image_size/2) as i32)
                .of_size(sub_image_size as u32, sub_image_size as u32);

            debug!("peak {} at x/y {}/{}",
                   peak_value, peak_region.left(), peak_region.top());
            let mut peak_image = image.view(peak_region.left() as u32,
                                            peak_region.top() as u32,
                                            sub_image_size as u32,
                                            sub_image_size as u32).to_image();
            contrast::stretch_contrast_mut(&mut peak_image, 0, peak_value);

            // Run StarGate to obtain star centroids; also obtain binned image.
            let noise_estimate = estimate_noise_from_image(&image);
            let (mut stars, hot_pixel_count, binned_image) =
                get_stars_from_image(&image, noise_estimate,
                                     sigma, max_size as u32,
                                     /*detect_hot_pixels=*/true,
                                     /*create_binned_image=*/true);
            // Sort by brightness estimate, brightest first.
            stars.sort_by(|a, b| b.mean_brightness.partial_cmp(&a.mean_brightness).unwrap());
            // Run StarGate a second time on the binned image.
            let binned_noise_estimate = estimate_noise_from_image(
                &binned_image.as_ref().unwrap());
            let (binned_stars, _, _) =
                get_stars_from_image(&binned_image.as_ref().unwrap(),
                                     binned_noise_estimate,
                                     sigma, max_size as u32,
                                     /*detect_hot_pixels=*/false,
                                     /*create_binned_image=*/false);
            // Post the result.
            let mut locked_state = state.lock().unwrap();
            locked_state.focus_result = Some(FocusResult{
                frame_id: locked_state.next_focus_result_id,
                captured_image: captured_image.clone(),
                binned_image: Arc::new(binned_image.unwrap()),
                star_candidates: stars,
                hot_pixel_count: hot_pixel_count as i32,
                center_region,
                center_peak_position: peak_position,
                peak_image,
                peak_image_region: peak_region,
                binned_star_candidate_count: binned_stars.len() as i32,
                processing_duration: last_result_time.unwrap().elapsed(),
            });
            locked_state.next_focus_result_id += 1;
            focus_result_available.notify_all();
        }  // loop.
        let mut locked_state = state.lock().unwrap();
        locked_state.worker_thread = None;
        focus_result_available.notify_all();
    }
}

#[derive(Clone)]
pub struct FocusResult {
    // See the corresponding field in FrameResult.
    pub frame_id: i32,

    // The full resolution camera image used to produce the information in this
    // result.
    pub captured_image: Arc<CapturedImage>,

    // The 2x2 binned image computed (with hot pixel removal) from
    // 'captured_image'.
    pub binned_image: Arc<GrayImage>,

    // The star candidates detected by StarGate; ordered by highest
    // mean_brightness first. From full resolution image.
    pub star_candidates: Vec<StarDescription>,

    // The number of hot pixels detected by StarGate.
    pub hot_pixel_count: i32,

    // See the corresponding field in FrameResult.
    pub center_region: Rect,

    // See the corresponding field in FrameResult.
    pub center_peak_position: (i32, i32),

    // A small full resolution crop of `captured_image` centered at
    // `center_peak_position`.
    pub peak_image: GrayImage,

    // The location of 'peak_image'.
    pub peak_image_region: Rect,

    // In setup mode (focusing), star detection is run twice: once at full
    // resolution, where 'star_candidates' above has the result; and a second
    // run on the 2x2 binned image, where the number of binned-image centroids
    // is returned here.
    pub binned_star_candidate_count: i32,

    // Time taken to produce this FocusResult, excluding the time taken to
    // acquire the image.
    pub processing_duration: std::time::Duration,
}
