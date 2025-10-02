// Copyright (c) 2024 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use cedar_camera::abstract_camera::{AbstractCamera, CapturedImage};

use std::cmp::max;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use image::{GenericImageView, GrayImage};
use imageproc::rect::Rect;
use log::{debug, error};
use cedar_detect::algorithm::{StarDescription, estimate_noise_from_image,
                              get_stars_from_image, summarize_region_of_interest};
use cedar_detect::histogram_funcs::{average_top_values,
                                    get_level_for_fraction,
                                    remove_stars_from_histogram,
                                    stats_for_histogram};
use cedar_detect::image_funcs::bin_and_histogram_2x2;
use cedar_elements::image_utils::{
    normalize_rows_mut, scale_image_mut};
use cedar_elements::value_stats::ValueStatsAccumulator;
use cedar_elements::cedar::ValueStats;

pub struct DetectEngine {
    // Initial exposure duration, prior to doing any calibrations. Setup mode
    // auto-exposure uses this as its baseline.
    initial_exposure_duration: Duration,

    // Bounds the range of exposure durations to be set by auto-exposure.
    min_exposure_duration: Duration,
    max_exposure_duration: Duration,

    // Parameters for star detection algorithm.
    detection_min_sigma: f64,
    detection_sigma: f64,

    // In align mode and operate mode (`focus_mode` is false), the
    // auto-exposure algorithm uses this as the desired number of detected
    // stars. The algorithm allows the number of detected stars to vary around
    // the goal by a large amount to the high side (is OK to have more stars
    // than needed) but only a small amount to the low side.
    star_count_goal: i32,

    // Our state, shared between DetectEngine methods and the worker thread.
    state: Arc<tokio::sync::Mutex<DetectState>>,

    // Executes worker().
    worker_thread: Option<std::thread::JoinHandle<()>>,

    // Signaled at worker_thread exit.
    worker_done: Arc<AtomicBool>,
}

// State shared between worker thread and the DetectEngine methods.
struct DetectState {
    // Note: camera settings can be adjusted behind our back.
    camera: Arc<tokio::sync::Mutex<Box<dyn AbstractCamera + Send>>>,

    // Cedar server disables autoexposure during calibrations.
    autoexposure_enabled: bool,

    // Determines whether rows are normalized to have the same dark level.
    normalize_rows: bool,

    frame_id: Option<i32>,

    // True means populate `DetectResult.focus_aid` info.
    focus_mode: bool,

    // Affects auto-exposure and turns off focus aids and star detection.
    daylight_mode: bool,

    // User-designated focus point for daylight focus mode. In full resolution
    // image coordinates. None if no point has been designated.
    daylight_focus_point: Option<(f64, f64)>,

    // When running CedarDetect, this supplies the `binning` value used.
    // See "About Resolutions" in cedar_server.rs.
    binning: u32,

    // Together with 'binning', this is used to adjust central peak image size.
    display_sampling: bool,

    // When using auto exposure in operate mode or setup align mode, this is the
    // exposure duration determined (by calibration) to yield `star_count_goal`
    // detected stars. Auto exposure logic will only deviate from this by a
    // bounded amount. None if calibration result is not yet available because
    // we are still focusing.
    calibrated_exposure_duration: Option<Duration>,

    // If we have determined a good star detection exposure duration that yields
    // close to `star_count_goal`, remember it here. None if auto exposure did
    // not find a good value.
    auto_exposure_duration: Option<Duration>,

    // When auto-exposing for detected star count, if there are too many stars
    // we shorten the exposure. But we don't bother going shorter than the
    // camera's post-capture processing time.
    camera_processing_duration: Option<Duration>,

    // We update the exposure time based on the number of detected stars.
    // Because of noise, twinking, etc., use a moving average.
    star_count_moving_average: f64,

    acquire_latency_stats: ValueStatsAccumulator,
    detect_latency_stats: ValueStatsAccumulator,

    // Estimated time at which `detect_result` will next be updated.
    eta: Option<Instant>,

    detect_result: Option<DetectResult>,
}

