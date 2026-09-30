// Copyright (c) 2026 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering as AtomicOrdering},
        Arc, Mutex as StdMutex,
    },
    time::{Duration, Instant, SystemTime},
};

use axum::Router;
use canonical_error::CanonicalError;
use cedar_camera::{
    abstract_camera::AbstractCamera, image_camera::ImageCamera,
    select_camera::{select_camera, CameraInterface},
};
use cedar_elements::{
    astro_util::{alt_az_from_equatorial, celestial_coord_from_horizon},
    cedar::{
        cedar_server::CedarServer, CalibrationData, CelestialCoordFormat,
        DetectSensitivity, FeatureLevel, FixedSettings, ImageCoord, LatLong,
        MountType, OperatingMode, OperationSettings,
        PlateSolution as PlateSolutionProto, Preferences, Rectangle,
    },
    cedar_common::{CelestialCoord, HorizonCoord},
    cedar_sky::{CatalogEntryMatch, Ordering},
    cedar_sky_trait::CedarSkyTrait,
    hot_pixel_trait::HotPixelTrait,
    imu_trait::ImuTrait,
    solver_trait::SolverTrait,
    wifi_trait::{
        WifiClientState as WifiClientStateDomain, WifiMode as WifiModeDomain,
        WifiModeObserver, WifiTrait,
    },
};
use chrono::offset::Local;
use futures::join;
use image::{GrayImage, ImageReader};
use log::{error, info, warn};
use nix::{
    sys::time::TimeSpec,
    time::{clock_gettime, clock_settime, ClockId},
};
use pico_args::Arguments;
use prost::Message;
use tetra3_server::tetra3_solver::Tetra3Solver;
use tonic_web::GrpcWebLayer;
use tower_http::{
    cors::{Any, CorsLayer},
    services::ServeDir,
};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{fmt, prelude::*, registry, EnvFilter};

mod actions;
mod bluetooth_transport;
mod calibration;
mod cedar_rpcs;
mod frame_capture;
mod operation_settings;
mod server_info;

use bluetooth_transport::{accept_with_sndbuf, serve_over_bt, ConnectionTrackingMakeService};

use cedar_rpcs::logged_status;

use self::multiplex_service::MultiplexService;
use crate::{
    activity_led::{ActivityLed, BlinkPattern},
    bonding_helper::{run_pairing_mode, set_adapter_name},
    calibrator::Calibrator,
    cpu_stats::CpuStats,
    detect_engine::{DetectEngine, DetectResult},
    lx200_server::create_lx200_server,
    motion_estimator::MotionEstimator,
    polar_analyzer::PolarAnalyzer,
    position_reporter::{create_alpaca_server, TelescopePosition},
    serve_engine::{ServeContext, ServeEngine},
    solve_engine::{SlewTargetInfo, SolveEngine},
};

// Minimum interval between frames delivered to the pipeline in focus assist
// mode. Focus assist can have short exposure (bright sky) that deliver frames
// far faster than the focus display is useful at, and causes the processing
// pipeline to do work at a high rate. The camera keeps running at its natural
// rate and discards the frames in between.
const FOCUS_ASSIST_UPDATE_INTERVAL: Duration = Duration::from_millis(50);

// Duration before auto-exiting pairing mode when forever==false.
const PAIRING_MODE_EXIT_DELAY_SECS: u64 = 300; // 5 minutes.

/// Per-connection info tracked for cedar_server clients. Keyed by peer
/// address in `ConnectionCounters::cedar_wifi_clients`/
/// `cedar_bluetooth_clients`; the client address is the map key, not
/// duplicated here.
#[derive(Default, Clone)]
pub(crate) struct ClientEntry {
    /// Device model reported by the client via the
    /// `x-cedar-client-device-model` gRPC metadata header, if any.
    pub(crate) device_model: Option<String>,
}

/// Identifies a specific cedar_server client connection, so request
/// handlers can look up (and update) that connection's entry in
/// `ConnectionCounters::cedar_wifi_clients`/`cedar_bluetooth_clients`.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ConnectionKey {
    Wifi(SocketAddr),
    Bluetooth(bluer::rfcomm::SocketAddr),
}

/// Shared state for tracking active client connections across all servers:
/// per-client maps for cedar_server clients, plain counts for LX200.
#[derive(Default)]
pub(crate) struct ConnectionCounters {
    /// Live cedar_server WiFi connections, keyed by peer address.
    pub(crate) cedar_wifi_clients: StdMutex<HashMap<SocketAddr, ClientEntry>>,
    /// Live cedar_server Bluetooth connections, keyed by peer address.
    pub(crate) cedar_bluetooth_clients:
        StdMutex<HashMap<bluer::rfcomm::SocketAddr, ClientEntry>>,
    pub(crate) lx200_wifi: AtomicU32,
    pub(crate) lx200_bluetooth: AtomicU32,
}

/// Notifies the activity LED and (if present) the Wifi implementation that an
/// RPC was received.
async fn note_rpc_received(
    activity_led: &Arc<tokio::sync::Mutex<ActivityLed>>,
    wifi: &Option<Arc<tokio::sync::RwLock<dyn WifiTrait + Send + Sync>>>,
) {
    activity_led.lock().await.received_rpc();
    if let Some(wifi) = wifi {
        wifi.read().await.received_rpc();
    }
}

/// The name identifying this device: its access point's SSID, which is what
/// the user sees when joining our hotspot. Also used as the Bluetooth name
/// and the mDNS hostname, so the device is known by one name everywhere.
///
/// Falls back to the processor serial number when no access point is
/// configured, since the SSID is where the per-device part comes from.
fn device_name(ap_ssid: Option<String>, serial_number: &str) -> String {
    match ap_ssid {
        Some(ssid) => ssid,
        None if serial_number.len() >= 3 => {
            format!("cedar-{}", &serial_number[serial_number.len() - 3..])
        }
        None => "cedar".to_string(),
    }
}

/// Drives the activity LED's blink pattern from WiFi mode changes: fast in
/// Client mode, regular otherwise.
struct WifiLedObserver {
    activity_led: Arc<tokio::sync::Mutex<ActivityLed>>,
}

impl WifiModeObserver for WifiLedObserver {
    fn on_mode_changed(
        &self,
        _old_mode: &WifiModeDomain,
        new_mode: &WifiModeDomain,
    ) {
        // blocking_lock(): this is called from Wifi's sync mode-transition
        // code, which itself only ever runs on a spawn_blocking task or a
        // detached thread -- never an async worker -- so blocking here is
        // safe, same as the wifi_arc.blocking_read() calls elsewhere.
        let locked_activity_led = self.activity_led.blocking_lock();
        locked_activity_led.set_blink_pattern(
            if matches!(new_mode, WifiModeDomain::Client { .. }) {
                BlinkPattern::Fast
            } else {
                BlinkPattern::Regular
            },
        );
        locked_activity_led.resume_blinking();
    }
}

struct MyCedar {
    // We organize our state as a sub-object so update_operation_settings() can
    // spawn a sub-task for the SETUP -> OPERATE mode transition; the sub-task
    // needs access to our state.
    state: Arc<tokio::sync::Mutex<CedarState>>,

    // Fake camera for using static image instead of an attached camera.
    test_image_camera:
        Option<Arc<tokio::sync::Mutex<Box<dyn AbstractCamera + Send>>>>,

    // Demo images, if any, that were found in ./demo_images directory.
    demo_images: Vec<String>,

    preferences_file: PathBuf,

    // The full path to our log file.
    log_file: PathBuf,

    product_name: String,
    copyright: String,
    feature_level: FeatureLevel,

    cedar_version: String,
    processor_model: String,
    os_version: String,
    serial_number: String,

    connection_counters: Arc<ConnectionCounters>,

    cpu_stats: Arc<CpuStats>,
}

struct CedarState {
    // The plate solver we are using.
    solver: Arc<tokio::sync::Mutex<dyn SolverTrait + Send + Sync>>,

    // The hardware camera that was detected, if any.
    attached_camera:
        Option<Arc<tokio::sync::Mutex<Box<dyn AbstractCamera + Send>>>>,

    // An exposure duration which is a good starting point for
    // `attached_camera`.
    initial_exposure_duration: Duration,

    // The `camera` field is always populated with a usable AbstractCamera.
    // This will be one of:
    // * attached_camera
    // * a test image configured on command line
    // * a demo mode image
    // * a uniform gray image if none of the above are available.
    camera: Arc<tokio::sync::Mutex<Box<dyn AbstractCamera + Send>>>,

    fixed_settings: Arc<tokio::sync::Mutex<FixedSettings>>,
    calibration_data: Arc<tokio::sync::Mutex<CalibrationData>>,
    operation_settings: OperationSettings,
    detect_engine: Arc<tokio::sync::Mutex<DetectEngine>>,
    solve_engine: Arc<tokio::sync::Mutex<SolveEngine>>,
    serve_engine: Arc<tokio::sync::Mutex<ServeEngine>>,
    calibrator: Arc<tokio::sync::Mutex<Calibrator>>,
    telescope_position: Arc<tokio::sync::Mutex<TelescopePosition>>,
    activity_led: Arc<tokio::sync::Mutex<ActivityLed>>,

    // Target of an alt/az goto, if one is active. Unlike an RA/Dec goto, this
    // is a Cedar-only concept: it is not conveyed to SkySafari/Stellarium,
    // which have no way to express a target fixed in the horizon frame. Kept
    // out of TelescopePosition for that reason. `None` means no alt/az goto is
    // active.
    alt_az_slew_target: Arc<tokio::sync::Mutex<Option<HorizonCoord>>>,

    // Not all builds of Cedar-server support Cedar-sky.
    cedar_sky: Option<Arc<tokio::sync::Mutex<dyn CedarSkyTrait + Send>>>,

    // Not all builds of Cedar-server support Wifi control.
    wifi: Option<Arc<tokio::sync::RwLock<dyn WifiTrait + Send + Sync>>>,

    // Not all builds of Cedar-server support IMU fusion.
    imu_tracker: Option<Arc<tokio::sync::Mutex<dyn ImuTrait + Send>>>,

    // Not all builds of Cedar-server support hot pixel mapping.
    hot_pixel_map: Option<Arc<tokio::sync::Mutex<dyn HotPixelTrait + Send>>>,

    // We host the user interface preferences and some operation settings here.
    // On startup we apply some of these to `operation_settings`; we reflect
    // them out to all clients and persist them to a server-side file.
    preferences: Arc<tokio::sync::Mutex<Preferences>>,

