// Copyright (c) 2026 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use std::{ops::DerefMut as _, path::PathBuf, sync::Arc};

use cedar_camera::image_camera::ImageCamera;
use cedar_elements::cedar::{
    cedar_server::Cedar, DetectSensitivity, OperatingMode, OperationSettings,
    Preferences,
};
use image::ImageReader;
use log::{info, warn};

use super::{
    cedar_rpcs::{logged_status, tonic_status, GrpcTimer},
    get_camera, CedarState, MyCedar, FOCUS_ASSIST_UPDATE_INTERVAL,
};

// Maps a user-facing DetectSensitivity setting to the multiplier DetectEngine
// applies to its configured (nominal) detection sigma. Lower sigma detects
// dimmer stars at the cost of admitting more spurious detections.
fn sigma_scale_for_sensitivity(sensitivity: DetectSensitivity) -> f64 {
    match sensitivity {
        DetectSensitivity::SensitivityUnspecified | DetectSensitivity::Normal => {
            1.0
        }
        DetectSensitivity::High => 0.9,
        DetectSensitivity::Highest => 0.8,
    }
}

impl MyCedar {
    pub(super) async fn update_operation_settings_impl(
        &self,
        request: tonic::Request<OperationSettings>,
    ) -> Result<tonic::Response<OperationSettings>, tonic::Status> {
        let _timer = GrpcTimer::new("update_operation_settings");
        let mut req: OperationSettings = request.into_inner();

        // Block operation setting changes while in skip-focus auto-calibration
        // mode. User must first disable skip_focus preference to exit
        // this mode. Also read skip_alignment preference for use below.
        let skip_alignment = {
            let locked_state = self.state.lock().await;
            if locked_state.skip_focus_worker_running
                && (req.operating_mode.is_some()
                    || req.daylight_mode.is_some()
                    || req.focus_assist_mode.is_some())
            {
                return Err(logged_status!(
                    failed_precondition,
                    "Cannot change operation settings while in skip-focus \
                     auto-calibration mode. \
                     Disable skip_focus preference first."
                ));
            }
            let skip_alignment = locked_state
                .preferences
                .lock()
                .await
                .skip_alignment
                .unwrap_or(false);
            skip_alignment
        };

        if let Some(new_operating_mode) = req.operating_mode {
            // Extract current state and component references first.
            let (
                current_operating_mode,
                calibrating,
                focus_mode,
                daylight_mode,
                solve_engine_arc,
                detect_engine_arc,
                telescope_position_arc,
                alt_az_slew_target_arc,
            ) = {
                let locked_state = self.state.lock().await;
                (
                    locked_state.operation_settings.operating_mode.unwrap(),
                    locked_state.calibrating,
                    locked_state.operation_settings.focus_assist_mode.unwrap(),
                    locked_state.operation_settings.daylight_mode.unwrap(),
                    locked_state.solve_engine.clone(),
                    locked_state.detect_engine.clone(),
                    locked_state.telescope_position.clone(),
                    locked_state.alt_az_slew_target.clone(),
                )
            }; // State lock released here!

            // Only do something if operating mode is changing.
            if new_operating_mode != current_operating_mode {
                if calibrating {
                    return Err(logged_status!(
                        failed_precondition,
                        "Cannot change operating mode while calibrating"
                    ));
                }
                let mut final_focus_mode = focus_mode;
                if let Some(req_focus_assist_mode) = req.focus_assist_mode {
                    final_focus_mode = req_focus_assist_mode;
                    // We're handling this here, so don't also handle it below.
                    req.focus_assist_mode = None;
                }
                let mut final_daylight_mode = daylight_mode;
                if let Some(req_daylight_mode) = req.daylight_mode {
                    final_daylight_mode = req_daylight_mode;
                    // We're handling this here, so don't also handle it below.
                    req.daylight_mode = None;
                }

                let mut calibrating = false;
                if new_operating_mode == OperatingMode::Setup as i32 {
                    // Transition: OPERATE -> SETUP mode. We are already
                    // calibrated. Set gain (requires state
                    // access)
                    {
                        let camera = self.state.lock().await.camera.clone();
                        MyCedar::set_gain(&camera, final_daylight_mode).await;
                        let mut locked_state = self.state.lock().await;
                        if final_focus_mode {
                            // In SETUP focus assist mode we use pre-calibrate
                            // settings, capped at the focus assist frame rate.
                            if let Err(x) = MyCedar::set_update_interval(
                                &locked_state,
                                FOCUS_ASSIST_UPDATE_INTERVAL,
                            )
                            .await
                            {
                                return Err(tonic_status(x));
                            }
                            let initial_exposure_duration =
                                locked_state.initial_exposure_duration;
                            drop(locked_state);
                            if let Err(x) =
                                MyCedar::set_pre_calibration_defaults(
                                    &camera,
                                    initial_exposure_duration,
                                )
                                .await
                            {
                                return Err(tonic_status(x));
                            }
                            locked_state = self.state.lock().await;
                        }
                        MyCedar::reset_session_stats(locked_state.deref_mut())
                            .await;
                    } // State lock released here!

                    // Configure engines (outside state lock).
                    solve_engine_arc.lock().await.set_align_mode(true).await;
                    {
                        let mut locked_detect_engine =
                            detect_engine_arc.lock().await;
                        locked_detect_engine
                            .set_focus_mode(final_focus_mode)
                            .await;
                        locked_detect_engine
                            .set_daylight_mode(final_daylight_mode)
                            .await;
                        if final_focus_mode {
                            locked_detect_engine
                                .set_calibrated_exposure_duration(None)
                                .await;
                        }
                    }
                    telescope_position_arc.lock().await.slew_active = false;
                    *alt_az_slew_target_arc.lock().await = None;
                } else if new_operating_mode == OperatingMode::Operate as i32 {
                    // Transition: SETUP -> OPERATE mode.
                    if focus_mode || daylight_mode {
                        // The SETUP (with focus mode or daytime align) ->
                        // OPERATE mode change involves a call to calibrate(),
                        // which can take several seconds. If the gRPC client
                        // aborts the RPC (e.g. due to timeout), we want the
                        // calibration and state updates (i.e. detect engine's
                        // focus_mode, our operating_mode) to be completed
                        // properly.
                        MyCedar::spawn_calibration(
                            self.state.clone(),
                            // new_operate_mode=
                            true,
                            // new_focus_mode=
                            false,
                            // new_daylight_mode=
                            false,
                        );
                        calibrating = true;
                        // The update of state.operation_settings.operation_mode
                        // happens when the calibration finishes. TODO: also
                        // focus and daylight sub-modes.
                    } else {
                        // Transition into Operate mode from SETUP align mode.
                        // Already calibrated.
                        {
                            let camera = self.state.lock().await.camera.clone();
                            MyCedar::set_gain(&camera, false).await;
                            let locked_state = self.state.lock().await;
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
                        } // State lock released here!

                        // Configure engines (outside state lock).
                        detect_engine_arc
                            .lock()
                            .await
                            .set_daylight_mode(false)
                            .await;
                        solve_engine_arc
                            .lock()
                            .await
                            .set_align_mode(false)
                            .await;
                        solve_engine_arc.lock().await.start();
                    }
                } else {
                    return Err(logged_status!(
                        invalid_argument,
                        format!(
                            "Got invalid operating_mode: {}.",
                            new_operating_mode
                        )
                    ));
                }
                if !calibrating {
                    let mut locked_state = self.state.lock().await;
                    locked_state.operation_settings.operating_mode =
                        Some(new_operating_mode);
                    locked_state.operation_settings.daylight_mode =
                        Some(final_daylight_mode);
                    locked_state.operation_settings.focus_assist_mode =
                        Some(final_focus_mode);
                }
            } // Operating mode is changing.
        } // Update operating_mode.
        if let Some(new_daylight_mode) = req.daylight_mode {
            let (detect_engine_arc, calibrating) = {
                let mut locked_state = self.state.lock().await;
                let mut calibrating = false;
                if locked_state.operation_settings.daylight_mode.unwrap()
                    != new_daylight_mode
                {
                    if locked_state.calibrating {
                        return Err(logged_status!(
                            failed_precondition,
                            "Cannot change daylight mode while calibrating"
                        ));
                    }
                    let mut final_focus_mode = locked_state
                        .operation_settings
                        .focus_assist_mode
                        .unwrap();
                    if let Some(req_focus_assist_mode) = req.focus_assist_mode {
                        final_focus_mode = req_focus_assist_mode;
                        // We're handling this here, so don't also handle it
                        // below.
                        req.focus_assist_mode = None;
                    }
                    if locked_state.operation_settings.operating_mode
                        == Some(OperatingMode::Setup as i32)
                    {
                        // In SETUP align mode?
                        if !locked_state
                            .operation_settings
                            .focus_assist_mode
                            .unwrap()
                            && !new_daylight_mode
                        {
                            // Turning off daylight_mode in SETUP align mode;
                            // need calibration, which can take several seconds.
                            // If the gRPC client aborts the RPC (e.g. due to
                            // timeout), we want the calibration and state
                            // updates (i.e. detect engine's focus_mode, our
                            // operating_mode) to be completed properly.
                            MyCedar::spawn_calibration(
                                self.state.clone(),
                                // new_operate_mode=
                                skip_alignment,
                                final_focus_mode,
                                new_daylight_mode,
                            );
                            calibrating = true;
                        } else {
                            let camera = locked_state.camera.clone();
                            drop(locked_state);
                            MyCedar::set_gain(&camera, new_daylight_mode).await;
                            locked_state = self.state.lock().await;
                        }
                    }
                }
                (locked_state.detect_engine.clone(), calibrating)
            }; // State lock released here!

            if !calibrating {
                detect_engine_arc
                    .lock()
                    .await
                    .set_daylight_mode(new_daylight_mode)
                    .await;
                self.state.lock().await.operation_settings.daylight_mode =
                    Some(new_daylight_mode);
            }
        }
        if let Some(new_focus_assist_mode) = req.focus_assist_mode {
            let (detect_engine_arc, calibrating) = {
                let mut locked_state = self.state.lock().await;
                let mut calibrating = false;
                if locked_state.operation_settings.operating_mode
                    == Some(OperatingMode::Setup as i32)
                    && locked_state
                        .operation_settings
                        .focus_assist_mode
                        .unwrap()
                        != new_focus_assist_mode
                {
                    if locked_state.calibrating {
                        return Err(logged_status!(
                            failed_precondition,
                            "Cannot change focus assist mode while calibrating"
                        ));
                    }
                    let daylight_mode =
                        locked_state.operation_settings.daylight_mode.unwrap();
                    if new_focus_assist_mode {
                        // Entering focus assist mode; cap the frame rate.
                        if let Err(x) = MyCedar::set_update_interval(
                            &locked_state,
                            FOCUS_ASSIST_UPDATE_INTERVAL,
                        )
                        .await
                        {
                            return Err(tonic_status(x));
                        }
                        let camera = locked_state.camera.clone();
                        let initial_exposure_duration =
                            locked_state.initial_exposure_duration;
                        drop(locked_state);
                        if let Err(x) = MyCedar::set_pre_calibration_defaults(
                            &camera,
                            initial_exposure_duration,
                        )
                        .await
                        {
                            return Err(tonic_status(x));
                        }
                        MyCedar::set_gain(&camera, daylight_mode).await;
                        locked_state = self.state.lock().await;
                    } else if !daylight_mode {
                        // Exiting focus assist mode, without daylight mode
                        // active.
                        // Trigger a calibration, which can take several
                        // seconds. If the gRPC client
                        // aborts the RPC (e.g. due to timeout), we
                        // want the calibration and state updates (i.e. detect
                        // engine's focus_mode, our operating_mode) to be
                        // completed properly.
                        let skip_focus = locked_state
                            .preferences
                            .lock()
                            .await
                            .skip_focus
                            .unwrap_or(false);
                        let spawn_fn =
                            |state: Arc<tokio::sync::Mutex<CedarState>>| {
                                if skip_focus {
                                    MyCedar::spawn_skip_focus_calibration(
                                        state,
                                        skip_alignment,
                                    );
                                } else {
                                    MyCedar::spawn_calibration(
                                        state,
                                        // new_operate_mode=
                                        skip_alignment,
                                        new_focus_assist_mode,
                                        daylight_mode,
                                    );
                                }
                            };
                        spawn_fn(self.state.clone());
                        calibrating = true;
                    }
                }
                (locked_state.detect_engine.clone(), calibrating)
            }; // State lock released here!

            if !calibrating {
                detect_engine_arc
                    .lock()
                    .await
                    .set_focus_mode(new_focus_assist_mode)
                    .await;
                self.state.lock().await.operation_settings.focus_assist_mode =
                    Some(new_focus_assist_mode);
            }
        }
        if let Some(_log_dwelled_positions) = req.log_dwelled_positions {
            return Err(logged_status!(
                unimplemented,
                "rpc UpdateOperationSettings not implemented \
                for log_dwelled_positions."
            ));
        }
        if let Some(catalog_entry_match) = req.catalog_entry_match {
            let solve_engine_arc = {
                let mut locked_state = self.state.lock().await;
                if locked_state.cedar_sky.is_none() {
                    return Err(logged_status!(
                        unimplemented,
                        format!(
                            "{} does not include Cedar Sky.",
                            self.product_name
                        )
                    ));
                }
                locked_state.operation_settings.catalog_entry_match =
                    Some(catalog_entry_match.clone());
                locked_state.solve_engine.clone()
            }; // State lock released here!

            solve_engine_arc
                .lock()
                .await
                .set_catalog_entry_match(Some(catalog_entry_match.clone()))
                .await;
            let preferences = Preferences {
                catalog_entry_match: Some(catalog_entry_match),
                ..Default::default()
            };
            self.update_preferences(tonic::Request::new(preferences))
                .await?;
        }
        if let Some(demo_image_filename) = req.demo_image_filename {
            // Load the demo image before acquiring the state lock, since
            // file I/O and image decoding can be slow.
            let demo_img_u8 = if !demo_image_filename.is_empty() {
                let input_path = PathBuf::from("./demo_images")
                    .join(demo_image_filename.clone());
                let img_file = match ImageReader::open(&input_path) {
                    Err(x) => {
                        return Err(logged_status!(
                            failed_precondition,
                            format!(
                                "Error opening image file {:?}: {:?}.",
                                input_path, x
                            )
                        ));
                    }
                    Ok(img_file) => img_file,
                };
                let img = match img_file.decode() {
                    Err(x) => {
                        return Err(logged_status!(
                            failed_precondition,
                            format!(
                                "Error decoding image file {:?}: {:?}.",
                                input_path, x
                            )
                        ));
                    }
                    Ok(img) => img,
                };
                Some(img.to_luma8())
            } else {
                None
            };

            let (
                old_camera,
                new_camera,
                width,
                height,
                detect_binning,
                display_sampling,
                detect_engine_arc,
                solve_engine_arc,
                calibrator_arc,
                preferences_arc,
            ) = {
                let mut locked_state = self.state.lock().await;
                let old_camera = locked_state.camera.clone();
                if demo_image_filename.is_empty() {
                    // Go back to using our configured camera.
                    locked_state.camera = get_camera(
                        &locked_state.attached_camera,
                        &self.test_image_camera,
                    )
                    .await;
                    locked_state.operation_settings.demo_image_filename = None;
                    locked_state
                        .solve_engine
                        .lock()
                        .await
                        .set_use_imu_tracker(true)
                        .await;
                    info!(
                        "Using camera {}",
                        locked_state.camera.lock().await.model().await
                    );
                } else {
                    let img_u8 = demo_img_u8.unwrap();
                    locked_state.camera = Arc::new(tokio::sync::Mutex::new(
                        Box::new(ImageCamera::new(img_u8).await.unwrap()),
                    ));
                    locked_state
                        .solve_engine
                        .lock()
                        .await
                        .set_use_imu_tracker(false)
                        .await;
                    info!("Using demo image {}", demo_image_filename);
                    locked_state.operation_settings.demo_image_filename =
                        Some(demo_image_filename);
                }
                let new_camera = locked_state.camera.clone();
                // Start the new camera now; old_camera is stopped later,
                // after DetectEngine/Calibrator switch over (see below).
                // Skip if they're the same Arc (e.g. redundant switch).
                if !Arc::ptr_eq(&old_camera, &new_camera) {
                    if let Err(e) = new_camera.lock().await.start().await {
                        // New camera never started; roll back so nothing
                        // ends up split between old and new.
                        locked_state.camera = old_camera.clone();
                        return Err(tonic_status(e));
                    }
                }
                let (width, height) =
                    MyCedar::camera_geometry(&new_camera).await;

                let std_duration =
                    MyCedar::get_automatic_update_interval(&locked_state);
                if let Err(x) = MyCedar::set_update_interval(
                    &locked_state,
                    std_duration,
                )
                .await
                {
                    return Err(tonic_status(x));
                }
                let (detect_binning, display_sampling) =
                    MyCedar::compute_binning(&locked_state, width, height);

                (
                    old_camera,
                    new_camera,
                    width,
                    height,
                    detect_binning,
                    display_sampling,
                    locked_state.detect_engine.clone(),
                    locked_state.solve_engine.clone(),
                    locked_state.calibrator.clone(),
                    locked_state.preferences.clone(),
                )
            }; // State lock released here!

            // Configure engines outside state lock. Switch DetectEngine
            // and Calibrator to new_camera before stopping old_camera
            // (below) - each tracks its own camera separately from
            // CedarState, so stopping too early risks a FailedPrecondition
            // if either fetches the old, now-stopped camera in between.
            detect_engine_arc
                .lock()
                .await
                .set_detect_binning(detect_binning, display_sampling)
                .await;
            detect_engine_arc
                .lock()
                .await
                .replace_camera(new_camera.clone())
                .await;
            solve_engine_arc.lock().await.clear_plate_solution().await;
            calibrator_arc
                .lock()
                .await
                .replace_camera(new_camera.clone());
            if !Arc::ptr_eq(&old_camera, &new_camera) {
                old_camera.lock().await.stop().await;
            }

            // Validate boresight_pixel (full-sensor coords) against the new
            // camera's image area, then re-apply to solve_engine.
            let boresight_pixel_value =
                preferences_arc.lock().await.boresight_pixel.clone();
            let inset = 16;
            let new_boresight = if let Some(bsp) = boresight_pixel_value {
                if bsp.x < inset as f64
                    || bsp.x > (width - inset) as f64
                    || bsp.y < inset as f64
                    || bsp.y > (height - inset) as f64
                {
                    preferences_arc.lock().await.boresight_pixel = None;
                    None
                } else {
                    Some(bsp)
                }
            } else {
                None
            };
            solve_engine_arc
                .lock()
                .await
                .set_boresight_pixel(new_boresight)
                .await
                .unwrap();
        }
        if let Some(use_imu) = req.use_imu {
            let solve_engine_arc = {
                let mut locked_state = self.state.lock().await;
                if locked_state.imu_tracker.is_none() {
                    return Err(logged_status!(
                        failed_precondition,
                        "IMU not available on this system"
                    ));
                }
                locked_state.operation_settings.use_imu = Some(use_imu);

                locked_state.solve_engine.clone()
            }; // State lock released here!

            // Configure solver to use/not use IMU (outside state lock).
            solve_engine_arc
                .lock()
                .await
                .set_use_imu_tracker(use_imu)
                .await;
        }
        if let Some(detect_sensitivity) = req.detect_sensitivity {
            let sensitivity = DetectSensitivity::try_from(detect_sensitivity)
                .unwrap_or(DetectSensitivity::Normal);
            let detect_engine_arc = {
                let mut locked_state = self.state.lock().await;
                locked_state.operation_settings.detect_sensitivity =
                    Some(sensitivity as i32);
                locked_state.detect_engine.clone()
            }; // State lock released here!

            detect_engine_arc
                .lock()
                .await
                .set_detect_sigma_scale(sigma_scale_for_sensitivity(
                    sensitivity,
                ))
                .await;
        }

        let locked_state = self.state.lock().await;
        let updated_op_settings = locked_state.operation_settings.clone();
        let serve_engine_arc = locked_state.serve_engine.clone();
        drop(locked_state);
        serve_engine_arc
            .lock()
            .await
            .update_operation_settings(updated_op_settings.clone())
            .await;

        Ok(tonic::Response::new(updated_op_settings))
    } // update_operation_settings_impl().
}