impl DetectEngine {
    pub fn new(initial_exposure_duration: Duration,
               min_exposure_duration: Duration,
               max_exposure_duration: Duration,
               detection_min_sigma: f64,
               detection_sigma: f64,
               star_count_goal: i32,
               camera: Arc<tokio::sync::Mutex<Box<dyn AbstractCamera + Send>>>,
               normalize_rows: bool,
               stats_capacity: usize)
               -> Self {
        DetectEngine{
            initial_exposure_duration,
            min_exposure_duration,
            max_exposure_duration,
            detection_min_sigma,
            detection_sigma,
            star_count_goal,
            state: Arc::new(tokio::sync::Mutex::new(DetectState{
                camera: camera.clone(),
                autoexposure_enabled: true,
                normalize_rows,
                frame_id: None,
                focus_mode: false,
                daylight_mode: false,
                daylight_focus_point: None,
                binning: 1,
                display_sampling: false,
                calibrated_exposure_duration: None,
                auto_exposure_duration: None,
                camera_processing_duration: None,
                star_count_moving_average: 0.0,
                acquire_latency_stats: ValueStatsAccumulator::new(stats_capacity),
                detect_latency_stats: ValueStatsAccumulator::new(stats_capacity),
                eta: None,
                detect_result: None,
            })),
            worker_thread: None,
            worker_done: Arc::new(AtomicBool::new(false)),
        }
    }

    pub async fn replace_camera(
        &mut self, camera: Arc<tokio::sync::Mutex<Box<dyn AbstractCamera + Send>>>)
    {
        self.reset_session_stats().await;
        let mut locked_state = self.state.lock().await;
        locked_state.camera = camera.clone();
        locked_state.auto_exposure_duration = None;
        locked_state.camera_processing_duration = None;
        locked_state.star_count_moving_average = 0.0;
        locked_state.detect_result = None;
    }

    pub async fn set_autoexposure_enabled(&mut self, enabled: bool) {
        let mut locked_state = self.state.lock().await;
        locked_state.autoexposure_enabled = enabled;
        locked_state.auto_exposure_duration = None;
    }

    pub async fn set_binning(&mut self, binning: u32, display_sampling: bool) {
        let mut locked_state = self.state.lock().await;
        locked_state.binning = binning;
        locked_state.display_sampling = display_sampling;
        // Don't need to do anything, worker thread will pick up the change when
        // it finishes the current interval.
    }

    pub async fn set_focus_mode(&mut self, enabled: bool) {
        let mut locked_state = self.state.lock().await;
        locked_state.focus_mode = enabled;
        if !enabled {
            locked_state.star_count_moving_average = 0.0;
        }
        // Don't need to do anything, worker thread will pick up the change when
        // it finishes the current interval.
    }

    pub async fn set_daylight_mode(&mut self, enabled: bool) {
        let mut locked_state = self.state.lock().await;
        locked_state.daylight_mode = enabled;
        if !enabled {
            locked_state.star_count_moving_average = 0.0;
        }
        locked_state.auto_exposure_duration = None;
        // Don't need to do anything, worker thread will pick up the change when
        // it finishes the current interval.
    }

    pub async fn set_daylight_focus_point(&mut self, point: (f64, f64)) {
        let mut locked_state = self.state.lock().await;
        locked_state.daylight_focus_point = Some(point);
    }

    pub fn get_detection_sigma(&self) -> f64 {
        self.detection_sigma
    }

    pub fn get_star_count_goal(&self) -> i32 {
        self.star_count_goal
    }

    pub async fn set_calibrated_exposure_duration(
        &mut self, calibrated_exposure_duration: Option<Duration>) {
        let mut locked_state = self.state.lock().await;
        locked_state.calibrated_exposure_duration = calibrated_exposure_duration;
        // Don't need to do anything, worker thread will pick up the change when
        // it finishes the current interval.
    }

    fn update_star_count_moving_average(state: &mut DetectState,
                                        num_stars_detected: usize) -> f64 {
        // First time?
        if state.star_count_moving_average == 0.0 {
            state.star_count_moving_average = num_stars_detected as f64;
        } else {
            // Alpha near 1.0: current value dominates. Alpha near 0.0: long
            // term average dominates.
            let alpha = 0.5;
            state.star_count_moving_average = alpha * num_stars_detected as f64 +
                (1.0 - alpha) * state.star_count_moving_average;
        }
        state.star_count_moving_average
    }

