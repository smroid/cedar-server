// Copyright (c) 2026 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use std::{
    fs,
    fs::metadata,
    io,
    io::{ErrorKind, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::atomic::Ordering as AtomicOrdering,
    time::{Duration, Instant, SystemTime},
};

use canonical_error::{CanonicalError, CanonicalErrorCode};
use cedar_elements::{
    astro_util::{celestial_coord_from_horizon, horizon_coord_from_celestial},
    cedar::{
        cedar_server::Cedar, ActionRequest, BondedDevice, CpuUsageReport,
        EmptyMessage, FeatureLevel, FixedSettings, FrameRequest, FrameResult,
        GetBluetoothNameResponse, GetBondedDevicesResponse, ImageRequest,
        ImageResult, OperatingMode, OperationSettings, Preferences,
        RemoveBondRequest, ServerLogRequest, ServerLogResult,
        SetPairingModeRequest, SetWifiModeRequest,
        WifiNetwork as WifiNetworkProto, WifiScanResponse,
    },
    cedar_common::{CelestialCoord, HorizonCoord, WifiMode as WifiModeProto},
    cedar_sky::{
        CatalogDescriptionResponse, CatalogEntry, CatalogEntryKey,
        CatalogEntryMatch, ConstellationResponse, ObjectTypeResponse, Ordering,
        QueryCatalogRequest, QueryCatalogResponse,
    },
    cedar_sky_trait::LocationInfo,
    thread_name::ThreadName,
    wifi_trait::WifiMode as WifiModeDomain,
};
use chrono::offset::Local;
use glob::glob;
use log::{debug, info, warn};
use nix::sys::time::TimeSpec;
use tokio_stream::wrappers::ReceiverStream;

use super::MyCedar;
use crate::{
    bonding_helper::{
        get_adapter_alias, get_bonded_devices as get_bonded_devices_impl,
        remove_bond as remove_bond_impl,
    },
    cpu_stats::CpuStats,
};

// gRPC performance monitoring threshold - log warning if methods take longer
// than this.
const GRPC_SLOW_THRESHOLD_MS: u64 = 100;

// RAII timing guard for gRPC methods - automatically logs duration when
// dropped.
pub(super) struct GrpcTimer {
    method_name: &'static str,
    start: Instant,
    warn_threshold: Duration,
}

impl GrpcTimer {
    pub(super) fn new(method_name: &'static str) -> Self {
        Self::with_threshold(
            method_name,
            Duration::from_millis(GRPC_SLOW_THRESHOLD_MS),
        )
    }

    pub(super) fn with_threshold(
        method_name: &'static str,
        warn_threshold: Duration,
    ) -> Self {
        Self {
            method_name,
            start: Instant::now(),
            warn_threshold,
        }
    }
}

impl Drop for GrpcTimer {
    fn drop(&mut self) {
        let duration = self.start.elapsed();
        if duration > self.warn_threshold {
            warn!("Slow gRPC {}: {:?}", self.method_name, duration);
        } else {
            debug!("gRPC {}: {:?}", self.method_name, duration);
        }
    }
}

pub(super) fn tonic_status(canonical_error: CanonicalError) -> tonic::Status {
    let code = match canonical_error.code {
        CanonicalErrorCode::Unknown => tonic::Code::Unknown,
        CanonicalErrorCode::InvalidArgument => tonic::Code::InvalidArgument,
        CanonicalErrorCode::DeadlineExceeded => tonic::Code::DeadlineExceeded,
        CanonicalErrorCode::NotFound => tonic::Code::NotFound,
        CanonicalErrorCode::AlreadyExists => tonic::Code::AlreadyExists,
        CanonicalErrorCode::PermissionDenied => tonic::Code::PermissionDenied,
        CanonicalErrorCode::Unauthenticated => tonic::Code::Unauthenticated,
        CanonicalErrorCode::ResourceExhausted => tonic::Code::ResourceExhausted,
        CanonicalErrorCode::FailedPrecondition => {
            tonic::Code::FailedPrecondition
        }
        CanonicalErrorCode::Aborted => tonic::Code::Aborted,
        CanonicalErrorCode::OutOfRange => tonic::Code::OutOfRange,
        CanonicalErrorCode::Unimplemented => tonic::Code::Unimplemented,
        CanonicalErrorCode::Internal => tonic::Code::Internal,
        CanonicalErrorCode::Unavailable => tonic::Code::Unavailable,
        CanonicalErrorCode::DataLoss => tonic::Code::DataLoss,
        // canonical_error module does not model Ok or Cancelled.
    };

    // Log RPC errors for monitoring and debugging
    warn!("RPC error: code={:?}, message={}", code, canonical_error.message);

    tonic::Status::new(code, canonical_error.message)
}