    // Snapshot of the last serve engine result, captured when calibration
    // begins. Used to show a frozen image during calibration.
    scaled_image: Option<Arc<GrayImage>>,
    scaled_image_binning_factor: u32,
    scaled_image_frame_id: i32,
    // The rectangle ServeEngine paired with `scaled_image` (a centered square
    // crop of the sensor, in full resolution coordinates). Snapshotted so the
    // frozen calibration image is described to clients with the same geometry
    // the live view used, rather than a fabricated full-sensor rectangle.
    scaled_image_rectangle: Option<Rectangle>,

    calibrating: bool,
    cancel_calibration: Arc<tokio::sync::Mutex<bool>>,
    // Relevant only if calibration is underway (`calibration_image` is
    // present).
    calibration_start: Instant,
    calibration_duration_estimate: Duration,

    // When true, server is in skip-focus auto-calibration mode with retries.
    skip_focus_active: bool,
    // When true, the skip-focus worker task is still running.
    skip_focus_worker_running: bool,
    // Timestamp of last calibration attempt (for skip-focus retry interval).
    skip_focus_last_attempt: Option<Instant>,

    // Some command line args.
    args_binning: Option<u32>,
    args_display_sampling: Option<bool>,

    // Bluetooth pairing mode state. The run_pairing_mode() loop in
    // bonding_helper polls this every 5 seconds and updates the adapter's
    // discoverable/pairable state accordingly.
    pairing_mode: Arc<tokio::sync::Mutex<bool>>,

    // When true, pairing mode will remain enabled indefinitely (not
    // auto-exit).
    pairing_mode_forever: Arc<tokio::sync::Mutex<bool>>,

    // Incremented each time a timed pairing window is started. A spawned
    // auto-exit timer compares its captured generation against this value
    // before clearing pairing_mode; if they differ, a newer window has
    // superseded it and the timer does nothing.
    pairing_mode_generation: Arc<tokio::sync::Mutex<u64>>,

    // Set to true when the client (mobile app) has provided the date/time via
    // update_fixed_settings(). When true, time updates from the telescope
    // (SkySafari/Stellarium) are ignored, since the client time is more
    // reliable.
    time_set_by_client: Arc<AtomicBool>,

    // Set to true when initiate_action is saving state prior to shutdown or
    // restart, so the signal handler (triggered by the subsequent shutdown
    // command) skips its own save_state calls and avoids a deadlock.
    saving_state: Arc<AtomicBool>,
}

impl MyCedar {
    // Increments the pairing mode generation counter and spawns a task that
    // clears pairing_mode after PAIRING_MODE_EXIT_DELAY_SECS, unless a newer
    // timer or forever mode supersedes it. Caller is responsible for setting
    // pairing_mode=true before calling.
    async fn spawn_pairing_mode_timer(
        pairing_mode: Arc<tokio::sync::Mutex<bool>>,
        pairing_mode_forever: Arc<tokio::sync::Mutex<bool>>,
        pairing_mode_generation: Arc<tokio::sync::Mutex<u64>>,
    ) {
        let gen = {
            let mut g = pairing_mode_generation.lock().await;
            *g += 1;
            *g
        };
        info!(
            "Enabling pairing mode, will auto-exit in {} seconds",
            PAIRING_MODE_EXIT_DELAY_SECS
        );
        tokio::task::spawn(async move {
            tokio::time::sleep(Duration::from_secs(
                PAIRING_MODE_EXIT_DELAY_SECS,
            ))
            .await;
            if *pairing_mode_forever.lock().await {
                return; // A subsequent call upgraded to forever mode.
            }
            if *pairing_mode_generation.lock().await != gen {
                return; // A newer timed window superseded us.
            }
            let mut pm = pairing_mode.lock().await;
            if *pm {
                *pm = false;
                info!("Auto-exiting pairing mode");
            }
        });
    }