    /// Obtains a result bundle, as configured above. The returned result is
    /// "fresh" in that we either wait to process a new exposure or return the
    /// result of processing the most recently completed exposure.
    /// This function does not "consume" the information that it returns;
    /// multiple callers will receive the current result bundle (or next result,
    /// if there is not yet a current result) if `prev_frame_id` is omitted. If
    /// `prev_frame_id` is supplied, the call blocks while the current result
    /// has the same id value.
    /// Returns: the processed result along with its frame_id value. Returns
    ///     None if non_blocking and a suitable result is not yet available.
    pub async fn get_next_result(&mut self, prev_frame_id: Option<i32>,
                                 non_blocking: bool) -> Option<DetectResult> {
        // Has the worker terminated for some reason?
        if self.worker_done.load(Ordering::Relaxed) {
            self.worker_done.store(false, Ordering::Relaxed);
            self.worker_thread = None;
        }
        // Start worker thread if terminated or not yet started.
        if self.worker_thread.is_none() {
            let initial_exposure_duration = self.initial_exposure_duration;
            let min_exposure_duration = self.min_exposure_duration;
            let max_exposure_duration = self.max_exposure_duration;
            let detection_min_sigma = self.detection_min_sigma;
            let detection_sigma = self.detection_sigma;
            let star_count_goal = self.star_count_goal;
            let cloned_state = self.state.clone();
            let cloned_done = self.worker_done.clone();

            // The DetectEngine::worker() function is async because it uses the
            // camera interface, which is async. Note however that worker()
            // logic calls the non-async get_stars_from_image() function, which
            // takes ~10ms in release builds and ~200ms in debug builds. Such
            // compute durations are well beyond the guidelines for running
            // async code without an .await yield point.
            //
            // We thus run DetectEngine::worker() on its own async runtime. See
            // https://thenewstack.io/using-rustlangs-async-tokio-runtime-for-cpu-bound-tasks/
            self.worker_thread = Some(std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .thread_name("detect_engine")
                    .build().unwrap();
                runtime.block_on(async move {
                    DetectEngine::worker(
                        initial_exposure_duration,
                        min_exposure_duration, max_exposure_duration,
                        detection_min_sigma, detection_sigma,
                        star_count_goal, cloned_state, cloned_done).await;
                });
            }));
        }
        // Get the most recently posted result; wait if there is none yet or the
        // currently posted result is the same as the one the caller has already
        // obtained.
        loop {
            let mut sleep_duration = Duration::from_millis(1);
            {
                let locked_state = self.state.lock().await;
                if locked_state.detect_result.is_some() &&
                    (prev_frame_id.is_none() ||
                     prev_frame_id.unwrap() !=
                     locked_state.detect_result.as_ref().unwrap().frame_id)
                {
                    // Don't consume it, other clients may want it.
                    return Some(locked_state.detect_result.clone().unwrap());
                }
                if non_blocking {
                    return None;
                }
                if locked_state.eta.is_some() {
                    let time_to_eta =
                        locked_state.eta.unwrap().saturating_duration_since(Instant::now());
                    if time_to_eta > sleep_duration {
                        sleep_duration = time_to_eta;
                    }
                }
            }
            tokio::time::sleep(sleep_duration).await;
        }
    }

    pub async fn reset_session_stats(&mut self) {
        let mut state = self.state.lock().await;
        state.acquire_latency_stats.reset_session();
        state.detect_latency_stats.reset_session();
    }

    pub async fn estimate_delay(&self, prev_frame_id: Option<i32>) -> Option<Duration> {
        let locked_state = self.state.lock().await;
        if locked_state.detect_result.is_some() &&
            (prev_frame_id.is_none() ||
             prev_frame_id.unwrap() !=
             locked_state.detect_result.as_ref().unwrap().frame_id)
        {
            Some(Duration::ZERO)
        } else if locked_state.eta.is_some() {
            Some(locked_state.eta.unwrap().saturating_duration_since(Instant::now()))
        } else {
            None
        }
    }

    async fn worker(initial_exposure_duration: Duration,
                    min_exposure_duration: Duration,
                    max_exposure_duration: Duration,
                    detection_min_sigma: f64,
                    detection_sigma: f64,
                    star_count_goal: i32,
                    state: Arc<tokio::sync::Mutex<DetectState>>,
                    done: Arc<AtomicBool>) {
        debug!("Starting detect engine");
        loop {
            let camera: Arc<tokio::sync::Mutex<Box<dyn AbstractCamera + Send>>>;
            let normalize_rows;
            let focus_mode: bool;
            let daylight_mode: bool;
            let daylight_focus_point: Option<(f64, f64)>;
            let binning: u32;
            let display_sampling: bool;
            let calibrated_exposure_duration: Option<Duration>;
            let auto_exposure_duration: Option<Duration>;
            {
                let mut locked_state = state.lock().await;
                camera = locked_state.camera.clone();
                normalize_rows = locked_state.normalize_rows;
                focus_mode = locked_state.focus_mode;
                daylight_mode = locked_state.daylight_mode;
                daylight_focus_point = locked_state.daylight_focus_point;
                binning = locked_state.binning;
                display_sampling = locked_state.display_sampling;
                calibrated_exposure_duration =
                    locked_state.calibrated_exposure_duration;
                auto_exposure_duration =
                    locked_state.auto_exposure_duration;
                locked_state.eta = None;
            }

            let captured_image;
            let camera_processing_duration;
            {
                let frame_id = state.lock().await.frame_id;
                let delay_est = camera.lock().await.estimate_delay(frame_id);
                if let Some(delay_est) = delay_est {
                    state.lock().await.eta = Some(Instant::now() + delay_est);
                }
                // Don't hold camera lock for the entirety of the time waiting for
                // the next image.
                loop {
                    let capture =
                        match camera.lock().await.try_capture_image(frame_id).await
                    {
                        Ok(c) => c,
                        Err(e) => {
                            error!("Error capturing image: {}", &e.to_string());
                            // TODO: advertise camera status somewhere.
                            None  // Keep going.
                        }
                    };
                    if capture.is_none() {
                        let short_delay = Duration::from_millis(1);
                        let delay_est = camera.lock().await.estimate_delay(frame_id);
                        if let Some(delay_est) = delay_est {
                            tokio::time::sleep(max(delay_est, short_delay)).await;
                        } else {
                            tokio::time::sleep(short_delay).await;
                        }
                        continue;
                    }
                    let (image, id) = capture.unwrap();
                    captured_image = image;
                    let mut locked_state = state.lock().await;
                    locked_state.frame_id = Some(id);
                    if locked_state.camera_processing_duration.is_none() {
                        locked_state.camera_processing_duration =
                            captured_image.processing_duration;
                    }
                    camera_processing_duration =
                        locked_state.camera_processing_duration;
                    break;
                }
            }

            // Process the just-acquired image.
            let process_start_time = Instant::now();
            let image: &GrayImage = &captured_image.image;
            let (width, height) = image.dimensions();

            let image_region = Rect::at(0, 0).of_size(width, height);

            // To avoid edge when centroiding and creating central peak image,
            // inset the ROI a little.
            let adjusted_binning = binning * if display_sampling { 2 } else { 1 };
            let inset = 8 * adjusted_binning as i32;

            let noise_estimate = estimate_noise_from_image(image);
            let prev_exposure_duration_secs =
                captured_image.capture_params.exposure_duration.as_secs_f64();

            let mut acquire_duration_secs = prev_exposure_duration_secs;
            if let Some(cpd) = camera_processing_duration {
              acquire_duration_secs =
                f64::max(acquire_duration_secs, cpd.as_secs_f64());
            }
            state.lock().await.acquire_latency_stats.add_value(
                acquire_duration_secs);

            let mut new_exposure_duration_secs = prev_exposure_duration_secs;

            let mut focus_aid: Option<FocusAid> = None;
            let mut contrast_ratio: Option<f64> = None;
            // black_level and peak_value for display stretching.
            let mut black_level = 0_u8;
            let mut peak_value = 0_u8;
            if focus_mode || daylight_mode {
                // Region of interest is the central square crop of the image.
                let square_roi_size = height;
                let roi_region = Rect::at(
                    ((width - square_roi_size) / 2) as i32 + inset, inset)
                    .of_size(square_roi_size - 2 * inset as u32,
                             square_roi_size - 2 * inset as u32);

                let roi_summary = summarize_region_of_interest(
                    image, &roi_region, noise_estimate, detection_sigma);
                let roi_histogram = roi_summary.histogram;
                peak_value = max(get_level_for_fraction(&roi_histogram, 0.999) as u8, 1);
                black_level =
                    if daylight_mode {
                        get_level_for_fraction(&roi_histogram, 0.001) as u8
                    } else {
                        get_level_for_fraction(&roi_histogram, 0.8) as u8
                    };

                // For contrast ratio, use a smaller central crop. Do a 2x2
                // binning in case we have a color image.
                let contrast_region_size = height / 8;
                let contrast_region = Rect::at(
                    ((width - contrast_region_size) / 2) as i32,
                    ((height - contrast_region_size) / 2) as i32)
                    .of_size(contrast_region_size as u32,
                             contrast_region_size as u32);
                let contrast_image = image.view(
                    contrast_region.left() as u32,
                    contrast_region.top() as u32,
                    contrast_region.width() as u32,
                    contrast_region.height() as u32).to_image();
                let contrast_region_histogram =
                    bin_and_histogram_2x2(&contrast_image,
                                          /*normalize_rows=*/false).histogram;
                let contrast_peak_value =
                    max(get_level_for_fraction(&contrast_region_histogram, 0.99) as u8, 1);
                let contrast_black_level = if daylight_mode {
                    get_level_for_fraction(&contrast_region_histogram, 0.01) as u8
                } else{
                    get_level_for_fraction(&contrast_region_histogram, 0.6) as u8
                };
                contrast_ratio = Some(
                    (contrast_peak_value - contrast_black_level) as f64
                        / contrast_peak_value as f64);

                // Auto exposure.

                let correction_factor: f64;
                let stats = stats_for_histogram(&roi_histogram);
                if daylight_mode {
                    let bright_value =
                        max(get_level_for_fraction(&roi_histogram, 0.9) as u8, 1);

                    // Push bright part of image towards upper mid-level.
                    correction_factor = if bright_value > 250 {
                        // If we're saturated, knock back exposure time quickly.
                        0.1
                    } else {
                        220.0 / bright_value as f64
                    };
                } else {
                    // Auto exposure in focus mode.
                    let dark_level_cap = 32.0;

                    // Way overexposed?
                    if stats.mean > 250.0 {
                        // Knock back exposure time quickly.
                        correction_factor = 0.05;
                    } else if stats.mean > dark_level_cap {
                        // In twilight or heavily light polluted situations (or with
                        // moonlight), control the average scene exposure to avoid
                        // whiting out the screen.
                        correction_factor = dark_level_cap / stats.mean;
                    } else {
                        // Overall scene is below dark_level_cap. Set a target
                        // value of the pixels in the detected peak region.
                        // Note that a lower brightness_goal value allows for
                        // faster exposures, which is nice in focus mode.
                        let peak_region_val = f64::max(roi_summary.peak_value, 1.0);
                        let brightness_goal = 64.0;
                        correction_factor = brightness_goal / peak_region_val;
                    }
                }

                // Don't adjust exposure time too often.
                if correction_factor < 0.7 || correction_factor > 1.3 {
                    new_exposure_duration_secs =
                        prev_exposure_duration_secs * correction_factor;
                }

                if !daylight_mode {
                    // Get a small sub-image centered on the peak coordinates.
                    let peak_position = (roi_summary.peak_x, roi_summary.peak_y);
                    debug!("peak at x/y {}/{}", peak_position.0, peak_position.1);
                    let sub_image_size = 15 * adjusted_binning as i32;
                    assert!(sub_image_size < 2 * inset);
                    let peak_region =
                        Rect::at((peak_position.0 as i32 - sub_image_size/2) as i32,
                                 (peak_position.1 as i32 - sub_image_size/2) as i32)
                        .of_size(sub_image_size as u32, sub_image_size as u32);
                    let peak_region = peak_region.intersect(image_region).unwrap();
                    let mut peak_image = image.view(peak_region.left() as u32,
                                                    peak_region.top() as u32,
                                                    peak_region.width() as u32,
                                                    peak_region.height() as u32).to_image();
                    if normalize_rows {
                        normalize_rows_mut(&mut peak_image);
                    }

                    // Find min/max for display stretching.
                    let mut histogram: [u32; 256] = [0_u32; 256];
                    for pixel_value in peak_image.pixels() {
                        histogram[pixel_value.0[0] as usize] += 1;
                    }
                    // Compute peak_value as the average of the 5 brightest pixels.
                    let max_value = average_top_values(&histogram, 5);

                    remove_stars_from_histogram(&mut histogram, /*sigma=*/8.0);
                    let min_value = get_level_for_fraction(&histogram, 0.95);

                    scale_image_mut(
                        &mut peak_image, min_value as u8, max_value, /*gamma=*/0.7);
                    focus_aid = Some(FocusAid{
                        center_peak_position: Some(peak_position),
                        center_peak_value: Some(peak_value),
                        peak_image: Some(peak_image),
                        peak_image_region: Some(peak_region),
                        daylight_focus_zoom_image: None,
                        daylight_focus_zoom_region: None,
                    });
                } else {
                    // Generate daylight focus zoom image
                    let focus_point = daylight_focus_point.unwrap_or_else(|| {
                        // Default to roi_region center if no point designated.
                        (roi_region.left() as f64 + roi_region.width() as f64 / 2.0,
                         roi_region.top() as f64 + roi_region.height() as f64 / 2.0)
                    });

                    // Calculate region size similar to existing focus logic.
                    let sub_image_size = 30 * adjusted_binning as i32;
                    let half_size = sub_image_size / 2;

                    // Create region centered on focus point, bounded by roi_region.
                    let desired_left = focus_point.0 as i32 - half_size;
                    let desired_top = focus_point.1 as i32 - half_size;

                    // Bound the region within roi_region
                    let bounded_left = desired_left.max(roi_region.left())
                        .min(roi_region.right() - sub_image_size);
                    let bounded_top = desired_top.max(roi_region.top())
                        .min(roi_region.bottom() - sub_image_size);

                    let daylight_focus_region = Rect::at(bounded_left, bounded_top)
                        .of_size(sub_image_size as u32, sub_image_size as u32)
                        .intersect(roi_region).unwrap();

                    let mut daylight_focus_image = image.view(
                        daylight_focus_region.left() as u32,
                        daylight_focus_region.top() as u32,
                        daylight_focus_region.width() as u32,
                        daylight_focus_region.height() as u32).to_image();

                    // Calculate display stretching values specific to the focus
                    // region. Use the original image with the daylight_focus_region
                    // coordinates to avoid margin issues.
                    let focus_summary = summarize_region_of_interest(
                        image, &daylight_focus_region, noise_estimate, detection_sigma);
                    let focus_histogram = focus_summary.histogram;
                    let focus_peak_value =
                        max(get_level_for_fraction(&focus_histogram, 0.999) as u8, 1);
                    let focus_black_level =
                        get_level_for_fraction(&focus_histogram, 0.001) as u8;

                    // Apply display stretching using focus region-specific values.
                    scale_image_mut(&mut daylight_focus_image, focus_black_level, focus_peak_value, /*gamma=*/0.7);
                    focus_aid = Some(FocusAid{
                        center_peak_position: None,
                        center_peak_value: None,
                        peak_image: None,
                        peak_image_region: None,
                        daylight_focus_zoom_image: Some(daylight_focus_image),
                        daylight_focus_zoom_region: Some(daylight_focus_region),
                    });
                }
            }  // focus_mode || daylight_mode

            let mut binned_image: Option<Arc<GrayImage>> = None;
            let mut stars: Vec<StarDescription> = vec![];
            let mut hot_pixel_count = 0;

            // If the captured_image is up to date w.r.t. the camera settings,
            // we can use it to influence our new exposure.
            let mut update_exposure = captured_image.params_accurate;

            if !daylight_mode && !focus_mode {
                // Run CedarDetect on the image.
                {
                    let mut locked_state = state.lock().await;
                    if let Some(recent_stats) =
                        &locked_state.detect_latency_stats.value_stats.recent
                    {
                        let detect_duration = Duration::from_secs_f64(recent_stats.min);
                        locked_state.eta = Some(Instant::now() + detect_duration);
                    }
                }
                let adjusted_sigma = f64::max(detection_sigma, detection_min_sigma);
                let detect_binned_image;
                let mut histogram;
                (stars, hot_pixel_count, detect_binned_image, histogram) =
                    get_stars_from_image(
                        image, noise_estimate, adjusted_sigma,
                        normalize_rows, binning,
                        /*detect_hot_pixels=*/true,
                        /*return_binned_image=*/binning != 1);
                let stats = stats_for_histogram(&histogram);
                binned_image = detect_binned_image.map(Arc::new);

                // Average the peak pixels of the N brightest stars.
                let mut sum_peak: i32 = 0;
                let mut num_peak = 0;
                const NUM_PEAKS: i32 = 10;
                for star in &stars {
                    sum_peak += star.peak_value as i32;
                    num_peak += 1;
                    if num_peak >= NUM_PEAKS {
                        break;
                    }
                }
                peak_value =
                    if num_peak == 0 {
                        // No stars detected; set peak_value according to histogram.
                        let top_value = average_top_values(&histogram, 5);
                        // Choose value a quarter of the way from top_value to 255.
                        let span = 255 - top_value;
                        top_value + span / 4
                    } else {
                        (sum_peak / num_peak) as u8
                    };

                // Get a good black level for display.
                remove_stars_from_histogram(&mut histogram, /*sigma=*/8.0);
                // Put the black level near the top of the non-star background,
                // so we don't display too much of the noise floor.
                black_level = get_level_for_fraction(&histogram, 0.98) as u8;

                // Because we're determining peak_value from detected stars,
                // in pathological situations the black_level might end up
                // higher than the peak_value. Kludge this back to sanity.
                if black_level > peak_value {
                    black_level = peak_value;
                }

                // Auto exposure.
                let baseline_exposure_duration =
                    match calibrated_exposure_duration
                {
                    Some(ced) => ced,
                    None => initial_exposure_duration,
                };
                let baseline_exposure_duration_secs =
                    baseline_exposure_duration.as_secs_f64();
                let fallback_exposure_duration_secs = if let Some(d) = auto_exposure_duration {
                    d.as_secs_f64()
                } else {
                    baseline_exposure_duration_secs
                };

                let num_stars_detected = stars.len();
                if num_stars_detected < 4 {
                    // We're likely slewing and thus detecting no stars.
                    // Don't update the moving average, and for safety use
                    // a known-good exposure duration.
                    new_exposure_duration_secs = fallback_exposure_duration_secs;
                    // Force update even if image is catching up to camera settings.
                    update_exposure = true;
                } else if captured_image.params_accurate {
                    let moving_average = {
                        let mut locked_state = state.lock().await;
                        Self::update_star_count_moving_average(
                            &mut locked_state, num_stars_detected)
                    };
                    if moving_average < 4.0 {
                        // This shouldn't happen because we don't update the moving
                        // average with num_stars_detected<4. But just in case do
                        // something sane.
                        new_exposure_duration_secs = fallback_exposure_duration_secs;
                    } else {
                        // When increasing exposure to increase star count,
                        // don't exceed a brightness limit. Note: this should be
                        // the same value as in
                        // Calibrator::calibrate_exposure_duration().
                        const BRIGHTNESS_LIMIT: u8 = 192;
                        // >1 if we have more stars than goal; <1 if fewer stars than
                        // goal.
                        let star_goal_fraction =
                            moving_average / star_count_goal as f64;
                        if star_goal_fraction < 1.0 &&
                            stats.mean as u8 > BRIGHTNESS_LIMIT
                        {
                            new_exposure_duration_secs = fallback_exposure_duration_secs;
                        } else {
                            // Don't adjust exposure time too often. Allow
                            // number of detected stars to exceed goal, but
                            // don't allow much of a shortfall.
                            if star_goal_fraction < 0.8 || star_goal_fraction > 1.6 {
                                // What is the relationship between exposure
                                // time and number of stars detected?
                                // * If we increase the exposure time by 2.5x,
                                //   we'll be able to detect stars 40% as
                                //   bright. This corresponds to an increase of
                                //   one stellar magnitude.
                                // * Per https://www.hnsky.org/star_count, at
                                //   mag=5 a one magnitude increase corresponds
                                //   to around 3x the number of stars.
                                // * 2.5x and 3x are "close enough", so we model
                                // the number of detectable stars as being
                                // simply proportional to the exposure time.
                                // This is OK because we'll only be varying the
                                // exposure time a modest amount relative to the
                                // baseline_exposure_duration.
                                new_exposure_duration_secs =
                                    prev_exposure_duration_secs / star_goal_fraction;
                                if calibrated_exposure_duration.is_some() {
                                    // Bound exposure duration to be within three
                                    // stops of calibrated_exposure_duration. Further
                                    // bounds are applied below.
                                    new_exposure_duration_secs = f64::max(
                                        new_exposure_duration_secs,
                                        baseline_exposure_duration_secs / 8.0);
                                    new_exposure_duration_secs = f64::min(
                                        new_exposure_duration_secs,
                                        baseline_exposure_duration_secs * 8.0);
                                }
                                // Don't make camera exposure shorter than the
                                // camera's post-readout processing time.
                                if let Some(cpd) = camera_processing_duration {
                                    if new_exposure_duration_secs < cpd.as_secs_f64() {
                                        new_exposure_duration_secs = cpd.as_secs_f64();
                                    }
                                }
                            } else {
                                // Auto exposure time is good. Remember it for
                                // use as a fallback.
                                state.lock().await.auto_exposure_duration =
                                    Some(Duration::from_secs_f64(
                                        prev_exposure_duration_secs));
                            }
                        }
                    }
                }
            }  // !daylight_mode && !focus_mode
            let elapsed = process_start_time.elapsed();
            state.lock().await.detect_latency_stats.add_value(elapsed.as_secs_f64());

            // Update camera exposure time if auto-exposure calls for an
            // adjustment.
            // Bound auto-exposure duration to given limits.
            new_exposure_duration_secs = f64::max(new_exposure_duration_secs,
                                                  min_exposure_duration.as_secs_f64());
            new_exposure_duration_secs = f64::min(new_exposure_duration_secs,
                                                  max_exposure_duration.as_secs_f64());
            if update_exposure && state.lock().await.autoexposure_enabled &&
                prev_exposure_duration_secs != new_exposure_duration_secs
            {
                debug!("Setting new exposure duration {}s",
                       new_exposure_duration_secs);
                let result = camera.lock().await.set_exposure_duration(
                    Duration::from_secs_f64(new_exposure_duration_secs));
                match result {
                    Ok(()) => (),
                    Err(e) => {
                        error!("Error updating exposure duration: {}",
                               &e.to_string());
                        done.store(true, Ordering::Relaxed);
                        return;  // Abandon thread execution!
                    }
                }
            }

            // Post the result.
            let mut locked_state = state.lock().await;
            locked_state.detect_result = Some(DetectResult{
                frame_id: locked_state.frame_id.unwrap(),
                captured_image,
                binned_image,
                star_candidates: stars,
                star_count_moving_average: locked_state.star_count_moving_average,
                display_black_level: black_level,
                contrast_ratio,
                noise_estimate,
                hot_pixel_count,
                peak_value,
                focus_aid,
                daylight_mode,
                processing_duration: elapsed,
                acquire_latency_stats:
                  locked_state.acquire_latency_stats.value_stats.clone(),
                detect_latency_stats:
                  locked_state.detect_latency_stats.value_stats.clone(),
            });
        }  // loop.
    }
}