// Helper macro to create and log tonic::Status errors
macro_rules! logged_status {
    ($code:ident, $msg:expr) => {{
        let message = $msg;
        warn!("RPC error: code={:?}, message={}", stringify!($code), message);
        tonic::Status::$code(message)
    }};
}
pub(super) use logged_status;

impl MyCedar {
    // Returns files matching `pattern`, sorted newest-to-oldest by mtime.
    fn find_files_newest_first(pattern: &str) -> Vec<PathBuf> {
        let mut files: Vec<(PathBuf, u64)> = Vec::new();

        for entry in glob(pattern).expect("Failed to read glob pattern") {
            match entry {
                Ok(path) => {
                    let metadata =
                        metadata(&path).expect("Failed to read metadata");
                    let modified_time = metadata
                        .modified()
                        .expect("Failed to get modified time")
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs();
                    files.push((path, modified_time));
                }
                Err(e) => {
                    warn!("Error globbing pattern {:?}: {:?}", pattern, e);
                }
            }
        }
        files.sort_by(|a, b| b.1.cmp(&a.1));
        files.into_iter().map(|(path, _)| path).collect()
    }

    // Reads up to `bytes_to_read` bytes of log tail, newest content last.
    // Since the log rotates (e.g. daily), the most-recently-modified file
    // alone may be too small to satisfy `bytes_to_read` (e.g. right after
    // rollover) -- in that case earlier rotated files are also read.
    pub(super) fn read_log_tail(
        log_file: &Path,
        bytes_to_read: i32,
    ) -> io::Result<String> {
        let pat = log_file.to_str().unwrap().to_owned() + ".*";
        let files = Self::find_files_newest_first(&pat);
        if files.is_empty() {
            return Err(io::Error::new(
                ErrorKind::NotFound,
                format!("No match for {:?}", pat),
            ));
        }
        let mut remaining = bytes_to_read as i64;
        let mut chunks: Vec<String> = Vec::new();
        for path in &files {
            if remaining <= 0 {
                break;
            }
            let mut f = fs::File::open(path)?;
            let len = f.metadata()?.len();
            let to_read = std::cmp::min(len as i64, remaining);
            f.seek(SeekFrom::End(-to_read))?;
            let mut content = String::new();
            f.read_to_string(&mut content)?;
            // If we didn't read this file from its start, the leading
            // portion of `content` may be a partial line; discard it up
            // to the first newline.
            if to_read < len as i64 {
                if let Some(pos) = content.find('\n') {
                    content = content[pos + 1..].to_string();
                }
            }
            remaining -= content.len() as i64;
            chunks.push(content);
        }
        // `chunks` is newest-file-first; reverse so output reads
        // oldest-to-newest.
        chunks.reverse();
        Ok(chunks.concat())
    }
}

#[tonic::async_trait]
impl Cedar for MyCedar {
    async fn get_server_log(
        &self,
        request: tonic::Request<ServerLogRequest>,
    ) -> Result<tonic::Response<ServerLogResult>, tonic::Status> {
        let _timer = GrpcTimer::new("get_server_log");

        let req: ServerLogRequest = request.into_inner();
        let tail = MyCedar::read_log_tail(&self.log_file, req.log_request);
        if let Err(e) = tail {
            return Err(logged_status!(
                failed_precondition,
                format!("Error reading log file {:?}: {:?}.", self.log_file, e)
            ));
        }
        let response = cedar_elements::cedar::ServerLogResult {
            log_content: tail.unwrap(),
        };

        Ok(tonic::Response::new(response))
    }

    async fn get_cpu_usage_report(
        &self,
        _request: tonic::Request<EmptyMessage>,
    ) -> Result<tonic::Response<CpuUsageReport>, tonic::Status> {
        let report = CpuStats::top_report().await;
        Ok(tonic::Response::new(CpuUsageReport { report }))
    }