    fn get_demo_images() -> Result<Vec<String>, tonic::Status> {
        let dir = Path::new("./demo_images");
        if !dir.exists() {
            return Err(logged_status!(
                failed_precondition,
                format!("The path {:?} is not found", dir)
            ));
        }
        if !dir.is_dir() {
            return Err(logged_status!(
                failed_precondition,
                format!("The path {:?} is not a directory", dir)
            ));
        }
        let mut response = Vec::<String>::new();
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let extension = path.extension().unwrap_or_default();
            if extension == "jpg" || extension == "bmp" {
                let file_name = path.file_name().unwrap().to_str().unwrap();
                response.push(file_name.to_string());
            }
        }
        Ok(response)
    }

    fn write_preferences_file(
        preferences_file: &PathBuf,
        preferences: &Preferences,
    ) {
        // Write updated preferences to file.
        let prefs_path = Path::new(preferences_file);
        let scratch_path = prefs_path.with_extension("tmp");

        let mut buf = vec![];
        if let Err(e) = preferences.encode(&mut buf) {
            warn!("Could not encode preferences: {:?}", e);
            return;
        }
        if let Err(e) = fs::write(&scratch_path, buf) {
            warn!("Could not write file {:?}: {:?}", &scratch_path, e);
            return;
        }
        if let Err(e) = fs::rename(&scratch_path, prefs_path) {
            warn!(
                "Could not rename file {:?} to {:?}: {:?}",
                &scratch_path, &prefs_path, e
            );
        }
    }

    async fn save_preferences(
        &self,
        serve_engine_arc: Arc<tokio::sync::Mutex<ServeEngine>>,
        preferences: Preferences,
    ) {
        Self::write_preferences_file(&self.preferences_file, &preferences);
        serve_engine_arc
            .lock()
            .await
            .update_preferences(preferences)
            .await;
    }

    fn fill_in_time(fixed_settings: &mut FixedSettings) {
        if let Ok(cur_time) = clock_gettime(ClockId::CLOCK_REALTIME) {
            let pst = prost_types::Timestamp {
                seconds: cur_time.tv_sec(),
                nanos: cur_time.tv_nsec() as i32,
            };
            fixed_settings.current_time = Some(pst);
        }
    }

    // MyCedar::new().
    pub async fn new(
        solver: Arc<tokio::sync::Mutex<dyn SolverTrait + Send + Sync>>,
        args_binning: Option<u32>,
        args_display_sampling: Option<bool>,
        initial_exposure_duration: Duration,
        min_exposure_duration: Duration,
        mut max_exposure_duration: Duration,
        activity_led: Arc<tokio::sync::Mutex<ActivityLed>>,
        attached_camera: Option<
            Arc<tokio::sync::Mutex<Box<dyn AbstractCamera + Send>>>,
        >,
        test_image_camera: Option<
            Arc<tokio::sync::Mutex<Box<dyn AbstractCamera + Send>>>,
        >,
        camera: Arc<tokio::sync::Mutex<Box<dyn AbstractCamera + Send>>>,
        telescope_position: Arc<tokio::sync::Mutex<TelescopePosition>>,
        base_star_count_goal: i32,
        base_detection_sigma: f64,
        stats_capacity: usize,
        preferences_file: PathBuf,
        log_file: PathBuf,
        product_name: &str,
        copyright: &str,
        feature_level: FeatureLevel,
        cedar_sky: Option<Arc<tokio::sync::Mutex<dyn CedarSkyTrait + Send>>>,
        wifi: Option<Arc<tokio::sync::RwLock<dyn WifiTrait + Send + Sync>>>,
        imu_tracker: Option<Arc<tokio::sync::Mutex<dyn ImuTrait + Send>>>,
        hot_pixel_map: Option<
            Arc<tokio::sync::Mutex<dyn HotPixelTrait + Send>>,
        >,
        saving_state: Arc<AtomicBool>,
    ) -> Result<Self, CanonicalError> {
        let cedar_version = env!("CARGO_PKG_VERSION");
        // These device-tree files exist only on Raspberry Pi; on x86 dev hosts
        // they are absent, so default to empty rather than panicking.
        let processor_model =
            fs::read_to_string("/sys/firmware/devicetree/base/model")
                .unwrap_or_default()
                .trim_end_matches('\0')
                .to_string();
        let serial_number =
            fs::read_to_string("/sys/firmware/devicetree/base/serial-number")
                .unwrap_or_default()
                .trim_end_matches('\0')
                .to_string();
        let reader = BufReader::new(fs::File::open("/etc/os-release").unwrap());
        let mut os_version: String = "".to_string();
        for line in reader.lines() {
            let line = line.unwrap();
            if line.starts_with("PRETTY_NAME=") {
                let parts: Vec<&str> = line.split('=').collect();
                os_version = parts[1].trim_matches('"').to_string();
                break;
            }
        }
        info!("{}", &product_name);
        info!("{}", &copyright);
        info!(
            "Cedar server version {} running on {}/{}",
            &cedar_version, &processor_model, &os_version
        );
        info!("Processor serial number {}", &serial_number);

        if let Some(attached_camera) = &attached_camera {
            let locked_camera = attached_camera.lock().await;
            let model = locked_camera.model().await;
            if model == "ov5647" || model == "imx219" {
                max_exposure_duration *= 3; // This camera is less sensitive.
            }
            if model == "imx219" {
                // IMX219 VBLANK tops out at 65535 lines with PPL=3448, giving
                // ~1.24s max. Cap here so the auto-exposure loop doesn't waste
                // time requesting exposures the sensor cannot deliver.
                max_exposure_duration =
                    max_exposure_duration.min(Duration::from_millis(1240));
            }
        }

        let detect_engine =
            Arc::new(tokio::sync::Mutex::new(DetectEngine::new(
                initial_exposure_duration,
                min_exposure_duration,
                max_exposure_duration,
                base_detection_sigma,
                base_star_count_goal,
                camera.clone(),
                stats_capacity,
                hot_pixel_map.clone(),
            )));

        // Everything the sky catalog offers, asked of the catalog rather than
        // listed here, so that one added later is selected without this
        // needing to be revisited. Empty if Cedar Sky is not present.
        let (known_catalog_labels, known_object_type_labels) = match &cedar_sky
        {
            Some(cedar_sky) => {
                let sky = cedar_sky.lock().await;
                (
                    sky.get_catalog_descriptions()
                        .into_iter()
                        .map(|d| d.label)
                        .collect::<Vec<String>>(),
                    sky.get_object_types()
                        .into_iter()
                        .map(|t| t.label)
                        .collect::<Vec<String>>(),
                )
            }
            None => (Vec::new(), Vec::new()),
        };

        // Set up initial Preferences to use if preferences file cannot be
        // loaded.
        let mut preferences = Preferences {
            celestial_coord_format: Some(CelestialCoordFormat::HmsDms.into()),
            eyepiece_fov: Some(1.0),
            night_vision_theme: Some(false),
            hide_app_bar: Some(true),
            mount_type: Some(MountType::AltAz.into()),
            observer_location: None,
            catalog_entry_match: if cedar_sky.is_some() {
                let mut cat_match = Some(CatalogEntryMatch {
                    faintest_magnitude: match feature_level {
                        FeatureLevel::Plus => Some(12), // Max 20.
                        FeatureLevel::Basic => Some(8), // Max 12.
                        _ => Some(10),                  /* Irrelevant, no
                                                          * Cedar Sky. */
                    },
                    match_catalog_label: true,
                    catalog_label: Vec::<String>::new(), // Filled below.
                    match_object_type_label: true,
                    object_type_label: Vec::<String>::new(), // Filled below.
                });
                let cm_ref = cat_match.as_mut().unwrap();
                // WDS is large enough (double/multiple stars down to faint
                // magnitudes) that including it by default would flood the
                // view; leave it opt-in. Matches the WDS exclusion below,
                // in the existing-preferences-file merge path.
                cm_ref.catalog_label = known_catalog_labels
                    .iter()
                    .filter(|label| label.as_str() != "WDS")
                    .cloned()
                    .collect();
                cm_ref.object_type_label = known_object_type_labels.clone();
                cat_match
            } else {
                None // No Cedar sky.
            },
            max_distance_active: Some(false),
            max_distance: Some(60.0),
            min_elevation_active: Some(true),
            min_elevation: Some(20.0),
            ordering: Some(Ordering::Brightness.into()),
            advanced: Some(false),
            text_size_index: Some(0),
            boresight_pixel: None,
            right_handed: Some(true),
            celestial_coord_choice: None,
            perf_gauge_choice: None,
            screen_always_on: Some(true),
            dont_show_items: Vec::new(),
            skip_focus: None,
            skip_alignment: None,
            // Left empty here whether or not Cedar Sky is present: a
            // preferences file supplies these when it has them, and either
            // way they are stamped from the sky catalog further below, once
            // the file has been merged.
            known_catalog_label: Vec::new(),
            known_object_type_label: Vec::new(),
        };

        // If there is a preferences file, read it and merge its contents into
        // initial `preferences`. Fields that are present in the both
        // `preferences` and the preferences file will replace those in initial
        // `preferences`.

        // Load UI preferences file.
        let prefs_path = Path::new(&preferences_file);
        let file_prefs_bytes = fs::read(prefs_path);
        if let Err(e) = file_prefs_bytes {
            warn!("Could not read file {:?}: {:?}", preferences_file, e);
        } else {
            match Preferences::decode(
                file_prefs_bytes.as_ref().unwrap().as_slice(),
            ) {
                Ok(mut file_prefs) => {
                    if let Some(fov) = file_prefs.eyepiece_fov {
                        if fov < 0.1 {
                            file_prefs.eyepiece_fov = Some(0.1);
                        }
                        if fov > 2.0 {
                            file_prefs.eyepiece_fov = Some(2.0);
                        }
                    }
                    if file_prefs.catalog_entry_match.is_some() {
                        // The protobuf merge() function accumulates into
                        // repeated fields of the destination; we don't want
                        // this.
                        preferences.catalog_entry_match = None;
                    }
                    // Same accumulation problem, for the same reason.
                    preferences.known_catalog_label.clear();
                    preferences.known_object_type_label.clear();
                    preferences.merge(&*file_prefs_bytes.unwrap()).unwrap();
                }
                Err(e) => {
                    warn!("Could not decode preferences {:?}", e);
                }
            }
        }
        let (width, height) = camera.lock().await.dimensions().await;
        let inset = 16;
        if let Some(ref bsp) = preferences.boresight_pixel {
            // Validate boresight_pixel loaded from preferences (full-sensor
            // coords), to make sure it is within the image area.
            // This could be violated if e.g. we changed camera
            // since the preferences were saved.
            if bsp.x < inset as f64
                || bsp.x > (width - inset) as f64
                || bsp.y < inset as f64
                || bsp.y > (height - inset) as f64
            {
                preferences.boresight_pixel = None;
            }
        }
        // Validate preferences against feature level. If someone switches the
        // camera down to the basic model, some preferences need to be adjusted.
        let limit_magnitude = match feature_level {
            FeatureLevel::Plus => 20,
            FeatureLevel::Basic => 12,
            _ => 20, // DIY.
        };
        if let Some(ref cm) = preferences.catalog_entry_match {
            if cm.faintest_magnitude.unwrap_or(0) > limit_magnitude {
                preferences
                    .catalog_entry_match
                    .as_mut()
                    .unwrap()
                    .faintest_magnitude = Some(limit_magnitude);
            }
        }
        if feature_level == FeatureLevel::Basic {
            preferences.mount_type = Some(MountType::AltAz.into());
        }
        // Select any catalog or object type the sky catalog has gained since
        // these preferences were written. A label the user deselected is
        // recorded in known_*_label and left alone; one appearing in neither
        // list is new, and is selected so that a catalog added by a server
        // upgrade does not stay invisible behind an option the user never
        // saw. See Preferences.known_catalog_label in cedar.proto.
        if cedar_sky.is_some() {
            // A preferences file written before these fields existed has
            // neither list, but we know what the catalog offered at that
            // time: seed them with it, so that whatever such a user
            // deselected is still recognized as deselected. Without this,
            // every label missing from their selection would look new.
            //
            // These are the labels as they stood then, not as they stand
            // now: 'double star' has since been renamed 'multiple star', and
            // seeding the current name would make it look pre-existing and
            // leave it unselected.
            if preferences.known_catalog_label.is_empty() {
                preferences.known_catalog_label =
                    ["M", "NGC", "IC", "IAU", "PL"]
                        .iter()
                        .map(|s| s.to_string())
                        .collect();
            }
            if preferences.known_object_type_label.is_empty() {
                preferences.known_object_type_label = [
                    "star",
                    "double star",
                    "star association",
                    "open cluster",
                    "globular cluster",
                    "star cluster + nebula",
                    "galaxy",
                    "galaxy pair",
                    "galaxy triplet",
                    "galaxy group",
                    "planetary nebula",
                    "HII ionized region",
                    "dark nebula",
                    "emission nebula",
                    "nebula",
                    "reflection nebula",
                    "supernova remnant",
                    "nova star",
                    "planet",
                    "dwarf planet",
                ]
                .iter()
                .map(|s| s.to_string())
                .collect();
            }
            // Drop labels the sky catalog no longer offers. A label that has
            // been renamed or retired would otherwise sit in the file for
            // ever, and query_catalog_entries rejects an entire query on one
            // unrecognized label rather than ignoring it -- so leaving
            // 'double star' behind after it became 'multiple star' empties
            // the field of view.
            let mut dropped_something = false;
            if let Some(cm) = preferences.catalog_entry_match.as_mut() {
                let before =
                    cm.catalog_label.len() + cm.object_type_label.len();
                cm.catalog_label.retain(|label| {
                    let known = known_catalog_labels.contains(label);
                    if !known {
                        info!("Dropping retired catalog '{}'", label);
                    }
                    known
                });
                cm.object_type_label.retain(|label| {
                    let known = known_object_type_labels.contains(label);
                    if !known {
                        info!("Dropping retired object type '{}'", label);
                    }
                    known
                });
                dropped_something = cm.catalog_label.len()
                    + cm.object_type_label.len()
                    != before;
            }
            let mut selected_something = false;
            if let Some(cm) = preferences.catalog_entry_match.as_mut() {
                for label in &known_catalog_labels {
                    // WDS is large enough (double/multiple stars down to
                    // faint magnitudes) that auto-selecting it would flood
                    // the view for existing users; leave it opt-in. Matches
                    // the fresh-install default above.
                    if label == "WDS" {
                        continue;
                    }
                    if !preferences.known_catalog_label.contains(label)
                        && !cm.catalog_label.contains(label)
                    {
                        info!("Selecting newly added catalog '{}'", label);
                        cm.catalog_label.push(label.clone());
                        selected_something = true;
                    }
                }
                for label in &known_object_type_labels {
                    if !preferences.known_object_type_label.contains(label)
                        && !cm.object_type_label.contains(label)
                    {
                        info!("Selecting newly added object type '{}'", label);
                        cm.object_type_label.push(label.clone());
                        selected_something = true;
                    }
                }
            }
            // The known lists can change without anything being selected --
            // a catalog dropped rather than added, or no catalog_entry_match
            // to select into.
            let known_changed = preferences.known_catalog_label
                != known_catalog_labels
                || preferences.known_object_type_label
                    != known_object_type_labels;
            preferences.known_catalog_label = known_catalog_labels;
            preferences.known_object_type_label = known_object_type_labels;
            if selected_something || dropped_something || known_changed {
                // Record what the catalog offers now, so this runs once
                // rather than on every startup. Without it the file keeps
                // its old lists until the user happens to change some other
                // preference.
                Self::write_preferences_file(&preferences_file, &preferences);
            }
        }

        let shared_preferences = Arc::new(tokio::sync::Mutex::new(preferences));
        let copied_preferences = shared_preferences.lock().await.clone();

        let fixed_settings = Arc::new(tokio::sync::Mutex::new(FixedSettings {
            observer_location: copied_preferences.observer_location.clone(),
            current_time: None,
            session_name: None,
            max_exposure_time: Some(
                prost_types::Duration::try_from(max_exposure_duration).unwrap(),
            ),
        }));

        let polar_analyzer =
            Arc::new(tokio::sync::Mutex::new(PolarAnalyzer::new()));

        let time_set_by_client = Arc::new(AtomicBool::new(false));

        let alt_az_slew_target: Arc<tokio::sync::Mutex<Option<HorizonCoord>>> =
            Arc::new(tokio::sync::Mutex::new(None));

        // Define callback invoked from SolveEngine().
        let closure_fixed_settings = fixed_settings.clone();
        let closure_preferences = shared_preferences.clone();
        let closure_preferences_file = preferences_file.clone();
        let closure_telescope_position = telescope_position.clone();
        let motion_estimator =
            Arc::new(tokio::sync::Mutex::new(MotionEstimator::new(
                // gap_tolerance=
                Duration::from_secs(3),
                // bump_tolerance=
                Duration::from_secs_f64(2.0),
            )));
        let closure_polar_analyzer = polar_analyzer.clone();

        // Pre-solve callback: gets the current slew target and sync coordinates
        // before the plate solve runs.
        let pre_solve_telescope_position = closure_telescope_position.clone();
        let pre_solve_alt_az_slew_target = alt_az_slew_target.clone();
        let pre_solve_fixed_settings = closure_fixed_settings.clone();
        let pre_solve_callback = Arc::new(
            move |frame_time: SystemTime| -> std::pin::Pin<
                Box<
                    dyn std::future::Future<
                            Output = (
                                Option<SlewTargetInfo>,
                                Option<CelestialCoord>,
                            ),
                        > + Send,
                >,
            > {
                let telescope_pos = pre_solve_telescope_position.clone();
                let alt_az_slew_target = pre_solve_alt_az_slew_target.clone();
                let fixed_settings = pre_solve_fixed_settings.clone();
                Box::pin(async move {
                    let mut locked_telescope_pos = telescope_pos.lock().await;
                    let mut locked_alt_az = alt_az_slew_target.lock().await;
                    let observer_location =
                        fixed_settings.lock().await.observer_location.clone();

                    let slew_target = Self::select_slew_target(
                        locked_telescope_pos.slew_active,
                        locked_telescope_pos.slew_target_ra,
                        locked_telescope_pos.slew_target_dec,
                        &mut locked_alt_az,
                        observer_location.as_ref(),
                        &frame_time,
                    );
                    let sync_coord = if locked_telescope_pos.sync_ra.is_some()
                        && locked_telescope_pos.sync_dec.is_some()
                    {
                        Some(CelestialCoord {
                            ra: locked_telescope_pos.sync_ra.unwrap(),
                            dec: locked_telescope_pos.sync_dec.unwrap(),
                            epoch: None,
                        })
                    } else {
                        None
                    };
                    // Clear sync flags after reading them.
                    if sync_coord.is_some() {
                        info!("Telescope synced boresight to {:?}", sync_coord);
                        locked_telescope_pos.sync_ra = None;
                        locked_telescope_pos.sync_dec = None;
                    }
                    (slew_target, sync_coord)
                })
            },
        );

        // Post-solve callback: processes the plate solution results and updates
        // server state (motion estimator, polar analyzer, preferences).
        let post_solve_closure_fixed_settings = closure_fixed_settings.clone();
        let post_solve_closure_preferences = closure_preferences.clone();
        let post_solve_closure_preferences_file =
            closure_preferences_file.clone();
        let post_solve_closure_telescope_position =
            closure_telescope_position.clone();
        let post_solve_motion_estimator = motion_estimator.clone();
        let post_solve_closure_polar_analyzer = closure_polar_analyzer.clone();
        let post_solve_time_set_by_client = time_set_by_client.clone();
        let post_solve_callback = Arc::new(
            move |boresight_pixel: Option<ImageCoord>,
                  detect_result: Option<DetectResult>,
                  plate_solution: Option<PlateSolutionProto>|
                  -> std::pin::Pin<
                Box<dyn std::future::Future<Output = Option<LatLong>> + Send>,
            > {
                Box::pin(Self::post_solve_callback(
                    boresight_pixel,
                    detect_result,
                    plate_solution,
                    post_solve_closure_fixed_settings.clone(),
                    post_solve_closure_preferences.clone(),
                    post_solve_closure_preferences_file.clone(),
                    post_solve_closure_telescope_position.clone(),
                    post_solve_motion_estimator.clone(),
                    post_solve_closure_polar_analyzer.clone(),
                    post_solve_time_set_by_client.clone(),
                ))
            },
        );

        let solve_engine = Arc::new(tokio::sync::Mutex::new(
            SolveEngine::new(
                solver.clone(),
                cedar_sky.clone(),
                hot_pixel_map.clone(),
                imu_tracker.clone(),
                detect_engine.clone(),
                stats_capacity,
                pre_solve_callback,
                post_solve_callback,
                copied_preferences.observer_location.clone(),
            )
            .unwrap(),
        ));
        // Shared calibration data arc: used by both CedarState and ServeContext
        // so the serve engine always reads up-to-date calibration results.
        let shared_calibration_data =
            Arc::new(tokio::sync::Mutex::new(CalibrationData {
                ..Default::default()
            }));
        let initial_serve_context = ServeContext {
            fixed_settings: closure_fixed_settings.lock().await.clone(),
            preferences: copied_preferences.clone(),
            operation_settings: OperationSettings {
                operating_mode: Some(OperatingMode::Setup as i32),
                daylight_mode: Some(false),
                focus_assist_mode: Some(true),
                ..Default::default()
            },
            calibration_data: shared_calibration_data.clone(),
            imu_tracker: imu_tracker.clone(),
            hot_pixel_map: hot_pixel_map.clone(),
            polar_analyzer: closure_polar_analyzer.clone(),
            jpeg_quality: 75,
            landscape: false,
        };
        let serve_engine = Arc::new(tokio::sync::Mutex::new(ServeEngine::new(
            solve_engine.clone(),
            detect_engine.clone(),
            initial_serve_context,
            stats_capacity,
        )));

        let state = {
            Arc::new(tokio::sync::Mutex::new(CedarState {
                solver: solver.clone(),
                attached_camera: attached_camera.clone(),
                camera: camera.clone(),
                initial_exposure_duration,
                fixed_settings,
                operation_settings: OperationSettings {
                    operating_mode: Some(OperatingMode::Setup as i32),
                    daylight_mode: Some(false),
                    focus_assist_mode: Some(true),
                    log_dwelled_positions: Some(false),
                    catalog_entry_match: copied_preferences
                        .catalog_entry_match
                        .clone(),
                    demo_image_filename: None,
                    use_imu: Some(imu_tracker.is_some()),
                    // Deliberately not taken from preferences; sensitivity
                    // tracks current sky conditions, not user preference.
                    detect_sensitivity: Some(DetectSensitivity::Normal as i32),
                },
                calibration_data: shared_calibration_data,
                detect_engine: detect_engine.clone(),
                solve_engine: solve_engine.clone(),
                serve_engine: serve_engine.clone(),
                calibrator: Arc::new(tokio::sync::Mutex::new(Calibrator::new(
                    camera.clone(),
                    hot_pixel_map.clone(),
                ))),
                telescope_position,
                activity_led,
                alt_az_slew_target,
                cedar_sky,
                wifi,
                imu_tracker,
                hot_pixel_map,
                preferences: shared_preferences.clone(),
                scaled_image: None,
                scaled_image_binning_factor: 1,
                scaled_image_frame_id: 0,
                scaled_image_rectangle: None,
                calibrating: false,
                cancel_calibration: Arc::new(tokio::sync::Mutex::new(false)),
                calibration_start: Instant::now(),
                calibration_duration_estimate: Duration::MAX,
                skip_focus_active: false,
                skip_focus_worker_running: false,
                skip_focus_last_attempt: None,
                args_binning,
                args_display_sampling,
                pairing_mode: Arc::new(tokio::sync::Mutex::new(false)),
                pairing_mode_forever: Arc::new(tokio::sync::Mutex::new(false)),
                pairing_mode_generation: Arc::new(tokio::sync::Mutex::new(0)),
                time_set_by_client,
                saving_state,
            }))
        };

        let mut demo_images: Vec<String> = vec![];
        match Self::get_demo_images() {
            Ok(d) => {
                demo_images = d;
            }
            Err(x) => {
                warn!("Could not enumerate demo images {:?}", x);
            }
        }

        let connection_counters = Arc::new(ConnectionCounters::default());
        let cedar = MyCedar {
            state: state.clone(),
            test_image_camera: test_image_camera.clone(),
            demo_images,
            preferences_file,
            log_file,
            product_name: product_name.to_string(),
            copyright: copyright.to_string(),
            feature_level,
            cedar_version: cedar_version.to_string(),
            processor_model,
            os_version,
            serial_number,
            connection_counters: connection_counters.clone(),
            cpu_stats: Arc::new(CpuStats::new()),
        };
        // Set pre-calibration defaults on camera.
        let locked_state = state.lock().await;
        let camera = locked_state.camera.clone();
        drop(locked_state);
        let (width, height) = Self::camera_geometry(&camera).await;
        let locked_state = state.lock().await;
        let (detect_binning, display_sampling) =
            Self::compute_binning(&locked_state, width, height);
        let initial_exposure_duration = locked_state.initial_exposure_duration;
        drop(locked_state);
        if let Err(x) = Self::set_pre_calibration_defaults(
            &camera,
            initial_exposure_duration,
        )
        .await
        {
            warn!("Could not set default settings on camera {:?}", x);
        }
        let mut locked_state = state.lock().await;

        {
            let mut detect_engine = locked_state.detect_engine.lock().await;
            detect_engine
                .set_detect_binning(detect_binning, display_sampling)
                .await;
            detect_engine
                .set_focus_mode(
                    locked_state.operation_settings.focus_assist_mode.unwrap(),
                )
                .await;
            detect_engine
                .set_daylight_mode(
                    locked_state.operation_settings.daylight_mode.unwrap(),
                )
                .await;
        }
        // Apply the frame rate cap for the mode we are starting up in. The
        // update_interval sites elsewhere only fire on mode *transitions*.
        if let Err(x) = Self::set_update_interval(
            &locked_state,
            Self::get_automatic_update_interval(&locked_state),
        )
        .await
        {
            warn!("Could not set initial update interval {:?}", x);
        }
        {
            let mut solve_engine = locked_state.solve_engine.lock().await;
            solve_engine
                .set_catalog_entry_match(
                    copied_preferences.catalog_entry_match.clone(),
                )
                .await;
            if let Some(eyepiece_fov) = copied_preferences.eyepiece_fov {
                solve_engine.set_eyepiece_fov(eyepiece_fov).await;
            }
            solve_engine.set_align_mode(true).await;
            if let Some(bsp) = &copied_preferences.boresight_pixel {
                let inset = 16;
                if bsp.x >= inset as f64
                    && bsp.x <= (width - inset) as f64
                    && bsp.y >= inset as f64
                    && bsp.y <= (height - inset) as f64
                {
                    solve_engine
                        .set_boresight_pixel(Some(bsp.clone()))
                        .await
                        .unwrap();
                } else {
                    warn!(
                        "Saved boresight_pixel {:?} is out of range; ignoring.",
                        bsp
                    );
                }
            }
        }

        // Check if skip_focus is enabled in preferences.
        let skip_focus = copied_preferences.skip_focus.unwrap_or(false);
        let skip_alignment = copied_preferences.skip_alignment.unwrap_or(false);
        if skip_focus {
            info!(
                "Skip-focus mode enabled, \
                starting auto-calibration (skip_alignment={})",
                skip_alignment
            );
            // Set initial state for skip-focus mode.
            locked_state.skip_focus_active = true;
            locked_state.operation_settings.focus_assist_mode = Some(true);

            // Configure detect engine for non-focus mode.
            {
                let mut detect_engine = locked_state.detect_engine.lock().await;
                detect_engine.set_focus_mode(true).await;
                detect_engine.set_daylight_mode(false).await;
            }

            // Release lock before spawning the calibration task.
            drop(locked_state);

            Self::spawn_skip_focus_calibration(state.clone(), skip_alignment);
        }

        Ok(cedar)
    } // MyCedar::new().

    // Chooses the goto target for a frame, given the RA/Dec goto state (shared
    // with SkySafari/Stellarium via TelescopePosition) and any alt/az goto
    // target.
    //
    // Only one goto is active at a time. A goto initiated from
    // SkySafari/Stellarium supersedes an alt/az goto: those clients bypass
    // initiate_action() and set `slew_active` directly, so because
    // initiate_slew_alt_az() clears that flag, seeing it set again means a new
    // RA/Dec goto has arrived. `alt_az_slew_target` is cleared in that case.
    //
    // Returns None if no goto is active, or if an alt/az goto is active but the
    // observer location is unknown, which is what ties an alt/az target to a
    // position on the celestial sphere.
    fn select_slew_target(
        slew_active: bool,
        slew_target_ra: f64,
        slew_target_dec: f64,
        alt_az_slew_target: &mut Option<HorizonCoord>,
        observer_location: Option<&LatLong>,
        frame_time: &SystemTime,
    ) -> Option<SlewTargetInfo> {
        if alt_az_slew_target.is_some() && slew_active {
            info!("Telescope goto supersedes alt/az goto");
            *alt_az_slew_target = None;
        }
        if let Some(alt_az) = alt_az_slew_target.clone() {
            // A fixed alt/az target's equatorial position changes as the earth
            // turns, so convert afresh for this frame.
            observer_location.map(|loc| SlewTargetInfo {
                coord: celestial_coord_from_horizon(
                    &alt_az,
                    loc.latitude.to_radians(),
                    loc.longitude.to_radians(),
                    frame_time,
                ),
                alt_az: Some(alt_az),
            })
        } else if slew_active {
            Some(SlewTargetInfo {
                coord: CelestialCoord {
                    ra: slew_target_ra,
                    dec: slew_target_dec,
                    epoch: None,
                },
                alt_az: None,
            })
        } else {
            None
        }
    }

    async fn post_solve_callback(
        boresight_pixel: Option<ImageCoord>,
        detect_result: Option<DetectResult>,
        plate_solution: Option<PlateSolutionProto>,
        fixed_settings: Arc<tokio::sync::Mutex<FixedSettings>>,
        preferences: Arc<tokio::sync::Mutex<Preferences>>,
        preferences_file: PathBuf,
        telescope_position: Arc<tokio::sync::Mutex<TelescopePosition>>,
        motion_estimator: Arc<tokio::sync::Mutex<MotionEstimator>>,
        polar_analyzer: Arc<tokio::sync::Mutex<PolarAnalyzer>>,
        time_set_by_client: Arc<AtomicBool>,
    ) -> Option<LatLong> {
        // Notice when solve engine has recently changed its boresight due
        // to the pre-solve callback function reporting a telescope sync.
        let mut prefs_to_save: Option<Preferences> = None;
        let mut updated_observer_location: Option<LatLong> = None;
        if let Some(bp) = boresight_pixel {
            let cedar_bp = bp.clone();
            let mut locked_preferences = preferences.lock().await;
            if locked_preferences.boresight_pixel.is_none()
                || cedar_bp
                    != *locked_preferences.boresight_pixel.as_ref().unwrap()
            {
                // Save in preferences (full-sensor coords).
                locked_preferences.boresight_pixel = Some(cedar_bp);
                // Flag updated preferences to write to file below.
                prefs_to_save = Some(locked_preferences.clone());
            }
        }
        if plate_solution.is_none() {
            telescope_position.lock().await.boresight_valid = false;
            if let Some(detect_result) = detect_result {
                motion_estimator.lock().await.add(
                    &detect_result.captured_image.readout_instant,
                    None,
                    None,
                );
            }
        } else {
            let plate_solution = plate_solution.unwrap();
            // Update telescope interface with our position.
            let coords = if plate_solution.target_sky_coord.is_empty() {
                plate_solution.image_sky_coord.as_ref().unwrap().clone()
            } else {
                plate_solution.target_sky_coord[0].clone()
            };
            let mut locked_telescope_position = telescope_position.lock().await;
            locked_telescope_position.boresight_ra = coords.ra;
            locked_telescope_position.boresight_dec = coords.dec;
            locked_telescope_position.boresight_valid = true;
            let captured_image = &detect_result.unwrap().captured_image;
            let readout_time = &captured_image.readout_time;
            let readout_instant = &captured_image.readout_instant;
            motion_estimator.lock().await.add(
                readout_instant,
                Some(coords.clone()),
                Some(plate_solution.rmse),
            );

            // Has telescope reported the site geolocation?
            if locked_telescope_position.site_latitude.is_some()
                && locked_telescope_position.site_longitude.is_some()
            {
                let latitude = locked_telescope_position.site_latitude.unwrap();
                let longitude =
                    locked_telescope_position.site_longitude.unwrap();
                locked_telescope_position.site_latitude = None;
                locked_telescope_position.site_longitude = None;
                // Normalize longitude into -180..180, in case the client
                // used a 0..360 convention or reported a wrapped value.
                let longitude = longitude.rem_euclid(360.0);
                let longitude = if longitude > 180.0 {
                    longitude - 360.0
                } else {
                    longitude
                };
                // SkySafari/Stellarium report (0, 0) when they have no real
                // location (e.g. a mobile device with GPS unavailable and no
                // location manually set), so reject that as a sentinel rather
                // than a legitimate position.
                if latitude == 0.0 && longitude == 0.0 {
                    warn!(
                        "Ignoring telescope-reported observer location (0, 0)"
                    );
                } else if !(-90.0..=90.0).contains(&latitude) {
                    warn!(
                        "Ignoring invalid telescope-reported observer \
                         location: latitude={}, longitude={}",
                        latitude, longitude
                    );
                } else {
                    let observer_location = LatLong {
                        latitude,
                        longitude,
                    };
                    fixed_settings.lock().await.observer_location =
                        Some(observer_location.clone());
                    updated_observer_location = Some(observer_location.clone());
                    info!("Telescope updated observer location");
                    // Save in preferences.
                    let mut locked_preferences = preferences.lock().await;
                    locked_preferences.observer_location =
                        Some(observer_location.clone());
                    // Flag updated preferences to write to file below.
                    prefs_to_save = Some(locked_preferences.clone());
                }
            }
            // Has telescope reported the time?
            if let Some(dt) = locked_telescope_position.utc_date.take() {
                if let Ok(duration) = dt.duration_since(std::time::UNIX_EPOCH) {
                    if time_set_by_client.load(AtomicOrdering::Relaxed) {
                        let telescope_secs = duration.as_secs() as i64;
                        if let Ok(cur_time) =
                            clock_gettime(ClockId::CLOCK_REALTIME)
                        {
                            if (cur_time.tv_sec() - telescope_secs).abs() > 60 {
                                warn!(
                                    "Ignoring telescope time update; \
                                    times differ by more than a minute. \
                                    Current time: {:?}, telescope time: {:?}",
                                    Local::now(),
                                    dt
                                );
                            } else {
                                info!(
                                    "Ignoring telescope time update; \
                                         client has already set the time"
                                );
                            }
                        } else {
                            info!(
                                "Ignoring telescope time update; \
                                client has already set the time"
                            );
                        }
                    } else {
                        _ = Self::set_server_time(TimeSpec::new(
                            duration.as_secs() as i64,
                            duration.subsec_nanos() as i64,
                        ));
                        info!(
                            "Updated server time from telescope to {:?}",
                            Local::now()
                        );
                    }
                } else {
                    warn!("Unable to set server time to {:?}", dt);
                }
            }

            let geo_location = &fixed_settings.lock().await.observer_location;
            if let Some(geo_location) = geo_location {
                let lat = geo_location.latitude.to_radians();
                let long = geo_location.longitude.to_radians();
                let bs_ra = coords.ra.to_radians();
                let bs_dec = coords.dec.to_radians();
                // alt/az of boresight. Also boresight hour angle.
                let (_alt, _az, ha) = alt_az_from_equatorial(
                    bs_ra,
                    bs_dec,
                    lat,
                    long,
                    readout_time,
                );
                let motion_estimate =
                    motion_estimator.lock().await.get_estimate();
                polar_analyzer.lock().await.process_solution(
                    &coords,
                    ha.to_degrees(),
                    geo_location.latitude,
                    &motion_estimate,
                );
            }
        }
        if let Some(prefs) = prefs_to_save {
            // Write updated preferences to file.
            Self::write_preferences_file(&preferences_file, &prefs);
        }
        updated_observer_location
    }

    fn set_server_time(
        current_time: TimeSpec,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Err(e) = clock_settime(ClockId::CLOCK_REALTIME, current_time) {
            if let Ok(cur_time) = clock_gettime(ClockId::CLOCK_REALTIME) {
                // If our current time is close to the client's time, just
                // warn.
                if (cur_time.tv_sec() - current_time.tv_sec()).abs() < 60 {
                    warn!("Could not update server time: {:?}", e);
                } else {
                    error!("Could not update server time: {:?}", e);
                }
            }
            Err(Box::new(e))
        } else {
            Ok(())
        }
    }
} // impl MyCedar.