#[derive(Clone)]
pub struct DetectResult {
    // See the corresponding field in cedar.FrameResult proto message.
    pub frame_id: i32,

    // The full-resolution camera image used to produce the information in this
    // detect result.
    pub captured_image: CapturedImage,

    // If binning was applied prior to detect, this is the 2x2 or 4x4 binned
    // (and hot pixel removed) image.
    pub binned_image: Option<Arc<GrayImage>>,

    // The star candidates detected by CedarDetect; ordered by highest
    // StarDescription.mean_brightness first.
    pub star_candidates: Vec<StarDescription>,

    // The number of detected stars as a moving average of recent processing
    // cycles.
    pub star_count_moving_average: f64,

    // When displaying the captured (or binned) image, map this pixel value to
    // black. This is chosen to allow stars to be visible but supress the
    // background level.
    pub display_black_level: u8,

    // A measure of the image contrast in focus mode. 0 means no contrast,
    // uniform brightness over image. 1 means high contrast (range of bright -
    // dark equals bright level; in other words dark == 0). Omitted if not
    // focus mode.
    pub contrast_ratio: Option<f64>,

    // Estimate of the RMS noise of the full-resolution image.
    pub noise_estimate: f64,

    // The number of hot pixels detected by CedarDetect.
    pub hot_pixel_count: i32,