    async fn update_fixed_settings(
        &self,
        request: tonic::Request<FixedSettings>,
    ) -> Result<tonic::Response<FixedSettings>, tonic::Status> {
        let _timer = GrpcTimer::new("update_fixed_settings");

        let req: FixedSettings = request.into_inner();
        if let Some(observer_location) = req.observer_location {
            if !(-90.0..=90.0).contains(&observer_location.latitude) {
                return Err(logged_status!(
                    invalid_argument,
                    format!(
                        "observer_location.latitude must be in -90..90; got {}",
                        observer_location.latitude
                    )
                ));
            }
            let locked_state = self.state.lock().await;
            let fixed_settings_arc = locked_state.fixed_settings.clone();
            let solve_engine_arc = locked_state.solve_engine.clone();
            let serve_engine_arc = locked_state.serve_engine.clone();
            drop(locked_state);
            fixed_settings_arc.lock().await.observer_location =
                Some(observer_location.clone());
            solve_engine_arc
                .lock()
                .await
                .set_observer_location(Some(observer_location.clone()))
                .await;
            let new_fixed_settings = fixed_settings_arc.lock().await.clone();
            serve_engine_arc
                .lock()
                .await
                .update_fixed_settings(new_fixed_settings)
                .await;
            let preferences = Preferences {
                observer_location: Some(observer_location.clone()),
                ..Default::default()
            };
            self.update_preferences(tonic::Request::new(preferences))
                .await?;
            info!("Updated observer location");
        }
        if let Some(current_time) = req.current_time {
            let current_time =
                TimeSpec::new(current_time.seconds, current_time.nanos as i64);
            if let Err(e) = MyCedar::set_server_time(current_time) {
                // Return an error to the client.
                // Note: the cedar-server binary needs CAP_SYS_TIME capability:
                // sudo setcap cap_sys_time+ep <path to cedar-server>
                return Err(logged_status!(
                    permission_denied,
                    format!("Error updating server time: {:?}", e)
                ));
            }
            // Now that we know the correct date/time, initialize the solar
            // system object database.
            if let Some(cedar_sky) = &self.state.lock().await.cedar_sky {
                cedar_sky
                    .lock()
                    .await
                    .initialize_solar_system(SystemTime::now())
                    .await;
            }
            self.state
                .lock()
                .await
                .time_set_by_client
                .store(true, AtomicOrdering::Relaxed);
            info!("Updated server time from client to {:?}", Local::now());
            // Don't store the client time in our fixed_settings state, but
            // arrange to return our current time.
        }
        if let Some(_session_name) = req.session_name {
            return Err(logged_status!(
                unimplemented,
                "rpc UpdateFixedSettings not implemented for session_name."
            ));
        }
        if let Some(_max_exposure_time) = req.max_exposure_time {
            return Err(logged_status!(
                unimplemented,
                "rpc UpdateFixedSettings cannot update max_exposure_time."
            ));
        }
        let fixed_settings_arc = self.state.lock().await.fixed_settings.clone();
        let mut fixed_settings = fixed_settings_arc.lock().await.clone();
        // Fill in our current time.
        MyCedar::fill_in_time(&mut fixed_settings);
        Ok(tonic::Response::new(fixed_settings))
    }

    async fn clear_observer_location(
        &self,
        _request: tonic::Request<EmptyMessage>,
    ) -> Result<tonic::Response<EmptyMessage>, tonic::Status> {
        let _timer = GrpcTimer::new("clear_observer_location");

        // Clear observer location from fixed settings.
        let locked_state = self.state.lock().await;
        let fixed_settings_arc = locked_state.fixed_settings.clone();
        let solve_engine_arc = locked_state.solve_engine.clone();
        let serve_engine_arc = locked_state.serve_engine.clone();
        let preferences_arc = locked_state.preferences.clone();
        drop(locked_state);
        fixed_settings_arc.lock().await.observer_location = None;
        solve_engine_arc
            .lock()
            .await
            .set_observer_location(None)
            .await;
        let new_fixed_settings = fixed_settings_arc.lock().await.clone();
        serve_engine_arc
            .lock()
            .await
            .update_fixed_settings(new_fixed_settings)
            .await;

        // Also clear from preferences for persistence.
        let mut our_prefs = preferences_arc.lock().await.clone();
        our_prefs.observer_location = None;
        *preferences_arc.lock().await = our_prefs.clone();
        self.save_preferences(serve_engine_arc, our_prefs).await;

        info!("Cleared observer location");
        Ok(tonic::Response::new(EmptyMessage {}))
    }

    async fn update_operation_settings(
        &self,
        request: tonic::Request<OperationSettings>,
    ) -> Result<tonic::Response<OperationSettings>, tonic::Status> {
        self.update_operation_settings_impl(request).await
    }

