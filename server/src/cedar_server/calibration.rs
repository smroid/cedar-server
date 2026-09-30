// Copyright (c) 2026 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use std::{
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use canonical_error::{aborted_error, CanonicalError, CanonicalErrorCode};
use cedar_camera::abstract_camera::{AbstractCamera, Gain, Offset};
use cedar_elements::cedar::{CalibrationFailureReason, OperatingMode};
use log::{debug, info, warn};

use super::{
    cedar_rpcs::tonic_status, CedarState, MyCedar,
    FOCUS_ASSIST_UPDATE_INTERVAL,
};
use crate::calibrator::ExposureCalibrationError;

// Retry interval for skip-focus auto-calibration attempts.
const SKIP_FOCUS_RETRY_INTERVAL_SECS: u64 = 15;

impl MyCedar {
    // See "About Resolutions" below.
    // Computes (detect_binning, display_sampling) for camera, taking optional
    // command line overrides into account.
    // Returns:
    // detect_binning: u32; whether (and how much) the acquired image is binned
    //     prior to CedarDetect. Note: when detect_binning >= 2, the display
    //     image sent to the UI is always 2x binned regardless of
    //     detect_binning.
    // display_sampling: bool; whether (possibly binned) image is to be further
    //     2x downsampled when sending to the UI.
    pub(super) fn compute_binning(
        state: &CedarState,
        width: u32,
        height: u32,
    ) -> (u32, bool) {
        let args_binning = state.args_binning;
        let args_display_sampling = state.args_display_sampling;
        // Use sensor dimensions to determine total binning needed.
        let mpix = (width * height) as f64 / 1000000.0;
        let mut total_binning = 1_u32;
        let mut display_sampling = false;
        if mpix <= 0.75 {
            // Use initial values.
        } else if mpix <= 3.0 {
            total_binning = 2;
        } else if mpix <= 12.0 {
            total_binning = 4;
        } else {
            total_binning = 4;
            display_sampling = true;
        }
        // Allow command-line overrides of sampling/binning parameters.
        if let Some(ba) = args_binning {
            total_binning = ba;
        }
        if let Some(dsa) = args_display_sampling {
            display_sampling = dsa;
        }
        debug!(
            "For {:.1}mpix, total_binning {}, display_sampling {}",
            mpix, total_binning, display_sampling
        );
        (total_binning, display_sampling)
    }

    pub(super) async fn camera_geometry(
        camera: &Arc<tokio::sync::Mutex<Box<dyn AbstractCamera + Send>>>,
    ) -> (u32, u32) {
        let locked = camera.lock().await;
        locked.dimensions().await
    }

    /// Atomically checks that calibration is not already in progress, snapshots
    /// the frozen image for display during calibration, and sets
    /// `calibrating = true`. Returns true if calibration was started, false if
    /// it was already in progress.
    pub(super) async fn begin_calibration(
        state: &Arc<tokio::sync::Mutex<CedarState>>,
        duration_estimate: Duration,
    ) -> bool {
        let mut locked_state = state.lock().await;
        if locked_state.calibrating {
            return false;
        }
        // Snapshot the last serve result so we can show a frozen image during
        // calibration (ServeEngine results are not usable while calibration is
        // running). Locking serve_engine while holding the state lock is safe
        // because ServeEngine never acquires the CedarState lock; the lock
        // ordering is always CedarState -> ServeEngine, never the reverse.
        let serve_engine_arc = locked_state.serve_engine.clone();
        if let Some(sr) = serve_engine_arc
            .lock()
            .await
            .get_next_result(None, None, /* non_blocking= */ true)
            .await
        {
            locked_state.scaled_image = sr.scaled_image;
            locked_state.scaled_image_binning_factor =
                sr.scaled_image_binning_factor;
            locked_state.scaled_image_frame_id = sr.scaled_image_frame_id;
            // Keep the rectangle that describes these pixels, so we don't have
            // to fabricate one when serving the frozen image.
            locked_state.scaled_image_rectangle = sr
                .frame_result
                .image
                .as_ref()
                .and_then(|i| i.rectangle.clone());
        }
        locked_state.calibrating = true;
        locked_state.calibration_start = Instant::now();
        locked_state.calibration_duration_estimate = duration_estimate;
        true
    }