    // The peak pixel value of star_candidates. If star_candidates is empty,
    // this value is fixed to 255. If daylight_mode, this is the value of the
    // brightest part of the central regioin.
    pub peak_value: u8,

    // Included if `focus_mode`.
    pub focus_aid: Option<FocusAid>,

    // Indicates whether daylight_mode was in effect for this result.
    pub daylight_mode: bool,

    // Time taken to produce this DetectResult, excluding the time taken to
    // acquire the image.
    pub processing_duration: std::time::Duration,

    // How much time (in seconds) is spent acquiring the image. This is the max
    // of the camera exposure time and the (pipelined) time taken to convert the
    // pixel format.
    pub acquire_latency_stats: ValueStats,

    // Distribution of `processing_duration` values.
    pub detect_latency_stats: ValueStats,
}

#[derive(Clone)]
pub struct FocusAid {
    // See the corresponding field in FrameResult. Only present in non-daylight focus mode.
    pub center_peak_position: Option<(f64, f64)>,

    // See the corresponding field in FrameResult. Only present in non-daylight focus mode.
    pub center_peak_value: Option<u8>,

    // A small crop of `captured_image` centered at `center_peak_position`.
    // Brightness scaled to full range for visibility. Only present in non-daylight focus mode.
    pub peak_image: Option<GrayImage>,

    // The location of `peak_image`. Only present in non-daylight focus mode.
    pub peak_image_region: Option<Rect>,

    // A small crop of `captured_image` centered at user-designated position
    // for daylight focus mode. Only present in daylight mode.
    pub daylight_focus_zoom_image: Option<GrayImage>,

    // The location of `daylight_focus_zoom_image`. Only present in daylight mode.
    pub daylight_focus_zoom_region: Option<Rect>,
}