// About Resolutions
//
// Cedar is designed to support a wide variety of cameras. It has been
// extensively tested with two rather different camera sensors:
//
// ASI120mm mini (AR0130CS):         1.2 megapixel, mono,  6.0 mm diagonal
// Raspberry Pi HQ camera (IMX477): 12.3 megapixel, color, 7.9 mm diagonal
//
// Cedar works very well with low resolution cameras such as the ASI mini. The
// high pixel resolution of the HQ camera presents some challenges:
//
// * Star images are typically highly oversampled (spread out over many pixels)
//   and often exceed CedarDetect's star profile shape window, and thus are not
//   detected.
// * The HQ image has too-high resolution for the CedarAim phone UI. A half
//   megapixel or less is adequate for good UI rendering.
// * Sending the HQ image to the phone UI takes too long.
//
// We thus employ image downsizing at various points in the processing chain.
//
// For the HQ camera, we employ 4x4 binning in order to fit CedarDetect's star
// profile shape window. Note that star candidates are then referenced to the
// full-resolution original capture for high-accuracy centroiding.
//
// An additional 2x2 sampling is used when sending HQ images to the CedarAim
// phone UI. For the HQ camera, the result is a 0.2 megapixel display image
// (around 500x375), which is adequate to provide a background for visualizing
// the plate solve result (this can be overridden with a command line flag e.g.
// for a tablet UI; see below).
//
// For the ASI mini camera, we apply 2x2 binning prior to CedarDetect, and refer
// star detections to the full resolution capture for centroiding. The binned
// image (0.3 megapixel) is sent to the phone UI.
//
// Rather than hardwiring the above image downsizing strategies for the HQ
// camera and the ASI mini camera, we instead generalize based on the camera
// sensor resolution:
//
// Camera  Mpix     CedarDetect    Display
//         <0.75
// ASI     0.75-3   2x2 binning
//         3-12     4x4 binning
// HQ      >12      4x4 binning    +2x2 sampling