    async fn update_preferences(
        &self,
        request: tonic::Request<Preferences>,
    ) -> Result<tonic::Response<Preferences>, tonic::Status> {
        let _timer = GrpcTimer::new("update_preferences");

        let req: Preferences = request.into_inner();
        if let Some(eyepiece_fov) = req.eyepiece_fov {
            if eyepiece_fov <= 0.0 {
                return Err(logged_status!(
                    invalid_argument,
                    format!(
                        "eyepiece_fov must be positive; got {}",
                        eyepiece_fov
                    )
                ));
            }
        }
        // Hold our lock across this entire operation to ensure that the file
        // update is done one at a time.
        let mut locked_state = self.state.lock().await;

        let mut our_prefs = locked_state.preferences.lock().await.clone();
        if let Some(coord_format) = req.celestial_coord_format {
            our_prefs.celestial_coord_format = Some(coord_format);
        }
        if let Some(eyepiece_fov) = req.eyepiece_fov {
            our_prefs.eyepiece_fov = Some(eyepiece_fov);
            locked_state
                .solve_engine
                .lock()
                .await
                .set_eyepiece_fov(eyepiece_fov)
                .await;
        }
        if let Some(night_vision) = req.night_vision_theme {
            our_prefs.night_vision_theme = Some(night_vision);
        }
        if let Some(hide_app_bar) = req.hide_app_bar {
            our_prefs.hide_app_bar = Some(hide_app_bar);
        }
        if let Some(mount_type) = req.mount_type {
            if self.feature_level == FeatureLevel::Basic {
                return Err(logged_status!(
                    invalid_argument,
                    "Cannot set mount type at Basic feature level"
                ));
            }
            our_prefs.mount_type = Some(mount_type);
        }
        if let Some(observer_location) = req.observer_location {
            our_prefs.observer_location = Some(observer_location);
        }
        if let Some(catalog_entry_match) = req.catalog_entry_match {
            our_prefs.catalog_entry_match = Some(catalog_entry_match);
        }
        if let Some(max_distance_active) = req.max_distance_active {
            our_prefs.max_distance_active = Some(max_distance_active);
        }
        if let Some(max_distance) = req.max_distance {
            our_prefs.max_distance = Some(max_distance);
        }
        if let Some(min_elevation_active) = req.min_elevation_active {
            our_prefs.min_elevation_active = Some(min_elevation_active);
        }
        if let Some(min_elevation) = req.min_elevation {
            our_prefs.min_elevation = Some(min_elevation);
        }
        if let Some(ordering) = req.ordering {
            our_prefs.ordering = Some(ordering);
        }
        if let Some(advanced) = req.advanced {
            our_prefs.advanced = Some(advanced);
        }
        if let Some(text_size_index) = req.text_size_index {
            our_prefs.text_size_index = Some(text_size_index);
        }
        if let Some(boresight_pixel) = req.boresight_pixel {
            our_prefs.boresight_pixel = Some(boresight_pixel);
        }
        if let Some(right_handed) = req.right_handed {
            our_prefs.right_handed = Some(right_handed);
        }
        if let Some(celestial_coord_choice) = req.celestial_coord_choice {
            our_prefs.celestial_coord_choice = Some(celestial_coord_choice);
        }
        if let Some(perf_gauge_choice) = req.perf_gauge_choice {
            our_prefs.perf_gauge_choice = Some(perf_gauge_choice);
        }
        if let Some(screen_always_on) = req.screen_always_on {
            our_prefs.screen_always_on = Some(screen_always_on);
        }
        if let Some(skip_focus) = req.skip_focus {
            our_prefs.skip_focus = Some(skip_focus);
            if skip_focus {
                locked_state.skip_focus_active = true;
            }
        }
        if let Some(skip_alignment) = req.skip_alignment {
            our_prefs.skip_alignment = Some(skip_alignment);
        }
        // Handle dont_show_items array - if non-empty, add items to existing
        // set
        if !req.dont_show_items.is_empty() {
            let mut existing_items = std::collections::HashSet::new();
            existing_items.extend(our_prefs.dont_show_items.clone());
            existing_items.extend(req.dont_show_items.clone());
            our_prefs.dont_show_items = existing_items.into_iter().collect();
        }
        *locked_state.preferences.lock().await = our_prefs.clone();

        // Handle skip_focus being cleared - exit skip-focus retry mode.
        if req.skip_focus == Some(false) && locked_state.skip_focus_active {
            info!(
                "User cleared skip_focus preference, exiting skip-focus mode"
            );

            // Deactivate skip-focus mode (this will cause retry loop to exit).
            locked_state.skip_focus_active = false;

            // If currently calibrating, cancel it.
            if locked_state.calibrating {
                *locked_state.cancel_calibration.lock().await = true;
                locked_state.solver.lock().await.cancel();
            }

            // Wait for skip-focus worker to actually exit.
            drop(locked_state);
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let state = self.state.lock().await;
                if !state.skip_focus_worker_running {
                    locked_state = state;
                    break;
                }
            }

            // Transition to normal SETUP focus assist mode.
            locked_state.operation_settings.focus_assist_mode = Some(true);
            locked_state.operation_settings.operating_mode =
                Some(OperatingMode::Setup as i32);

            let camera = locked_state.camera.clone();
            let initial_exposure_duration =
                locked_state.initial_exposure_duration;
            drop(locked_state);
            MyCedar::set_pre_calibration_defaults(
                &camera,
                initial_exposure_duration,
            )
            .await
            .ok();
            MyCedar::set_gain(&camera, false).await;
            locked_state = self.state.lock().await;

            locked_state
                .detect_engine
                .lock()
                .await
                .set_focus_mode(true)
                .await;
        }