    /// Runs a single calibration attempt. Returns true if calibration
    /// succeeded. Handles all setup (gain, autoexposure, timing) and
    /// cleanup (autoexposure restore). Does NOT handle mode transitions on
    /// success/failure - caller is responsible.
    pub(super) async fn run_calibration_attempt(
        state: Arc<tokio::sync::Mutex<CedarState>>,
    ) -> bool {
        // Setup phase: query solver timeout before begin_calibration so the
        // duration estimate is accurate.
        let (solver_arc, calibration_data_arc, detect_engine_arc) = {
            let locked_state = state.lock().await;
            (
                locked_state.solver.clone(),
                locked_state.calibration_data.clone(),
                locked_state.detect_engine.clone(),
            )
        }; // State lock released here!

        let calibration_solve_timeout =
            solver_arc.lock().await.default_timeout();
        let camera = state.lock().await.camera.clone();
        MyCedar::set_gain(&camera, /* daylight_mode= */ false).await;

        if !MyCedar::begin_calibration(
            &state,
            Duration::from_secs(5) + calibration_solve_timeout,
        )
        .await
        {
            return false; // Already in flight.
        }

        calibration_data_arc.lock().await.calibration_time =
            Some(prost_types::Timestamp::from(SystemTime::now()));
        detect_engine_arc
            .lock()
            .await
            .set_autoexposure_enabled(false)
            .await;

        // Execute phase.
        let succeeded = match MyCedar::calibrate(state.clone()).await {
            Ok(s) => s,
            Err(e) => {
                // The only error we expect is Aborted.
                if e.code != CanonicalErrorCode::Aborted {
                    warn!("Unexpected calibration error: {:?}", e);
                }
                false
            }
        };

        // Cleanup phase.
        let cancel_calibration_arc = {
            let mut locked_state = state.lock().await;
            locked_state.calibrating = false;
            locked_state.cancel_calibration.clone()
        }; // State lock released here!

        detect_engine_arc
            .lock()
            .await
            .set_autoexposure_enabled(true)
            .await;

        // Check if cancelled.
        let cancelled = *cancel_calibration_arc.lock().await;
        if cancelled {
            *cancel_calibration_arc.lock().await = false;
        }

        succeeded && !cancelled
    }