// Note that the "display" sampling value is an additional sampling (if any)
// applied after the CedarDetect binning has been applied.
//
// Command line arguments are provided to allow overrides to be applied to the
// above rubric.

#[derive(Debug)]
struct AppArgs {
    tetra3_script: String,
    tetra3_database: String,
    camera_interface: String,
    camera_index: usize,
    binning: Option<u32>,
    display_sampling: Option<bool>,
    test_image: Option<String>,
    min_exposure: Duration,
    max_exposure: Duration,
    star_count_goal: i32,
    sigma: f64,
    ui_prefs: String,
    log_dir: String,
    log_file: String,
    // TODO: max solve time
}

fn parse_duration(
    arg: &str,
) -> Result<std::time::Duration, std::num::ParseFloatError> {
    let seconds = arg.parse()?;
    Ok(std::time::Duration::from_secs_f64(seconds))
}

// `get_dependencies` Is called to obtain the CedarSkyTrait, WifiTrait,
//     ImuTrait, HotPixelTrait, and SolverTrait implementations, and the
//     camera, if any. A camera returned here is used instead of the one
//     select_camera() would find. This function is called after logging has
//     been set up and `server_main()`s command line arguments have been
//     consumed, and before the tokio runtime is started.
//     The AtomicBool is set to true if control-c occurs.
pub fn server_main(
    copyright: &str,
    flutter_app_path: &str,
    get_dependencies: fn(
        Arguments,
    ) -> (
        Option<Arc<tokio::sync::Mutex<dyn CedarSkyTrait + Send>>>,
        Option<Arc<tokio::sync::RwLock<dyn WifiTrait + Send + Sync>>>,
        Option<Arc<tokio::sync::Mutex<dyn ImuTrait + Send>>>,
        Option<Arc<tokio::sync::Mutex<dyn HotPixelTrait + Send>>>,
        Option<Arc<tokio::sync::Mutex<dyn SolverTrait + Send + Sync>>>,
        Option<Arc<tokio::sync::Mutex<Box<dyn AbstractCamera + Send>>>>,
    ),
    // Default total binning to use when --binning is not passed on the command
    // line.
    default_total_binning: Option<u32>,
    product_name_override: Option<&str>,
) {
    #[rustfmt::skip]
    const HELP: &str = "\
    FLAGS:
      -h, --help                     Prints help information

    OPTIONS:
      --tetra3_script <path>         ../cedar/tetra3_server/python/tetra3_server.py
      --tetra3_database <name>       default_database
      --camera_interface asi|rpi
      --camera_index NUMBER
      --binning 1|2|4|8
      --display_sampling true|false
      --test_image <path>
      --min_exposure NUMBER          0.00001
      --max_exposure NUMBER          1.0
      --star_count_goal NUMBER       20
      --sigma NUMBER                 8.0
      --ui_prefs <path>              ./cedar_ui_prefs.binpb
      --log_dir <path>               .
      --log_file <file>              cedar_log.txt
    ";

    let mut pargs = Arguments::from_env();
    if pargs.contains(["-h", "--help"]) {
        println!("{}", HELP);
        std::process::exit(0);
    }
    let args = AppArgs {
        tetra3_script: pargs.value_from_str("--tetra3_script").unwrap_or(
            "../cedar/tetra3_server/python/tetra3_server.py".to_string(),
        ),
        tetra3_database: pargs
            .value_from_str("--tetra3_database")
            .unwrap_or("default_database".to_string()),
        camera_interface: pargs
            .value_from_str("--camera_interface")
            .unwrap_or("".to_string()),
        camera_index: pargs.value_from_str("--camera_index").unwrap_or(0),
        binning: pargs.opt_value_from_str("--binning").unwrap(),
        display_sampling: pargs
            .opt_value_from_str("--display_sampling")
            .unwrap(),
        test_image: pargs.opt_value_from_str("--test_image").unwrap(),
        min_exposure: pargs
            .value_from_fn("--min_exposure", parse_duration)
            .unwrap_or(parse_duration("0.00001").unwrap()),
        max_exposure: pargs
            .value_from_fn("--max_exposure", parse_duration)
            .unwrap_or(parse_duration("1.0").unwrap()),
        star_count_goal: pargs
            .value_from_str("--star_count_goal")
            .unwrap_or(20),
        sigma: pargs.value_from_str("--sigma").unwrap_or(8.0),
        ui_prefs: pargs
            .value_from_str("--ui_prefs")
            .unwrap_or("./cedar_ui_prefs.binpb".to_string()),
        log_dir: pargs.value_from_str("--log_dir").unwrap_or(".".to_string()),
        log_file: pargs
            .value_from_str("--log_file")
            .unwrap_or("cedar_log.txt".to_string()),
    };

    // Set up logging.
    let file_appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix(&args.log_file)
        .max_log_files(10)
        .build(&args.log_dir)
        .unwrap();

    // Use blocking (synchronous) writers so each log entry is accepted by
    // the OS before returning. The non-blocking variant buffers in a worker
    // thread whose destructor does not run on process::exit() or _exit(),
    // causing the last entries to be lost on shutdown or crash.
    registry()
        .with(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with(fmt::layer().with_writer(std::io::stdout))
        .with(fmt::layer().with_ansi(false).with_writer(file_appender))
        .init();
    let remaining = pargs.finish();

    let got_signal = Arc::new(AtomicBool::new(false));
    let saving_state = Arc::new(AtomicBool::new(false));

    let (cedar_sky, wifi, imu_tracker, hot_pixel_map, solver, injected_camera) =
        get_dependencies(Arguments::from_vec(remaining));

    // Handle both SIGINT and SIGTERM (the latter is sent by systemd on
    // `systemctl restart`, e.g. when the updater installs a new version).
    let got_signal2 = got_signal.clone();
    let saving_state2 = saving_state.clone();
    let imu_for_signal = imu_tracker.clone();
    let hpm_for_signal = hot_pixel_map.clone();
    ctrlc::set_handler(move || {
        if saving_state2.load(AtomicOrdering::Relaxed) {
            // initiate_action already saved state; just exit.
            info!("Got shutdown signal; state already saved, exiting");
        } else {
            info!("Got shutdown signal, saving state");
            prepare_for_exit(&imu_for_signal, &hpm_for_signal);
        }
        got_signal2.store(true, AtomicOrdering::Relaxed);
        std::thread::sleep(Duration::from_secs(1));
        info!("Exiting");
        // Use _exit() rather than exit() to skip C++ static destructors.
        // exit() triggers libcamera's static unordered_map destructors while
        // the Tokio runtime may still have live camera handles making libcamera
        // calls, causing an intermittent unordered_map::at crash.
        unsafe {
            libc::_exit(0);
        }
    })
    .unwrap();

    // Derive product name from device verification status (indicated by whether
    // cedar_sky is Some), unless overridden by caller.
    let product_name = product_name_override.unwrap_or_else(|| {
        if cedar_sky.is_some() {
            "Hopper"
        } else {
            "Cedar-Box"
        }
    });

    async_main(
        args,
        product_name,
        copyright,
        flutter_app_path,
        got_signal,
        saving_state,
        cedar_sky,
        wifi,
        imu_tracker,
        hot_pixel_map,
        solver,
        injected_camera,
        default_total_binning,
    );
}

/// Perform cleanup actions before process exit (signal, shutdown, or restart).
/// Must only be called from outside a Tokio runtime (e.g. the signal handler).
fn prepare_for_exit(
    imu_tracker: &Option<Arc<tokio::sync::Mutex<dyn ImuTrait + Send>>>,
    hot_pixel_map: &Option<Arc<tokio::sync::Mutex<dyn HotPixelTrait + Send>>>,
) {
    if let Some(imu) = imu_tracker {
        if let Err(e) = imu.blocking_lock().save_state() {
            warn!("Failed to save IMU state: {:?}", e);
        }
    }
    if let Some(hpm) = hot_pixel_map {
        if let Err(e) = hpm.blocking_lock().save_state() {
            warn!("Failed to save hot pixel map state: {:?}", e);
        }
    }
}

/// Async version for use within a Tokio runtime (shutdown/restart RPC
/// handlers).
async fn prepare_for_exit_async(
    imu_tracker: &Option<Arc<tokio::sync::Mutex<dyn ImuTrait + Send>>>,
    hot_pixel_map: &Option<Arc<tokio::sync::Mutex<dyn HotPixelTrait + Send>>>,
) {
    if let Some(imu) = imu_tracker {
        if let Err(e) = imu.lock().await.save_state() {
            warn!("Failed to save IMU state: {:?}", e);
        }
    }
    if let Some(hpm) = hot_pixel_map {
        if let Err(e) = hpm.lock().await.save_state() {
            warn!("Failed to save hot pixel map state: {:?}", e);
        }
    }
}

async fn get_attached_camera(
    camera_interface: Option<&CameraInterface>,
    camera_index: usize,
) -> Result<Box<dyn AbstractCamera + Send>, CanonicalError> {
    select_camera(camera_interface, camera_index).await
}

async fn get_camera(
    attached_camera: &Option<
        Arc<tokio::sync::Mutex<Box<dyn AbstractCamera + Send>>>,
    >,
    test_image_camera: &Option<
        Arc<tokio::sync::Mutex<Box<dyn AbstractCamera + Send>>>,
    >,
) -> Arc<tokio::sync::Mutex<Box<dyn AbstractCamera + Send>>> {
    if let Some(test_image_camera) = test_image_camera {
        return test_image_camera.clone();
    }
    if let Some(attached_camera) = attached_camera {
        return attached_camera.clone();
    }
    // Fake up a uniform grey ImageCamera.
    let width = 800;
    let height = 600;
    let pixels = vec![16_u8; width * height];
    let img_u8 =
        GrayImage::from_vec(width as u32, height as u32, pixels).unwrap();

    Arc::new(tokio::sync::Mutex::new(Box::new(
        ImageCamera::new(img_u8).await.unwrap(),
    )))
}

// Tokio names its async workers and its spawn_blocking pool alike
// ("tokio-rt-worker"), so blocking threads rename themselves via
// a ThreadName guard inside the closure; see the spawn_blocking call sites.
#[tokio::main]
async fn async_main(
    args: AppArgs,
    product_name: &str,
    copyright: &str,
    flutter_app_path: &str,
    got_signal: Arc<AtomicBool>,
    saving_state: Arc<AtomicBool>,
    cedar_sky: Option<Arc<tokio::sync::Mutex<dyn CedarSkyTrait + Send>>>,
    wifi: Option<Arc<tokio::sync::RwLock<dyn WifiTrait + Send + Sync>>>,
    imu_tracker: Option<Arc<tokio::sync::Mutex<dyn ImuTrait + Send>>>,
    hot_pixel_map: Option<Arc<tokio::sync::Mutex<dyn HotPixelTrait + Send>>>,
    injected_solver: Option<
        Arc<tokio::sync::Mutex<dyn SolverTrait + Send + Sync>>,
    >,
    injected_camera: Option<
        Arc<tokio::sync::Mutex<Box<dyn AbstractCamera + Send>>>,
    >,
    default_total_binning: Option<u32>,
) {
    // If any thread panics, bail out.
    std::panic::set_hook(Box::new(|panic_info| {
        error!("Thread panicked: {}", panic_info);
        std::process::exit(1);
    }));

    if let Some(imu) = &imu_tracker {
        imu.lock().await.start();
    }

    let camera_interface = match args.camera_interface.as_str() {
        "" => None,
        "asi" => Some(CameraInterface::ASI),
        "rpi" => Some(CameraInterface::Rpi),
        _ => {
            error!(
                "Unrecognized 'camera_interface' value: {}",
                args.camera_interface
            );
            std::process::exit(1);
        }
    };

    let attached_camera = if let Some(camera) = injected_camera {
        if camera_interface.is_some() {
            warn!(
                "Ignoring 'camera_interface'; a camera was supplied by \
                   get_dependencies"
            );
        }
        Some(camera)
    } else {
        match get_attached_camera(camera_interface.as_ref(), args.camera_index)
            .await
        {
            Ok(cam) => Some(Arc::new(tokio::sync::Mutex::new(cam))),
            Err(e) => {
                error!("Could not select camera: {:?}", e);
                None
            }
        }
    };

    let test_image_camera: Option<
        Arc<tokio::sync::Mutex<Box<dyn AbstractCamera + Send>>>,
    > = match &args.test_image {
        Some(test_image_path) => {
            let input_path = PathBuf::from(test_image_path);
            let img = ImageReader::open(&input_path).unwrap().decode().unwrap();
            let img_u8 = img.to_luma8();
            info!("Using test image {} instead of camera.", test_image_path);
            Some(Arc::new(tokio::sync::Mutex::new(Box::new(
                ImageCamera::new(img_u8).await.unwrap(),
            ))))
        }
        None => None,
    };

    let feature_level = if product_name.eq_ignore_ascii_case("Hopper") {
        if let Some(attached_camera) = &attached_camera {
            let camera_model = attached_camera.lock().await.model().await;
            if camera_model == "imx296" || camera_model == "imx290" {
                FeatureLevel::Plus // Hopper.
            } else {
                FeatureLevel::Basic // Hopper LE.
            }
        } else {
            FeatureLevel::Diy
        }
    } else {
        FeatureLevel::Diy
    };

    let effective_binning = args.binning.or(default_total_binning);
    if let Some(binning_arg) = effective_binning {
        match binning_arg {
            1 | 2 | 4 | 8 => (),
            _ => {
                error!(
                    "Invalid binning argument {}, must be 1, 2, 4, or 8",
                    binning_arg
                );
                std::process::exit(1);
            }
        }
    }

    let camera = get_camera(&attached_camera, &test_image_camera).await;
    if let Err(e) = camera.lock().await.start().await {
        error!("Failed to start initial camera: {:?}", e);
        std::process::exit(1);
    }
    {
        let locked_camera = camera.lock().await;
        info!(
            "Using camera {} {}x{}",
            locked_camera.model().await,
            locked_camera.dimensions().await.0,
            locked_camera.dimensions().await.1
        );
    }

    let shared_telescope_position =
        Arc::new(tokio::sync::Mutex::new(TelescopePosition::new()));

    // Apparently when a client cancels a gRPC request (e.g. timeout), the
    // corresponding server-side tokio task is cancelled. Per
    // https://docs.rs/tokio/latest/tokio/task/index.html#cancellation
    //   "When tasks are shut down, it will stop running at whichever .await it
    //   has yielded at. All local variables are destroyed by running their
    //   destructor."
    //
    // In our code, this can have grave consequences. Consider:
    //   <acquire resource>
    //   foobar().await;
    //   <release resource>
    // Because of task cancellation, we might never regain control after the
    // .await, and thus not release the resource.
    //
    // Because tokio guarantees that locals are destroyed (I verified this),
    // RAII should be used to guard against control never coming back from
    // .await:
    //   <acquire resource in RAII object, with release on drop>
    //   foobar().await;
    //
    // Another precaution is to spawn a separate task to run long-lived or
    // transactional operations, such that if the RPC's task gets cancelled,
    // the spawned task will detach and run to completion.
    // See: https://greptime.com/blogs/2023-01-12-hidden-control-flow
    //      https://github.com/hyperium/tonic/issues/981

    // Adapted from
    // https://github.com/tokio-rs/axum/tree/main/examples/rest-grpc-multiplex
    // https://github.com/tokio-rs/axum/blob/main/examples/static-file-server

    // Build the static content web service.
    let rest = Router::new().nest_service("/", ServeDir::new(flutter_app_path));

    let activity_led =
        Arc::new(tokio::sync::Mutex::new(ActivityLed::new(got_signal.clone())));
    if let Some(wifi) = &wifi {
        // Take the activity LED's blink pattern from WiFi mode changes: fast
        // in client mode, regular otherwise.
        let observer = Arc::new(WifiLedObserver {
            activity_led: activity_led.clone(),
        });
        let locked_wifi = wifi.read().await;
        locked_wifi.set_mode_observer(observer);

        // Apply the mode we are already in, since observers only hear about
        // later changes. A startup join may still be in flight, so count that
        // as client mode too. Not via the observer: it uses blocking_lock(),
        // which panics on an async worker.
        let joining = matches!(
            locked_wifi.client_status().map(|s| s.state),
            Some(WifiClientStateDomain::Connecting)
        );
        if joining
            || matches!(locked_wifi.mode(), WifiModeDomain::Client { .. })
        {
            activity_led
                .lock()
                .await
                .set_blink_pattern(BlinkPattern::Fast);
        }
    }

    // Use supplied solver, with Tetra3Solver as fallback.
    let solver = match injected_solver {
        Some(s) => s,
        None => Arc::new(tokio::sync::Mutex::new(
            Tetra3Solver::new(
                &args.tetra3_script,
                &args.tetra3_database,
                got_signal.clone(),
            )
            .await
            .unwrap(),
        )),
    };

    // Build the gRPC service.
    let path: PathBuf = [args.log_dir, args.log_file].iter().collect();
    let cedar = MyCedar::new(
        solver,
        effective_binning,
        args.display_sampling,
        // initial_exposure_duration=
        Duration::from_millis(100),
        args.min_exposure,
        args.max_exposure,
        activity_led.clone(),
        attached_camera,
        test_image_camera,
        camera,
        shared_telescope_position.clone(),
        args.star_count_goal,
        args.sigma,
        // TODO: arg for this?
        // stats_capacity=
        100,
        PathBuf::from(args.ui_prefs),
        path,
        product_name,
        copyright,
        feature_level,
        cedar_sky,
        wifi.clone(),
        imu_tracker,
        hot_pixel_map,
        saving_state,
    )
    .await
    .unwrap();

    // Clone pairing_mode state before cedar is moved.
    let pairing_mode_state = cedar.state.lock().await.pairing_mode.clone();

    // Initialize our device name during startup. The same name identifies us
    // over Bluetooth and on the network; it is the access point's SSID, which
    // is what the user already sees when joining our hotspot.
    {
        let state = cedar.state.lock().await;
        let ap_ssid = match state.wifi.as_ref() {
            Some(wifi) => wifi.read().await.access_point().map(|ap| ap.ssid),
            None => None,
        };
        let device_name = device_name(ap_ssid, &cedar.serial_number);
        if let Some(wifi) = state.wifi.as_ref() {
            wifi.read().await.set_host_name(&device_name);
        }
        drop(state);
        // Bound this so a host without a Bluetooth adapter / bluetoothd (e.g.
        // an x86 dev machine) doesn't hang startup before the server
        // binds its ports. Bluetooth itself remains best-effort via the
        // spawned BT tasks.
        match tokio::time::timeout(
            Duration::from_secs(5),
            set_adapter_name(&device_name),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!(
                "Failed to set Bluetooth adapter name during startup: {:?}",
                e
            ),
            Err(_) => warn!(
                "Timed out setting Bluetooth adapter name during startup \
                       (no Bluetooth adapter?); continuing"
            ),
        }
    }

    let connection_counters = cedar.connection_counters.clone();
    let cedar_server = CedarServer::new(cedar);

    let grpc = tonic::transport::Server::builder()
        .accept_http1(true) // TODO: don't need this?
        .layer(GrpcWebLayer::new())
        .layer(CorsLayer::new().allow_origin(Any).allow_methods(Any))
        .add_service(cedar_server)
        .into_service();

    // Combine static content (flutter app) server and gRPC server into one
    // service.
    let service = MultiplexService::new(rest, grpc);

    // Listen on any WiFi address for the given port.
    let addr = SocketAddr::from(([0, 0, 0, 0], 80));
    info!("Listening at {:?}", addr);
    let listener_80 = tokio::net::TcpListener::bind(&addr).await.unwrap();
    let service_future = hyper::Server::builder(accept_with_sndbuf(
        listener_80,
    ))
    .serve(ConnectionTrackingMakeService::new(
        service.clone(),
        connection_counters.clone(),
        80,
    ));

    let addr8080 = SocketAddr::from(([0, 0, 0, 0], 8080));
    let listener_8080 = tokio::net::TcpListener::bind(&addr8080).await.unwrap();
    let service_future8080 = hyper::Server::builder(accept_with_sndbuf(
        listener_8080,
    ))
    .serve(ConnectionTrackingMakeService::new(
        service.clone(),
        connection_counters.clone(),
        8080,
    ));

    // Also listen on Bluetooth. serve_over_bt returns whenever the BT stack
    // was hard-reset (bluer session invalidated) or bluetoothd went away;
    // restart it in a loop so a wedge recovery brings the BT server back
    // up automatically. Brief backoff prevents a spin if the stack is
    // persistently broken.
    let bt_counters = connection_counters.clone();
    let bt_server_handle: tokio::task::JoinHandle<Result<(), tonic::Status>> =
        tokio::task::spawn(async move {
            loop {
                {
                    // Scoped so the non-Send Box<dyn Error> return value
                    // does not span the await below.
                    let status =
                        serve_over_bt(service.clone(), bt_counters.clone())
                            .await;
                    if let Err(e) = &status {
                        warn!("BT server exited with error: {:?}", e);
                    }
                }
                warn!("Restarting BT server in 2s");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });

    // Run pairing mode loop.
    let pairing_mode_handle: tokio::task::JoinHandle<
        Result<(), tonic::Status>,
    > = tokio::task::spawn(async move {
        if let Err(e) = run_pairing_mode(pairing_mode_state).await {
            warn!("Bluetooth pairing mode loop exited with error: {:?}", e);
        }
        Ok(())
    });

    // Spin up servers for reporting our RA/Dec solution as the telescope
    // position.

    // Function called whenever telescope interrogates our position.
    let async_callback = Box::new(move || {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                note_rpc_received(&activity_led, &wifi).await;
            });
        });
    });

    // Serve LX200 protocol over Bluetooth and WiFi.
    let mut lx200_server_bt = create_lx200_server(
        shared_telescope_position.clone(),
        async_callback.clone(),
        true,
        connection_counters.clone(),
    );
    let lx200_task_bt_handle: tokio::task::JoinHandle<
        Result<(), tonic::Status>,
    > = tokio::task::spawn(async move {
        let _status = lx200_server_bt.serve_requests().await;
        Ok(())
    });
    let mut lx200_server = create_lx200_server(
        shared_telescope_position.clone(),
        async_callback.clone(),
        false,
        connection_counters.clone(),
    );
    let lx200_task_handle: tokio::task::JoinHandle<Result<(), tonic::Status>> =
        tokio::task::spawn(async move {
            let _status = lx200_server.serve_requests().await;
            Ok(())
        });

    // Serve Alpaca protocol.
    let alpaca_server =
        create_alpaca_server(shared_telescope_position, async_callback);
    let alpaca_server_future = alpaca_server.start();

    let (service_result, service_result8080, alpaca_result, _, _, _, _) = join!(
        service_future,
        service_future8080,
        alpaca_server_future,
        bt_server_handle,
        lx200_task_bt_handle,
        lx200_task_handle,
        pairing_mode_handle
    );
    service_result.unwrap();
    service_result8080.unwrap();
    alpaca_result.unwrap();
}

