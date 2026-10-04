// Copyright (c) 2026 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use std::{
    process::Command,
    sync::atomic::Ordering as AtomicOrdering,
};

use cedar_elements::{
    astro_util::celestial_coord_to_j2000,
    cedar::{
        cedar_server::Cedar, ActionRequest, EmptyMessage, MountType,
        OperatingMode, Preferences,
    },
    thread_name::ThreadName,
    wifi_trait::WifiMode as WifiModeDomain,
};
use log::{info, warn};

use super::{
    cedar_rpcs::{logged_status, tonic_status, GrpcTimer},
    prepare_for_exit_async, MyCedar,
};
use crate::bonding_helper::set_adapter_name;

impl MyCedar {
    pub(super) async fn initiate_action_impl(
        &self,
        request: tonic::Request<ActionRequest>,
    ) -> Result<tonic::Response<EmptyMessage>, tonic::Status> {
        let _timer = GrpcTimer::new("initiate_action");

        let req: ActionRequest = request.into_inner();
        if req.cancel_calibration.unwrap_or(false) {
            let (cancel_calibration_arc, solver_arc, calibrating) = {
                let locked_state = self.state.lock().await;
                (
                    locked_state.cancel_calibration.clone(),
                    locked_state.solver.clone(),
                    locked_state.calibrating,
                )
            }; // State lock released here!
            if calibrating {
                *cancel_calibration_arc.lock().await = true;
                solver_arc.lock().await.cancel();
            }
        }
        if req.capture_boresight.unwrap_or(false) {
            let preferences;
            {
                let operating_mode = self
                    .state
                    .lock()
                    .await
                    .operation_settings
                    .operating_mode
                    .unwrap_or(OperatingMode::Setup as i32);
                if operating_mode == OperatingMode::Setup as i32 {
                    return Err(logged_status!(
                        failed_precondition,
                        "Capture boresight not valid in setup mode."
                    ));
                }
                // Operate mode.
                let solve_engine_arc =
                    self.state.lock().await.solve_engine.clone();
                let plate_solution = solve_engine_arc
                    .lock()
                    .await
                    .get_next_result(None, /* non_blocking= */ false)
                    .await
                    .unwrap();
                if let Some(slew_request) = plate_solution.slew_request {
                    let bsp = slew_request.image_pos.unwrap();
                    if let Err(x) = solve_engine_arc
                        .lock()
                        .await
                        .set_boresight_pixel(Some(bsp.clone()))
                        .await
                    {
                        return Err(tonic_status(x));
                    }
                    preferences = Preferences {
                        boresight_pixel: Some(bsp),
                        ..Default::default()
                    };
                    if let Some(hpm) =
                        self.state.lock().await.hot_pixel_map.clone()
                    {
                        hpm.lock().await.reset();
                    }
                } else {
                    return Err(logged_status!(
                        failed_precondition,
                        "No slew request active".to_string()
                    ));
                }
            }
            self.update_preferences(tonic::Request::new(preferences))
                .await?;
        } // capture_boresight.
        if let Some(mut bsp) = req.designate_boresight {
            let image_rotator;
            let width;
            let height;
            {
                let locked_state = self.state.lock().await;
                let serve_engine_arc = locked_state.serve_engine.clone();
                let camera_arc = locked_state.camera.clone();
                drop(locked_state);
                image_rotator =
                    serve_engine_arc.lock().await.image_rotator().await;
                (width, height) = camera_arc.lock().await.dimensions().await;
            }
            (bsp.x, bsp.y) = image_rotator
                .transform_from_rotated(bsp.x, bsp.y, width, height);

            let distance_from_center = ((width as f64 / 2.0 - bsp.x)
                * (width as f64 / 2.0 - bsp.x)
                + (height as f64 / 2.0 - bsp.y)
                    * (height as f64 / 2.0 - bsp.y))
                .sqrt();
            if distance_from_center > height as f64 / 2.0 {
                return Err(logged_status!(
                    failed_precondition,
                    "Too far from center".to_string()
                ));
            }

            let solve_engine = self.state.lock().await.solve_engine.clone();
            if let Err(x) = solve_engine
                .lock()
                .await
                .set_boresight_pixel(Some(bsp.clone()))
                .await
            {
                return Err(tonic_status(x));
            };
            if let Some(hpm) = self.state.lock().await.hot_pixel_map.clone() {
                hpm.lock().await.reset();
            }
            let preferences = Preferences {
                boresight_pixel: Some(bsp.clone()),
                ..Default::default()
            };
            self.update_preferences(tonic::Request::new(preferences))
                .await?;
        }
        if req.shutdown_server.unwrap_or(false) {
            info!("Shutting down host system");
            let (imu_tracker, hot_pixel_map, saving_state, activity_led) = {
                let locked_state = self.state.lock().await;
                (
                    locked_state.imu_tracker.clone(),
                    locked_state.hot_pixel_map.clone(),
                    locked_state.saving_state.clone(),
                    locked_state.activity_led.clone(),
                )
            };
            saving_state.store(true, AtomicOrdering::Relaxed);
            prepare_for_exit_async(&imu_tracker, &hot_pixel_map).await;
            activity_led.lock().await.stop();
            let output = Command::new("sudo")
                .arg("shutdown")
                .arg("now")
                .output()
                .expect("Failed to execute 'sudo shutdown now' command");
            if !output.status.success() {
                let error_str = String::from_utf8_lossy(&output.stderr);
                return Err(logged_status!(
                    failed_precondition,
                    format!("sudo shutdown error: {:?}.", error_str)
                ));
            }
        }
        if req.restart_server.unwrap_or(false) {
            info!("Restarting host system");
            let (imu_tracker, hot_pixel_map, saving_state, activity_led) = {
                let locked_state = self.state.lock().await;
                (
                    locked_state.imu_tracker.clone(),
                    locked_state.hot_pixel_map.clone(),
                    locked_state.saving_state.clone(),
                    locked_state.activity_led.clone(),
                )
            };
            saving_state.store(true, AtomicOrdering::Relaxed);
            prepare_for_exit_async(&imu_tracker, &hot_pixel_map).await;
            activity_led.lock().await.stop();
            let output = Command::new("sudo")
                .arg("reboot")
                .arg("now")
                .output()
                .expect("Failed to execute 'sudo reboot now' command");
            if !output.status.success() {
                let error_str = String::from_utf8_lossy(&output.stderr);
                return Err(logged_status!(
                    failed_precondition,
                    format!("sudo reboot error: {:?}.", error_str)
                ));
            }
        }
        if let Some(slew_coord) = req.initiate_slew {
            if !(-90.0..=90.0).contains(&slew_coord.dec) {
                return Err(logged_status!(
                    invalid_argument,
                    format!(
                        "initiate_slew.dec must be in -90..90; got {}",
                        slew_coord.dec
                    )
                ));
            }
            let (
                preferences_arc,
                fixed_settings_arc,
                telescope_pos_arc,
                alt_az_slew_target_arc,
            ) = {
                let locked_state = self.state.lock().await;
                (
                    locked_state.preferences.clone(),
                    locked_state.fixed_settings.clone(),
                    locked_state.telescope_position.clone(),
                    locked_state.alt_az_slew_target.clone(),
                )
            }; // State lock released here!

            let mount_type = preferences_arc.lock().await.mount_type;
            if mount_type == Some(MountType::AltAz.into())
                && fixed_settings_arc.lock().await.observer_location.is_none()
            {
                return Err(logged_status!(
                    failed_precondition,
                    "Need observer location for goto with alt-az mount"
                ));
            }
            // Only one goto at a time; this supersedes any alt/az goto.
            *alt_az_slew_target_arc.lock().await = None;
            let slew_coord = celestial_coord_to_j2000(&slew_coord);
            let mut telescope = telescope_pos_arc.lock().await;
            telescope.slew_target_ra = slew_coord.ra;
            telescope.slew_target_dec = slew_coord.dec;
            telescope.slew_active = true;
        }
        if let Some(slew_alt_az) = req.initiate_slew_alt_az {
            if !(-90.0..=90.0).contains(&slew_alt_az.altitude) {
                return Err(logged_status!(
                    invalid_argument,
                    format!(
                        "initiate_slew_alt_az.altitude must be in -90..90; \
                         got {}",
                        slew_alt_az.altitude
                    )
                ));
            }
            let (fixed_settings_arc, telescope_pos_arc, alt_az_slew_target_arc) = {
                let locked_state = self.state.lock().await;
                (
                    locked_state.fixed_settings.clone(),
                    locked_state.telescope_position.clone(),
                    locked_state.alt_az_slew_target.clone(),
                )
            }; // State lock released here!

            // Unlike an RA/Dec goto, the observer location is needed regardless
            // of mount type, since it is what ties an alt/az target to a
            // position on the celestial sphere.
            if fixed_settings_arc.lock().await.observer_location.is_none() {
                return Err(logged_status!(
                    failed_precondition,
                    "Need observer location for alt/az goto"
                ));
            }
            // Only one goto at a time; this supersedes any RA/Dec goto.
            telescope_pos_arc.lock().await.slew_active = false;
            *alt_az_slew_target_arc.lock().await = Some(slew_alt_az);
        }
        if req.stop_slew.unwrap_or(false) {
            let (telescope_position_arc, alt_az_slew_target_arc) = {
                let locked_state = self.state.lock().await;
                (
                    locked_state.telescope_position.clone(),
                    locked_state.alt_az_slew_target.clone(),
                )
            };
            telescope_position_arc.lock().await.slew_active = false;
            *alt_az_slew_target_arc.lock().await = None;
        }
        if req.save_image.unwrap_or(false) {
            let solve_engine = self.state.lock().await.solve_engine.clone();
            let result = solve_engine.lock().await.save_image().await;
            if let Err(x) = result {
                return Err(tonic_status(x));
            }
        }
        if let Some(update_ap) = req.update_wifi_access_point {
            let wifi = self.state.lock().await.wifi.clone();
            if wifi.is_none() {
                return Err(logged_status!(
                    unimplemented,
                    format!(
                        "{} does not include WiFi control.",
                        self.product_name
                    )
                ));
            }
            let mut locked_wifi = wifi.as_ref().unwrap().write().await;
            if let Err(x) = locked_wifi.update_access_point(
                update_ap.channel,
                update_ap.ssid.as_deref(),
                update_ap.psk.as_deref(),
            ) {
                return Err(tonic_status(x));
            }
            drop(locked_wifi);
            // Update the Bluetooth and published names to match the new SSID,
            // so all three ways of identifying this device stay in agreement.
            if let Some(new_ssid) = update_ap.ssid {
                let wifi_arc = wifi.as_ref().unwrap().clone();
                let name = new_ssid.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    wifi_arc.blocking_read().set_host_name(&name)
                })
                .await;
                if let Err(e) = set_adapter_name(&new_ssid).await {
                    warn!(
                        "Failed to update Bluetooth adapter name \
                        when updating WiFi SSID: {:?}",
                        e
                    );
                }
            }
        }
        #[allow(deprecated)] // Still honored for pre-SetWifiMode clients.
        if let Some(wifi_enabled) = req.wifi_enabled {
            // Deprecated: superseded by the SetWifiMode RPC. Mapped onto it
            // here for older clients -- true means access point mode, false
            // means WiFi off.
            let wifi = self.state.lock().await.wifi.clone();
            if wifi.is_none() {
                return Err(logged_status!(
                    unimplemented,
                    format!(
                        "{} does not include WiFi control.",
                        self.product_name
                    )
                ));
            }
            let wifi_arc = wifi.as_ref().unwrap().clone();
            let target_mode = if wifi_enabled {
                WifiModeDomain::AccessPoint
            } else {
                WifiModeDomain::Inactive
            };
            let result = tokio::task::spawn_blocking(move || {
                let _name = ThreadName::new("wifi-enable");
                wifi_arc.blocking_read().set_mode(target_mode, None, None)
            })
            .await
            .map_err(|e| {
                tonic::Status::internal(format!(
                    "set_mode task panicked: {:?}",
                    e
                ))
            })?;
            if let Err(x) = result {
                return Err(tonic_status(x));
            }
            // When disabling WiFi to use Bluetooth, ensure the adapter is
            // discoverable and pairable so the client can connect. If pairing
            // mode is already enabled forever, leave it be. Otherwise start
            // (or restart) the timed pairing window so the user can pair.
            if !wifi_enabled {
                let state = self.state.lock().await;
                let already_forever = *state.pairing_mode_forever.lock().await;
                if !already_forever {
                    *state.pairing_mode.lock().await = true;
                    MyCedar::spawn_pairing_mode_timer(
                        state.pairing_mode.clone(),
                        state.pairing_mode_forever.clone(),
                        state.pairing_mode_generation.clone(),
                    )
                    .await;
                }
            }
        }
        if req.clear_dont_show_items.unwrap_or(false) {
            let (prefs_to_write, serve_engine_arc) = {
                let locked_state = self.state.lock().await;
                let preferences_arc = locked_state.preferences.clone();
                let mut locked_prefs = preferences_arc.lock().await;
                locked_prefs.dont_show_items.clear();
                (locked_prefs.clone(), locked_state.serve_engine.clone())
            };
            self.save_preferences(serve_engine_arc, prefs_to_write)
                .await;
        }
        if let Some(mut dfr) = req.designate_daylight_focus_region {
            let (image_rotator, camera, detect_engine) = {
                let locked_state = self.state.lock().await;
                let serve_engine_arc = locked_state.serve_engine.clone();
                let image_rotator =
                    serve_engine_arc.lock().await.image_rotator().await;
                (
                    image_rotator,
                    locked_state.camera.clone(),
                    locked_state.detect_engine.clone(),
                )
            };
            let (width, height) = camera.lock().await.dimensions().await;
            (dfr.x, dfr.y) = image_rotator
                .transform_from_rotated(dfr.x, dfr.y, width, height);
            // Check that the point is within reasonable bounds.
            if dfr.x < 0.0
                || dfr.x >= width as f64
                || dfr.y < 0.0
                || dfr.y >= height as f64
            {
                return Err(logged_status!(
                    failed_precondition,
                    "Focus region point out of bounds".to_string()
                ));
            }
            detect_engine
                .lock()
                .await
                .set_daylight_focus_point(dfr.clone())
                .await;
        }
        if req.calibrate_dark_frame.unwrap_or(false) {
            if self.test_image_camera.is_some() {
                return Err(logged_status!(
                    failed_precondition,
                    "Dark frame calibration is not available with a test image."
                ));
            }
            // Precondition checks and extraction of needed arcs (no mutation
            // yet).
            let (calibrator, camera, detect_engine, fixed_settings_arc) = {
                let locked_state = self.state.lock().await;
                if locked_state
                    .operation_settings
                    .demo_image_filename
                    .as_deref()
                    .unwrap_or("")
                    .len()
                    > 0
                {
                    return Err(logged_status!(
                        failed_precondition,
                        "Dark frame calibration is not available \
                        in demo image mode."
                    ));
                }
                if locked_state.hot_pixel_map.is_none() {
                    return Err(logged_status!(
                        failed_precondition,
                        "No hot pixel map is configured."
                    ));
                }
                (
                    locked_state.calibrator.clone(),
                    locked_state.camera.clone(),
                    locked_state.detect_engine.clone(),
                    locked_state.fixed_settings.clone(),
                )
            }; // State lock released here!

            // Read max_exposure_duration before begin_calibration to set an
            // accurate duration estimate. Camera and detect_engine locks are
            // deferred to the spawned task: they may be held by the serve loop
            // during a long exposure, and begin_calibration stops the serve
            // loop (calibrating=true) so the spawned task acquires them
            // quickly.
            let max_exposure_duration = std::time::Duration::try_from(
                fixed_settings_arc
                    .lock()
                    .await
                    .max_exposure_time
                    .clone()
                    .unwrap(),
            )
            .unwrap();

            if !MyCedar::begin_calibration(
                &self.state,
                max_exposure_duration * 2,
            )
            .await
            {
                return Err(logged_status!(
                    failed_precondition,
                    "Calibration already in progress."
                ));
            }
            let state = self.state.clone();
            let _task_handle: tokio::task::JoinHandle<()> =
                tokio::task::spawn(async move {
                    let (width, height) =
                        MyCedar::camera_geometry(&camera).await;
                    let (detection_binning, _) = MyCedar::compute_binning(
                        &*state.lock().await,
                        width,
                        height,
                    );
                    let detection_sigma =
                        detect_engine.lock().await.get_detection_sigma();
                    MyCedar::set_gain(&camera, /* daylight_mode= */ false)
                        .await;
                    if let Err(e) = calibrator
                        .lock()
                        .await
                        .calibrate_dark_frame(
                            max_exposure_duration,
                            detection_binning,
                            detection_sigma,
                        )
                        .await
                    {
                        warn!("Dark frame calibration failed: {:?}", e);
                    }
                    state.lock().await.calibrating = false;
                });
        }
        if req.reset_hot_pixel_map.unwrap_or(false) {
            let hot_pixel_map = self.state.lock().await.hot_pixel_map.clone();
            match hot_pixel_map {
                None => {
                    return Err(logged_status!(
                        failed_precondition,
                        "No hot pixel map is configured."
                    ));
                }
                Some(hpm) => {
                    let mut locked_hpm = hpm.lock().await;
                    locked_hpm.reset();
                    if let Err(e) = locked_hpm.save_state() {
                        return Err(tonic_status(e));
                    }
                }
            }
        }
        if req.reset_imu_calibration.unwrap_or(false) {
            let imu_tracker = self.state.lock().await.imu_tracker.clone();
            match imu_tracker {
                None => {
                    return Err(logged_status!(
                        failed_precondition,
                        "No IMU tracker is configured."
                    ));
                }
                Some(imu_tracker) => {
                    imu_tracker.lock().await.reset().await;
                }
            }
        }
        if req.crash_server.unwrap_or(false) {
            log::info!("Received crash_server action request.");
            std::process::exit(1);
        }
        Ok(tonic::Response::new(EmptyMessage {}))
    } // initiate_action_impl().
}
