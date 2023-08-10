use camera_service::abstract_camera::{AbstractCamera, CapturedImage};

use std::ops::DerefMut;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use image::{GenericImageView, GrayImage};
use imageproc::contrast;
use imageproc::rect::Rect;
use log::{debug, error, info};
use star_gate::algorithm::{estimate_noise_from_image,
                           summarize_region_of_interest};

pub struct FocusEngine {
    // Our state, shared between ASICamera methods and the video capture thread.
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

    // If true, use auto exposure. If false, the caller is expected to have set
    // the camera's exposure integration time. TODO: allow it to be updated via
    // FocusEngine method.
    auto_expose: bool,

    // Zero means go fast as images can be captured. TODO: allow it to be
    // updated via FocusEngine method.
    update_interval: Duration,

    // The `frame_id` to use for the next posted `focus_result`.
    next_focus_result_id: i32,

    focus_result: Option<FocusResult>,

    // Set by stop(); the video capture thread exits when it sees this.
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
               update_interval: Duration, auto_expose: bool) -> FocusEngine {
        let focus_engine = FocusEngine{
            state: Arc::new(Mutex::new(SharedState{
                camera: camera.clone(),
                frame_id: None,
                auto_expose,
                update_interval,
                next_focus_result_id: 0,
                focus_result: None,
                stop_request: false,
                worker_thread: None,
            })),
            focus_result_available: Arc::new(Condvar::new()),
        };
        {
            let cloned_state = focus_engine.state.clone();
            let cloned_condvar = focus_engine.focus_result_available.clone();
            let mut state = focus_engine.state.lock().unwrap();
            state.worker_thread = Some(thread::spawn(|| {
                FocusEngine::worker(cloned_state, cloned_condvar);
            }));
        }
        focus_engine
    }

    // operation methods...
    // TODO: doc this.
    pub fn get_next_result(&mut self, prev_frame_id: Option<i32>) -> FocusResult {
        let mut state = self.state.lock().unwrap();
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

    // TODO: doc this.
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
            let auto_expose: bool;
            let update_interval: Duration;
            {
                let mut locked_state = state.lock().unwrap();
                auto_expose = locked_state.auto_expose;
                update_interval = locked_state.update_interval;
                if locked_state.stop_request {
                    info!("Stopping focus engine");
                    locked_state.stop_request = false;
                    break;
                }
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

            let noise_estimate = estimate_noise_from_image(image);
            // TODO: allow sigma to be passed in.
            let roi_summary = summarize_region_of_interest(
                image, &center_region, noise_estimate, /*sigma=*/6.0);
            let mut peak_value = 1_u8;  // Avoid div0 below.
            let histogram = &roi_summary.histogram;
            for bin in 2..256 {
                if histogram[bin] > 0 {
                    peak_value = bin as u8;
                }
            }
            let peak_value_goal = 64;
            if auto_expose {
                // Adjust exposure time based on histogram of center_region. We
                // aim for a peak brightness of 64 instead of 255 for a shorter
                // exposure integration time. We'll scale up the pixel values in
                // the zoomed_peak_image for good display visibility.
                // Compute how much to scale the previous exposure integration
                // time to move towards the goal.
                let correction_factor = peak_value_goal as f32 / peak_value as f32;
                if peak_value >= 255 ||
                    correction_factor < 0.8 || correction_factor > 1.2
                {
                    let prev_exposure_duration_secs =
                        captured_image.capture_params.exposure_duration.as_secs_f32();
                    let mut new_exposure_duration_secs =
                        prev_exposure_duration_secs * correction_factor;
                    // Bound exposure duration to 0.01ms..2s.
                    new_exposure_duration_secs = f32::max(
                        new_exposure_duration_secs, 0.00001);
                    new_exposure_duration_secs = f32::min(
                        new_exposure_duration_secs, 2.0);
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
            // Use the projections of the center_region to identify the
            // brightest point of the center_region.
            // TODO: why not just grab the peak pixel value within
            // center_region? Can find it while computing histogram in
            // summarize_region_of_interest().
            // TODO: also consider doing a 1d/2d identification of the brightest
            // star candidate, using an approach that allows for severe defocus.
            let mut peak_projected = 0.0_f32;
            let mut peak_y = 0;
            for (y, val) in roi_summary.horizontal_projection.iter().enumerate() {
                if *val > peak_projected {
                    peak_y = y;
                    peak_projected = *val;
                }
            }
            peak_projected = 0.0;
            let mut peak_x = 0;
            for (x, val) in roi_summary.vertical_projection.iter().enumerate() {
                if *val > peak_projected {
                    peak_x = x;
                    peak_projected = *val;
                }
            }
            // Convert to image coordinates.
            let peak_position = (center_region.left() as u32 + peak_x as u32,
                                 center_region.top() as u32 + peak_y as u32);
            // Get a small sub-image centered on the peak coordinates.
            let sub_image_size = 30_u32;
            let peak_region = Rect::at((peak_position.0 - sub_image_size/2) as i32,
                                       (peak_position.1 - sub_image_size/2) as i32)
                .of_size(sub_image_size, sub_image_size);

            info!("peak {} at x/y {}/{}", peak_value, peak_region.left(), peak_region.top());
            let mut peak_image = image.view(peak_region.left() as u32,
                                            peak_region.top() as u32,
                                            sub_image_size, sub_image_size).to_image();
            contrast::stretch_contrast_mut(&mut peak_image, 0, peak_value);

            // Post the result.
            let mut locked_state = state.lock().unwrap();
            locked_state.focus_result = Some(FocusResult{
                frame_id: locked_state.next_focus_result_id,
                captured_image: captured_image.clone(),
                center_region,
                peak_position,
                peak_image,
                peak_image_region: peak_region,
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
    pub frame_id: i32,

    pub captured_image: Arc<CapturedImage>,

    pub center_region: Rect,

    pub peak_position: (u32, u32),

    pub peak_image: GrayImage,

    pub peak_image_region: Rect,

    // Time taken to produce this FocusResult, excluding the time taken to
    // acquire the image.
    pub processing_duration: std::time::Duration,

    // TODO: candidates, hot pixel count, etc. from StarGate (which we run
    // alongside the focusing logic)
}