mod multiplex_service {
    // Adapted from
    // https://github.com/tokio-rs/axum/tree/main/examples/rest-grpc-multiplex
    use std::{
        convert::Infallible,
        task::{Context, Poll},
    };

    use axum::{
        http::{header::CONTENT_TYPE, Request},
        response::{IntoResponse, Response},
    };
    use futures::{future::BoxFuture, ready};
    use tower::Service;

    pub struct MultiplexService<A, B> {
        rest: A,
        rest_ready: bool,
        grpc: B,
        grpc_ready: bool,
    }

    impl<A, B> MultiplexService<A, B> {
        pub fn new(rest: A, grpc: B) -> Self {
            Self {
                rest,
                rest_ready: false,
                grpc,
                grpc_ready: false,
            }
        }
    }

    impl<A, B> Clone for MultiplexService<A, B>
    where
        A: Clone,
        B: Clone,
    {
        fn clone(&self) -> Self {
            Self {
                rest: self.rest.clone(),
                grpc: self.grpc.clone(),
                // the cloned services probably wont be ready
                rest_ready: false,
                grpc_ready: false,
            }
        }
    }

    impl<A, B> Service<Request<hyper::Body>> for MultiplexService<A, B>
    where
        A: Service<Request<hyper::Body>, Error = Infallible>,
        A::Response: IntoResponse,
        A::Future: Send + 'static,
        B: Service<Request<hyper::Body>>,
        B::Response: IntoResponse,
        B::Future: Send + 'static,
    {
        type Response = Response;
        type Error = B::Error;
        type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

        fn poll_ready(
            &mut self,
            cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            // drive readiness for each inner service and record which is ready
            loop {
                match (self.rest_ready, self.grpc_ready) {
                    (true, true) => {
                        return Ok(()).into();
                    }
                    (false, _) => {
                        ready!(self.rest.poll_ready(cx))
                            .map_err(|err| match err {})?;
                        self.rest_ready = true;
                    }
                    (_, false) => {
                        ready!(self.grpc.poll_ready(cx))?;
                        self.grpc_ready = true;
                    }
                }
            }
        }

        fn call(&mut self, req: Request<hyper::Body>) -> Self::Future {
            // require users to call `poll_ready` first, if they don't we're
            // allowed to panic as per the `tower::Service` contract
            assert!(
                self.grpc_ready,
                "grpc service not ready. Did you forget to call `poll_ready`?"
            );
            assert!(
                self.rest_ready,
                "rest service not ready. Did you forget to call `poll_ready`?"
            );

            // if we get a grpc request call the grpc service, otherwise call
            // the rest service when calling a service it becomes
            // not-ready so we have drive readiness again
            if is_grpc_request(&req) {
                self.grpc_ready = false;
                let future = self.grpc.call(req);
                Box::pin(async move {
                    let res = future.await?;
                    Ok(res.into_response())
                })
            } else {
                self.rest_ready = false;
                let future = self.rest.call(req);
                Box::pin(async move {
                    let res = future.await.map_err(|err| match err {})?;
                    Ok(res.into_response())
                })
            }
        }
    }