        let serve_engine_arc = locked_state.serve_engine.clone();
        let updated_op_settings = locked_state.operation_settings.clone();
        drop(locked_state);
        serve_engine_arc
            .lock()
            .await
            .update_operation_settings(updated_op_settings)
            .await;
        self.save_preferences(serve_engine_arc, our_prefs.clone())
            .await;

        Ok(tonic::Response::new(our_prefs))
    }

    async fn get_frame(
        &self,
        request: tonic::Request<FrameRequest>,
    ) -> Result<tonic::Response<FrameResult>, tonic::Status> {
        self.get_frame_impl(request).await
    }

    type GetFramesStream = ReceiverStream<Result<FrameResult, tonic::Status>>;

    async fn get_frames(
        &self,
        request: tonic::Request<FrameRequest>,
    ) -> Result<tonic::Response<Self::GetFramesStream>, tonic::Status> {
        self.get_frames_impl(request).await
    }

    type GetImageStream = ReceiverStream<Result<ImageResult, tonic::Status>>;

    async fn get_image(
        &self,
        request: tonic::Request<ImageRequest>,
    ) -> Result<tonic::Response<Self::GetImageStream>, tonic::Status> {
        self.get_image_impl(request).await
    }

    async fn initiate_action(
        &self,
        request: tonic::Request<ActionRequest>,
    ) -> Result<tonic::Response<EmptyMessage>, tonic::Status> {
        self.initiate_action_impl(request).await
    }

    async fn query_catalog_entries(
        &self,
        request: tonic::Request<QueryCatalogRequest>,
    ) -> Result<tonic::Response<QueryCatalogResponse>, tonic::Status> {
        let _timer = GrpcTimer::new("query_catalog_entries");
        let (solve_engine_arc, fixed_settings_arc, cedar_sky_arc) = {
            let locked_state = self.state.lock().await;
            if locked_state.cedar_sky.is_none() {
                return Err(logged_status!(
                    unimplemented,
                    "Cedar Sky is not present"
                ));
            }
            (
                locked_state.solve_engine.clone(),
                locked_state.fixed_settings.clone(),
                locked_state.cedar_sky.clone(),
            )
        }; // State lock released here!

        let req: QueryCatalogRequest = request.into_inner();
        let limit_result = req.limit_result.map(|l| l as usize);
        let ordering = Ordering::try_from(req.ordering.unwrap_or(0))
            .unwrap_or(Ordering::Brightness);
        let ordering = Some(if ordering == Ordering::Unspecified {
            Ordering::Brightness
        } else {
            ordering
        });
        // `catalog_entry_match` may legitimately be absent when
        // `text_search` is given instead (see QueryCatalogRequest in
        // cedar_sky.proto -- query_catalog_entries() ignores it in that
        // case). Absent without text_search is a malformed request.
        let default_catalog_entry_match = CatalogEntryMatch::default();
        let catalog_entry_match = match req.catalog_entry_match.as_ref() {
            Some(cm) => cm,
            None if req.text_search.is_some() => &default_catalog_entry_match,
            None => {
                return Err(logged_status!(
                    invalid_argument,
                    "catalog_entry_match must be given unless text_search is"
                ));
            }
        };

        let plate_solution = solve_engine_arc
            .lock()
            .await
            .get_next_result(None, /* non_blocking= */ false)
            .await
            .unwrap();
        let sky_location =
            if let Some(psp) = plate_solution.plate_solution.as_ref() {
                if !psp.target_sky_coord.is_empty() {
                    Some(psp.target_sky_coord[0].clone())
                } else {
                    psp.image_sky_coord.clone()
                }
            } else {
                None
            };
        let location_info = {
            let fixed_settings = fixed_settings_arc.lock().await;
            fixed_settings.observer_location.as_ref().map(|obs_loc| {
                LocationInfo {
                    observer_location: obs_loc.clone(),
                    observing_time: SystemTime::now(),
                }
            })
        };

        let result = cedar_sky_arc
            .as_ref()
            .unwrap()
            .lock()
            .await
            .query_catalog_entries(
                req.max_distance,
                req.min_elevation,
                catalog_entry_match.faintest_magnitude,
                catalog_entry_match.match_catalog_label,
                &catalog_entry_match.catalog_label,
                catalog_entry_match.match_object_type_label,
                &catalog_entry_match.object_type_label,
                req.text_search,
                ordering,
                req.decrowd_distance,
                limit_result,
                sky_location,
                location_info,
            )
            .await;
        if let Err(e) = result {
            return Err(tonic_status(e));
        }
        let (entries, truncated_count) = result.unwrap();

        let mut response = QueryCatalogResponse::default();
        for entry in entries {
            response.entries.push(entry);
        }
        response.truncated_count = truncated_count as i32;

        Ok(tonic::Response::new(response))
    } // query_catalog_entries().

    async fn get_catalog_entry(
        &self,
        request: tonic::Request<CatalogEntryKey>,
    ) -> Result<tonic::Response<CatalogEntry>, tonic::Status> {
        let _timer = GrpcTimer::new("get_catalog_entry");

        let (fixed_settings_arc, cedar_sky_arc) = {
            let locked_state = self.state.lock().await;
            if locked_state.cedar_sky.is_none() {
                return Err(logged_status!(
                    unimplemented,
                    "Cedar Sky is not present"
                ));
            }
            (
                locked_state.fixed_settings.clone(),
                locked_state.cedar_sky.clone(),
            )
        }; // State lock released here!

        let location_info = {
            let fixed_settings = fixed_settings_arc.lock().await;
            fixed_settings.observer_location.as_ref().map(|obs_loc| {
                LocationInfo {
                    observer_location: obs_loc.clone(),
                    observing_time: SystemTime::now(),
                }
            })
        };

        let req: CatalogEntryKey = request.into_inner();
        let x = cedar_sky_arc
            .as_ref()
            .unwrap()
            .lock()
            .await
            .get_catalog_entry(req, SystemTime::now(), location_info)
            .await;
        match x {
            Ok(entry) => Ok(tonic::Response::new(entry)),
            Err(e) => {
                return Err(tonic_status(e));
            }
        }
    } // get_catalog_entry().

    async fn get_catalog_descriptions(
        &self,
        _request: tonic::Request<EmptyMessage>,
    ) -> Result<tonic::Response<CatalogDescriptionResponse>, tonic::Status>
    {
        let _timer = GrpcTimer::new("get_catalog_descriptions");

        let cedar_sky_arc = {
            let locked_state = self.state.lock().await;
            if locked_state.cedar_sky.is_none() {
                return Err(logged_status!(
                    unimplemented,
                    "Cedar Sky is not present"
                ));
            }
            locked_state.cedar_sky.clone()
        }; // State lock released here!

        let catalog_descriptions = cedar_sky_arc
            .as_ref()
            .unwrap()
            .lock()
            .await
            .get_catalog_descriptions();
        let mut response = CatalogDescriptionResponse::default();
        for cd in catalog_descriptions {
            response.catalog_descriptions.push(cd);
        }

        Ok(tonic::Response::new(response))
    }

    async fn get_object_types(
        &self,
        _request: tonic::Request<EmptyMessage>,
    ) -> Result<tonic::Response<ObjectTypeResponse>, tonic::Status> {
        let _timer = GrpcTimer::new("get_object_types");

        let cedar_sky_arc = {
            let locked_state = self.state.lock().await;
            if locked_state.cedar_sky.is_none() {
                return Err(logged_status!(
                    unimplemented,
                    "Cedar Sky is not present"
                ));
            }
            locked_state.cedar_sky.clone()
        }; // State lock released here!

        let object_types = cedar_sky_arc
            .as_ref()
            .unwrap()
            .lock()
            .await
            .get_object_types();
        let mut response = ObjectTypeResponse::default();
        for ot in object_types {
            response.object_types.push(ot);
        }

        Ok(tonic::Response::new(response))
    }

    async fn get_constellations(
        &self,
        _request: tonic::Request<EmptyMessage>,
    ) -> Result<tonic::Response<ConstellationResponse>, tonic::Status> {
        let _timer = GrpcTimer::new("get_constellations");

        let cedar_sky_arc = {
            let locked_state = self.state.lock().await;
            if locked_state.cedar_sky.is_none() {
                return Err(logged_status!(
                    unimplemented,
                    "Cedar Sky is not present"
                ));
            }
            locked_state.cedar_sky.clone()
        }; // State lock released here!

        let constellations = cedar_sky_arc
            .as_ref()
            .unwrap()
            .lock()
            .await
            .get_constellations();
        let mut response = ConstellationResponse::default();
        for c in constellations {
            response.constellations.push(c);
        }

        Ok(tonic::Response::new(response))
    }

    async fn get_bluetooth_name(
        &self,
        _request: tonic::Request<EmptyMessage>,
    ) -> Result<tonic::Response<GetBluetoothNameResponse>, tonic::Status> {
        // Get the current Bluetooth adapter alias and address.
        let (name, address) = match get_adapter_alias().await {
            Ok((alias, addr)) => (alias, Some(addr)),
            Err(e) => {
                warn!("Unable to get Bluetooth adapter info: {:?}", e);
                ("".to_string(), None)
            }
        };

        Ok(tonic::Response::new(GetBluetoothNameResponse { name, address }))
    }

    async fn get_bonded_devices(
        &self,
        _request: tonic::Request<EmptyMessage>,
    ) -> Result<tonic::Response<GetBondedDevicesResponse>, tonic::Status> {
        let result = get_bonded_devices_impl().await;
        let response = match result {
            Ok(btdevices) => {
                let mut devices: Vec<BondedDevice> = Vec::new();
                for device in btdevices {
                    devices.push(BondedDevice {
                        name: device.name,
                        address: device.address,
                    });
                }
                GetBondedDevicesResponse { devices }
            }
            Err(_) => {
                warn!("Error retrieving bonded devices");
                GetBondedDevicesResponse::default()
            }
        };
        Ok(tonic::Response::new(response))
    }

    async fn remove_bond(
        &self,
        request: tonic::Request<RemoveBondRequest>,
    ) -> Result<tonic::Response<EmptyMessage>, tonic::Status> {
        let req: RemoveBondRequest = request.into_inner();
        let result = remove_bond_impl(req.address).await;
        match result {
            Ok(()) => {}
            Err(_) => {
                warn!("Error removing bond");
            }
        };
        Ok(tonic::Response::new(EmptyMessage::default()))
    }

    async fn scan_wifi(
        &self,
        _request: tonic::Request<EmptyMessage>,
    ) -> Result<tonic::Response<WifiScanResponse>, tonic::Status> {
        let wifi = self.state.lock().await.wifi.clone();
        if wifi.is_none() {
            return Err(logged_status!(
                unimplemented,
                format!("{} does not include WiFi control.", self.product_name)
            ));
        }
        // A scan takes seconds: it sweeps the 2.4GHz channels, and if the
        // radio is rfkill-blocked it must also be unblocked and re-blocked
        // around the scan. Run it on a blocking thread so we don't park an
        // async worker for the duration.
        let wifi_arc = wifi.as_ref().unwrap().clone();
        let result = tokio::task::spawn_blocking(move || {
            let _name = ThreadName::new("wifi-scan");
            wifi_arc.blocking_read().scan_wifi()
        })
        .await
        .map_err(|e| {
            tonic::Status::internal(format!("scan_wifi task panicked: {:?}", e))
        })?;

        let networks = match result {
            Err(x) => return Err(tonic_status(x)),
            Ok(networks) => networks
                .into_iter()
                .map(|n| WifiNetworkProto {
                    ssid: n.ssid,
                    signal_strength: n.signal_strength,
                    secured: n.secured,
                })
                .collect(),
        };
        Ok(tonic::Response::new(WifiScanResponse { networks }))
    }

    async fn set_wifi_mode(
        &self,
        request: tonic::Request<SetWifiModeRequest>,
    ) -> Result<tonic::Response<EmptyMessage>, tonic::Status> {
        let req = request.into_inner();
        let wifi = self.state.lock().await.wifi.clone();
        if wifi.is_none() {
            return Err(logged_status!(
                unimplemented,
                format!("{} does not include WiFi control.", self.product_name)
            ));
        }

        // Translate the proto request into a domain WifiMode, validating the
        // client-mode fields here so an obviously bad request fails fast.
        let (mode, psk) = match WifiModeProto::try_from(req.mode) {
            Ok(WifiModeProto::AccessPoint) => {
                (WifiModeDomain::AccessPoint, None)
            }
            Ok(WifiModeProto::Inactive) => (WifiModeDomain::Inactive, None),
            Ok(WifiModeProto::Client) => {
                let ssid = req
                    .client_ssid
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        logged_status!(
                            invalid_argument,
                            "client mode requires client_ssid".to_string()
                        )
                    })?;
                // An absent or empty client_psk means the network is open.
                let psk = req.client_psk.filter(|s| !s.is_empty());
                (WifiModeDomain::Client { ssid }, psk)
            }
            _ => {
                return Err(logged_status!(
                    invalid_argument,
                    format!("unrecognized wifi mode {}", req.mode)
                ));
            }
        };

        let join_timeout = match req.client_join_timeout {
            None => None,
            Some(d) => Some(Duration::try_from(d).map_err(|e| {
                logged_status!(
                    invalid_argument,
                    format!("invalid client_join_timeout: {:?}", e)
                )
            })?),
        };

        // set_mode returns once the switch is initiated; for client mode the
        // join proceeds on its own and the client polls
        // ServerInformation.wifi_client.state. It still does blocking work
        // (writing a profile, nmcli calls), so run it off the async workers.
        //
        // Note: a WifiLedObserver, registered on this trait object in
        // server_main(), reacts to the resulting mode change (if any) and
        // drives the activity LED; nothing further is needed here.
        let wifi_arc = wifi.as_ref().unwrap().clone();
        let result = tokio::task::spawn_blocking(move || {
            let _name = ThreadName::new("wifi-set-mode");
            wifi_arc.blocking_read().set_mode(
                mode,
                psk.as_deref(),
                join_timeout,
            )
        })
        .await
        .map_err(|e| {
            tonic::Status::internal(format!("set_mode task panicked: {:?}", e))
        })?;
        if let Err(x) = result {
            return Err(tonic_status(x));
        }
        Ok(tonic::Response::new(EmptyMessage::default()))
    }

    async fn convert_to_horizon(
        &self,
        request: tonic::Request<CelestialCoord>,
    ) -> Result<tonic::Response<HorizonCoord>, tonic::Status> {
        let coord = request.into_inner();
        if !(-90.0..=90.0).contains(&coord.dec) {
            return Err(logged_status!(
                invalid_argument,
                format!("dec must be in -90..90; got {}", coord.dec)
            ));
        }
        let observer_location = self
            .state
            .lock()
            .await
            .fixed_settings
            .clone()
            .lock()
            .await
            .observer_location
            .clone()
            .ok_or_else(|| {
                logged_status!(
                    failed_precondition,
                    "Observer location is not known"
                )
            })?;
        let horizon = horizon_coord_from_celestial(
            &coord,
            observer_location.latitude.to_radians(),
            observer_location.longitude.to_radians(),
            &SystemTime::now(),
        );
        Ok(tonic::Response::new(horizon))
    }

    async fn convert_to_celestial(
        &self,
        request: tonic::Request<HorizonCoord>,
    ) -> Result<tonic::Response<CelestialCoord>, tonic::Status> {
        let horizon = request.into_inner();
        if !(-90.0..=90.0).contains(&horizon.altitude) {
            return Err(logged_status!(
                invalid_argument,
                format!(
                    "altitude must be in -90..90; got {}",
                    horizon.altitude
                )
            ));
        }
        let observer_location = self
            .state
            .lock()
            .await
            .fixed_settings
            .clone()
            .lock()
            .await
            .observer_location
            .clone()
            .ok_or_else(|| {
                logged_status!(
                    failed_precondition,
                    "Observer location is not known"
                )
            })?;
        let coord = celestial_coord_from_horizon(
            &horizon,
            observer_location.latitude.to_radians(),
            observer_location.longitude.to_radians(),
            &SystemTime::now(),
        );
        Ok(tonic::Response::new(coord))
    }

    async fn set_pairing_mode(
        &self,
        request: tonic::Request<SetPairingModeRequest>,
    ) -> Result<tonic::Response<EmptyMessage>, tonic::Status> {
        let req = request.into_inner();
        let state = self.state.lock().await;
        {
            let mut pairing_mode = state.pairing_mode.lock().await;
            *pairing_mode = req.enabled;
        }
        if req.enabled {
            *state.pairing_mode_forever.lock().await = req.forever;
            if req.forever {
                info!("Entering pairing mode via RPC, will remain enabled");
            } else {
                MyCedar::spawn_pairing_mode_timer(
                    state.pairing_mode.clone(),
                    state.pairing_mode_forever.clone(),
                    state.pairing_mode_generation.clone(),
                )
                .await;
            }
        } else {
            info!("Exiting pairing mode via RPC");
        }
        Ok(tonic::Response::new(EmptyMessage::default()))
    }
} // impl Cedar for MyCedar.