    // When we leave SETUP (with focus mode), either because focus mode is
    // turned off (transitioning to SETUP align mode), or because of
    // transitioning to OPERATE, we need to calibrate.
    // When the calibration finishes, the `new_operate_mode`, `new_focus_mode`,
    // and `new_daylight_mode` args are used to set the post-calibration
    // operating mode. If the calibration is aborted, the current operating mode
    // is retained.
    pub(super) fn spawn_calibration(
        state: Arc<tokio::sync::Mutex<CedarState>>,
        new_operate_mode: bool,
        new_focus_mode: bool,
        new_daylight_mode: bool,
    ) {
        // The calibrate() call can take several seconds. If the gRPC client
        // aborts the RPC (e.g. due to timeout), we want the calibration and
        // state updates (i.e. detect engine's focus_mode, our operating_mode)
        // to be completed properly.
        //
        // The spawned task runs to completion even if the RPC handler task
        // aborts.
        //
        // Note that below we return immediately rather than joining the
        // task_handle. We arrange for get_frame() to return a FrameResult with
        // a information about the ongoing calibration.

        let _task_handle: tokio::task::JoinHandle<Result<(), tonic::Status>> =
            tokio::task::spawn(async move {
                let succeeded =
                    MyCedar::run_calibration_attempt(state.clone()).await;

                // Get state for mode transitions.
                let (
                    detect_engine_arc,
                    solve_engine_arc,
                    focus_mode,
                    daylight_mode,
                ) = {
                    let locked_state = state.lock().await;
                    (
                        locked_state.detect_engine.clone(),
                        locked_state.solve_engine.clone(),
                        locked_state
                            .operation_settings
                            .focus_assist_mode
                            .unwrap(),
                        locked_state.operation_settings.daylight_mode.unwrap(),
                    )
                };

                if !succeeded {
                    // Calibration failed or was cancelled. Stay in current
                    // mode.
                    let camera = state.lock().await.camera.clone();
                    MyCedar::set_gain(&camera, daylight_mode).await;
                    let mut locked_detect_engine =
                        detect_engine_arc.lock().await;
                    locked_detect_engine.set_focus_mode(focus_mode).await;
                    locked_detect_engine.set_daylight_mode(daylight_mode).await;
                } else {
                    // Calibration completed.
                    {
                        let camera = state.lock().await.camera.clone();
                        MyCedar::set_gain(&camera, new_daylight_mode).await;
                        let mut locked_state = state.lock().await;
                        locked_state.operation_settings.daylight_mode =
                            Some(new_daylight_mode);
                        locked_state.operation_settings.focus_assist_mode =
                            Some(new_focus_mode);
                    }

                    {
                        let mut detect_engine = detect_engine_arc.lock().await;
                        if new_operate_mode {
                            // Transition into Operate mode.
                            detect_engine.set_focus_mode(false).await;
                            detect_engine.set_daylight_mode(false).await;
                        } else {
                            detect_engine
                                .set_daylight_mode(new_daylight_mode)
                                .await;
                            detect_engine.set_focus_mode(new_focus_mode).await;
                        }
                    }

                    if new_operate_mode {
                        {
                            let mut solve_engine =
                                solve_engine_arc.lock().await;
                            solve_engine.set_align_mode(false).await;
                            solve_engine.start();
                        }
                        // Set automatic update interval for OPERATE mode.
                        {
                            let mut locked_state = state.lock().await;
                            let std_duration =
                                MyCedar::get_automatic_update_interval(
                                    &locked_state,
                                );
                            if let Err(x) = MyCedar::set_update_interval(
                                &locked_state,
                                std_duration,
                            )
                            .await
                            {
                                return Err(tonic_status(x));
                            }
                            locked_state.operation_settings.operating_mode =
                                Some(OperatingMode::Operate as i32);
                        }
                    }

                    // Notify serve engine of the new operation settings so it
                    // polls the correct upstream engine.
                    let (serve_engine_arc, updated_op_settings) = {
                        let locked_state = state.lock().await;
                        (
                            locked_state.serve_engine.clone(),
                            locked_state.operation_settings.clone(),
                        )
                    };
                    serve_engine_arc
                        .lock()
                        .await
                        .update_operation_settings(updated_op_settings)
                        .await;
                }
                Ok(())
            });
        // Let _task_handle go out of scope, detaching the spawned calibration
        // task to complete regardless of a possible RPC timeout.
    }