    fn is_grpc_request<B>(req: &Request<B>) -> bool {
        req.headers()
            .get(CONTENT_TYPE)
            .map(|content_type| content_type.as_bytes())
            .filter(|content_type| {
                content_type.starts_with(b"application/grpc")
            })
            .is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_location() -> LatLong {
        LatLong {
            latitude: 37.0,
            longitude: -122.0,
        }
    }

    #[test]
    fn test_select_slew_target_none_active() {
        let mut alt_az = None;
        assert!(MyCedar::select_slew_target(
            /*slew_active=*/ false,
            0.0,
            0.0,
            &mut alt_az,
            Some(&test_location()),
            &SystemTime::now(),
        )
        .is_none());
    }

    #[test]
    fn test_select_slew_target_ra_dec() {
        let mut alt_az = None;
        let target = MyCedar::select_slew_target(
            /*slew_active=*/ true,
            123.0,
            45.0,
            &mut alt_az,
            Some(&test_location()),
            &SystemTime::now(),
        )
        .unwrap();
        assert_eq!(target.coord.ra, 123.0);
        assert_eq!(target.coord.dec, 45.0);
        assert!(target.alt_az.is_none());
    }

    #[test]
    fn test_select_slew_target_alt_az() {
        let mut alt_az = Some(HorizonCoord {
            altitude: 45.0,
            azimuth: 90.0,
            epoch: None,
        });
        let target = MyCedar::select_slew_target(
            /*slew_active=*/ false,
            0.0,
            0.0,
            &mut alt_az,
            Some(&test_location()),
            &SystemTime::now(),
        )
        .unwrap();
        // The alt/az target is reported as-is, along with the equatorial
        // position it currently corresponds to.
        assert_eq!(target.alt_az.as_ref().unwrap().altitude, 45.0);
        assert_eq!(target.alt_az.as_ref().unwrap().azimuth, 90.0);
        assert!(target.coord.ra >= 0.0 && target.coord.ra < 360.0);
        // Still active for subsequent frames.
        assert!(alt_az.is_some());
    }

    #[test]
    fn test_select_slew_target_alt_az_needs_observer_location() {
        let mut alt_az = Some(HorizonCoord {
            altitude: 45.0,
            azimuth: 90.0,
            epoch: None,
        });
        assert!(MyCedar::select_slew_target(
            /*slew_active=*/ false,
            0.0,
            0.0,
            &mut alt_az,
            // An alt/az target can't be placed on the celestial sphere without
            // knowing where the observer is.
            None,
            &SystemTime::now(),
        )
        .is_none());
        // The goto is not cancelled; the location may yet become known.
        assert!(alt_az.is_some());
    }

    #[test]
    fn test_select_slew_target_telescope_goto_supersedes_alt_az() {
        let mut alt_az = Some(HorizonCoord {
            altitude: 45.0,
            azimuth: 90.0,
            epoch: None,
        });
        // SkySafari/Stellarium set slew_active directly; initiate_slew_alt_az()
        // had cleared it, so this means a new RA/Dec goto arrived.
        let target = MyCedar::select_slew_target(
            /*slew_active=*/ true,
            200.0,
            -10.0,
            &mut alt_az,
            Some(&test_location()),
            &SystemTime::now(),
        )
        .unwrap();
        assert_eq!(target.coord.ra, 200.0);
        assert_eq!(target.coord.dec, -10.0);
        assert!(target.alt_az.is_none());
        // The alt/az goto is cancelled, not merely ignored for this frame.
        assert!(alt_az.is_none());
    }

    #[test]
    fn test_select_slew_target_alt_az_tracks_earth_rotation() {
        let alt_az_coord = HorizonCoord {
            altitude: 45.0,
            azimuth: 90.0,
            epoch: None,
        };
        let now = SystemTime::now();
        let later = now + Duration::from_secs(3600);

        let mut alt_az = Some(alt_az_coord.clone());
        let first = MyCedar::select_slew_target(
            false,
            0.0,
            0.0,
            &mut alt_az,
            Some(&test_location()),
            &now,
        )
        .unwrap();
        let mut alt_az = Some(alt_az_coord);
        let second = MyCedar::select_slew_target(
            false,
            0.0,
            0.0,
            &mut alt_az,
            Some(&test_location()),
            &later,
        )
        .unwrap();

        // A fixed alt/az point's right ascension advances ~15 degrees/hour,
        // which is why the conversion is redone every frame.
        let ra_advance = (second.coord.ra - first.coord.ra).rem_euclid(360.0);
        assert!(
            (14.0..16.0).contains(&ra_advance),
            "right ascension advanced {ra_advance} degrees in an hour"
        );
    }

    #[test]
    fn test_proto_merge() {
        let mut prefs1 = Preferences {
            eyepiece_fov: Some(1.0),
            night_vision_theme: Some(true),
            ..Default::default()
        };
        let prefs2 = Preferences {
            night_vision_theme: Some(false),
            hide_app_bar: Some(true),
            ..Default::default()
        };
        let prefs2_bytes = Preferences::encode_to_vec(&prefs2);
        prefs1.merge(&*prefs2_bytes).unwrap();

        // Field present only in prefs1.
        assert_eq!(prefs1.eyepiece_fov, Some(1.0));

        // Field present on both protos, take prefs2 value.
        assert_eq!(prefs1.night_vision_theme, Some(false));

        // Field present only in prefs2.
        assert_eq!(prefs1.hide_app_bar, Some(true));
    }
} // mod tests.