    /// Spawns an auto-calibration loop for skip-focus mode. Retries
    /// periodically until calibration succeeds or skip_focus_active is
    /// cleared.
    pub(super) fn spawn_skip_focus_calibration(
        state: Arc<tokio::sync::Mutex<CedarState>>,
        advance_to_operate: bool, // Based on skip_alignment preference.
    ) {
        tokio::task::spawn(async move {
            // Mark worker as running.
            {
                let mut locked_state = state.lock().await;
                locked_state.skip_focus_worker_running = true;
            }

            loop {
                // Record attempt time.
                {
                    let mut locked_state = state.lock().await;
                    locked_state.skip_focus_last_attempt = Some(Instant::now());
                }

                // Run calibration using shared helper.
                let succeeded =
                    MyCedar::run_calibration_attempt(state.clone()).await;

                if succeeded {
                    info!("Skip-focus calibration succeeded");
                    let mut locked_state = state.lock().await;
                    locked_state.skip_focus_active = false;

                    if advance_to_operate {
                        // Transition to OPERATE mode.
                        locked_state.operation_settings.operating_mode =
                            Some(OperatingMode::Operate as i32);
                        locked_state.operation_settings.focus_assist_mode =
                            Some(false);
                        locked_state.operation_settings.daylight_mode =
                            Some(false);

                        let detect_engine = locked_state.detect_engine.clone();
                        let solve_engine = locked_state.solve_engine.clone();
                        drop(locked_state);

                        detect_engine.lock().await.set_focus_mode(false).await;
                        detect_engine
                            .lock()
                            .await
                            .set_daylight_mode(false)
                            .await;
                        solve_engine.lock().await.set_align_mode(false).await;
                        solve_engine.lock().await.start();

                        let locked_state = state.lock().await;
                        let std_duration =
                            MyCedar::get_automatic_update_interval(
                                &locked_state,
                            );
                        MyCedar::set_update_interval(
                            &locked_state,
                            std_duration,
                        )
                        .await
                        .ok();
                    } else {
                        // Transition to SETUP alignment mode.
                        locked_state.operation_settings.focus_assist_mode =
                            Some(false);
                        locked_state.operation_settings.daylight_mode =
                            Some(false);

                        let detect_engine = locked_state.detect_engine.clone();
                        drop(locked_state);

                        detect_engine.lock().await.set_focus_mode(false).await;
                        detect_engine
                            .lock()
                            .await
                            .set_daylight_mode(false)
                            .await;
                    }
                    // Notify serve engine of the new operation settings.
                    let (serve_engine_arc, updated_op_settings) = {
                        let locked_state = state.lock().await;
                        (
                            locked_state.serve_engine.clone(),
                            locked_state.operation_settings.clone(),
                        )
                    };
                    serve_engine_arc
                        .lock()
                        .await
                        .update_operation_settings(updated_op_settings)
                        .await;
                    let mut s = state.lock().await;
                    s.skip_focus_worker_running = false;
                    return;
                }

                // Calibration failed - retry after interval from attempt start.
                info!(
                    "Skip-focus calibration failed, will retry in {} seconds",
                    SKIP_FOCUS_RETRY_INTERVAL_SECS
                );

                // Check every 100ms if we should abort or if interval has
                // elapsed.
                loop {
                    tokio::time::sleep(Duration::from_millis(100)).await;

                    let locked_state = state.lock().await;
                    if !locked_state.skip_focus_active {
                        info!("Skip-focus mode deactivated during retry wait");
                        drop(locked_state);
                        let mut s = state.lock().await;
                        s.skip_focus_worker_running = false;
                        return;
                    }
                    if locked_state
                        .skip_focus_last_attempt
                        .unwrap()
                        .elapsed()
                        .as_secs()
                        >= SKIP_FOCUS_RETRY_INTERVAL_SECS
                    {
                        break;
                    }
                }
            }
        });
    }

    pub(super) fn get_automatic_update_interval(
        state: &CedarState,
    ) -> std::time::Duration {
        if state.operation_settings.operating_mode
            == Some(OperatingMode::Setup as i32)
            && state.operation_settings.focus_assist_mode.unwrap_or(false)
        {
            return FOCUS_ASSIST_UPDATE_INTERVAL;
        }
        // For all other cases, go as fast as possible.
        // This can be adjusted automatically in the future based on motion
        // detection.
        Duration::ZERO
    }

    pub(super) async fn set_update_interval(
        state: &CedarState,
        update_interval: std::time::Duration,
    ) -> Result<(), CanonicalError> {
        if let Some(attached_camera) = &state.attached_camera {
            attached_camera
                .lock()
                .await
                .set_update_interval(update_interval)
                .await
                .unwrap();
        }
        state
            .camera
            .lock()
            .await
            .set_update_interval(update_interval)
            .await
    }

    pub(super) async fn reset_session_stats(state: &mut CedarState) {
        state.detect_engine.lock().await.reset_session_stats().await;
        state.solve_engine.lock().await.reset_session_stats().await;
        state.serve_engine.lock().await.reset_session_stats().await;
    }

    // Called when entering SETUP mode.
    pub(super) async fn set_pre_calibration_defaults(
        camera: &Arc<tokio::sync::Mutex<Box<dyn AbstractCamera + Send>>>,
        initial_exposure_duration: Duration,
    ) -> Result<(), CanonicalError> {
        let mut locked_camera = camera.lock().await;
        locked_camera
            .set_exposure_duration(initial_exposure_duration)
            .await?;
        if let Err(e) = locked_camera.set_offset(Offset::new(3)).await {
            debug!("Could not set offset: {:?}", e);
        }
        Ok(())
    }

    pub(super) async fn set_gain(
        camera: &Arc<tokio::sync::Mutex<Box<dyn AbstractCamera + Send>>>,
        daylight_mode: bool,
    ) {
        let mut locked_camera = camera.lock().await;
        let gain = if daylight_mode {
            Gain::new(0)
        } else {
            locked_camera.optimal_gain().await
        };
        locked_camera.set_gain(gain).await.unwrap();
    }

    // Called when entering OPERATE mode. The bool indicates whether the
    // calibration succeeded; the only error returned is ABORTED if the
    // calibration is canceled.
    pub(super) async fn calibrate(
        state: Arc<tokio::sync::Mutex<CedarState>>,
    ) -> Result<bool, CanonicalError> {
        let initial_exposure_duration;
        let max_exposure_duration;
        let detect_binning;
        let detection_sigma;
        let star_count_goal;
        let camera;
        let calibrator;
        let cancel_calibration;
        let calibration_data;
        let detect_engine;
        let solve_engine;
        let solver;
        let width;
        let height;
        let fixed_settings_arc = {
            let locked_state = state.lock().await;
            camera = locked_state.camera.clone();
            calibrator = locked_state.calibrator.clone();
            cancel_calibration = locked_state.cancel_calibration.clone();
            calibration_data = locked_state.calibration_data.clone();
            detect_engine = locked_state.detect_engine.clone();
            solve_engine = locked_state.solve_engine.clone();
            solver = locked_state.solver.clone();
            initial_exposure_duration = locked_state.initial_exposure_duration;
            locked_state.fixed_settings.clone()
        }; // State lock released here!

        max_exposure_duration = std::time::Duration::try_from(
            fixed_settings_arc
                .lock()
                .await
                .max_exposure_time
                .clone()
                .unwrap(),
        )
        .unwrap();

        {
            {
                (width, height) = MyCedar::camera_geometry(&camera).await;
                let _display_sampling;
                (detect_binning, _display_sampling) = MyCedar::compute_binning(
                    &*state.lock().await,
                    width,
                    height,
                );
            }

            // Use the sigma currently in effect, so a calibrated exposure
            // duration matches the sensitivity setting operation will run
            // under.
            let locked_detect_engine = detect_engine.lock().await;
            detection_sigma = locked_detect_engine.get_detection_sigma();
            star_count_goal = locked_detect_engine.get_star_count_goal();
        }
        calibration_data.lock().await.calibration_failure_reason = None;

        let offset = match calibrator
            .lock()
            .await
            .calibrate_offset(cancel_calibration.clone())
            .await
        {
            Ok(o) => o,
            Err(e) => {
                if e.code == CanonicalErrorCode::Aborted {
                    return Err(e);
                }
                if e.code != CanonicalErrorCode::Unimplemented {
                    warn! {"Error while calibrating offset: {:?}, using 3", e};
                }
                Offset::new(3) // Sane fallback value.
            }
        };
        calibration_data.lock().await.camera_offset = Some(offset.value());

        let exp_duration = match calibrator
            .lock()
            .await
            .calibrate_exposure_duration(
                initial_exposure_duration,
                max_exposure_duration,
                star_count_goal,
                detect_binning,
                detection_sigma,
                cancel_calibration.clone(),
            )
            .await
        {
            Ok(ed) => ed,
            Err(e) => {
                match e {
                    ExposureCalibrationError::TooFewStars => {
                        calibration_data
                            .lock()
                            .await
                            .calibration_failure_reason =
                            Some(CalibrationFailureReason::TooFewStars.into());
                    }
                    ExposureCalibrationError::BrightSky => {
                        calibration_data
                            .lock()
                            .await
                            .calibration_failure_reason =
                            Some(CalibrationFailureReason::BrightSky.into());
                    }
                    ExposureCalibrationError::Aborted => {
                        return Err(aborted_error(
                            "Cancelled during calibrate_exposure_duration().",
                        ));
                    }
                }
                warn! {
                    "Error while calibrating exposure duration: \
                    {:?}, using {:?}",
                    e,
                    initial_exposure_duration,
                };
                return Ok(false);
            }
        };
        calibration_data.lock().await.target_exposure_time =
            Some(prost_types::Duration::try_from(exp_duration).unwrap());
        detect_engine
            .lock()
            .await
            .set_calibrated_exposure_duration(Some(exp_duration))
            .await;

        match calibrator
            .lock()
            .await
            .calibrate_optical(
                solver.clone(),
                detect_binning,
                detection_sigma,
                cancel_calibration.clone(),
            )
            .await
        {
            Ok((fov, distortion, match_max_error, solve_duration)) => {
                let mut locked_calibration_data = calibration_data.lock().await;
                locked_calibration_data.fov_horizontal = Some(fov);
                locked_calibration_data.fov_vertical =
                    Some(fov * height as f64 / width as f64);
                locked_calibration_data.lens_distortion = Some(distortion);
                locked_calibration_data.match_max_error = Some(match_max_error);
                let sensor_width_mm =
                    camera.lock().await.sensor_size().await.0 as f64;
                let lens_fl_mm =
                    sensor_width_mm / (2.0 * (fov / 2.0).to_radians()).tan();
                locked_calibration_data.lens_fl_mm = Some(lens_fl_mm);
                let pixel_width_mm = sensor_width_mm
                    / camera.lock().await.dimensions().await.0 as f64;
                locked_calibration_data.pixel_angular_size =
                    Some((pixel_width_mm / lens_fl_mm).atan().to_degrees());

                let operation_solve_timeout = std::cmp::min(
                    std::cmp::max(
                        solve_duration * 10,
                        Duration::from_millis(500),
                    ),
                    Duration::from_secs(1),
                ); // TODO: max solve time cmd line arg
                let mut locked_solve_engine = solve_engine.lock().await;
                locked_solve_engine.set_fov_estimate(Some(fov)).await?;
                locked_solve_engine.set_distortion(distortion).await?;
                locked_solve_engine
                    .set_match_max_error(match_max_error)
                    .await?;
                locked_solve_engine
                    .set_solve_timeout(operation_solve_timeout)
                    .await?;
            }
            Err(e) => {
                let mut locked_calibration_data = calibration_data.lock().await;
                if e.code != CanonicalErrorCode::Aborted {
                    locked_calibration_data.calibration_failure_reason =
                        Some(CalibrationFailureReason::SolverFailed.into());
                }
                locked_calibration_data.fov_horizontal = None;
                locked_calibration_data.lens_distortion = None;
                locked_calibration_data.match_max_error = None;
                locked_calibration_data.lens_fl_mm = None;
                locked_calibration_data.pixel_angular_size = None;
                let mut locked_solve_engine = solve_engine.lock().await;
                locked_solve_engine.set_fov_estimate(None).await?;
                locked_solve_engine.set_distortion(0.0).await?;
                locked_solve_engine.set_match_max_error(0.005).await?;
                // TODO: pass this in? Should come from command line, maybe is
                // max solve time.
                locked_solve_engine
                    .set_solve_timeout(Duration::from_secs(1))
                    .await?;
                if e.code == CanonicalErrorCode::Aborted {
                    return Err(e);
                }
                warn! {"Error while calibrating optics: {:?}", e};
                return Ok(false);
            }
        };
        debug!("Calibration result: {:?}", calibration_data.lock().await);
        Ok(true)
    }
}
