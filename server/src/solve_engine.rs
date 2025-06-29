// Copyright (c) 2024 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use crate::detect_engine::{DetectEngine, DetectResult};

use std::cmp::max;
use std::ops::DerefMut;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use canonical_error::{CanonicalError,
                      failed_precondition_error, invalid_argument_error};
use chrono::{DateTime, Local, Utc};
use image::{GenericImageView, GrayImage};
use imageproc::rect::Rect;
use log::{debug, error, warn};

use cedar_elements::solver_trait::{
    SolveExtension, SolveParams, SolverTrait};
use cedar_elements::value_stats::ValueStatsAccumulator;
use cedar_elements::cedar_common::CelestialCoord;
use cedar_elements::cedar::{
    FovCatalogEntry, ImageCoord, PlateSolution as PlateSolutionProto,
    SlewRequest, ValueStats};
use cedar_elements::cedar_sky_trait::CedarSkyTrait;
use cedar_elements::cedar_sky::{CatalogEntry, CatalogEntryMatch, Ordering};
use cedar_detect::histogram_funcs::{average_top_values,
                                    get_level_for_fraction,
                                    remove_stars_from_histogram};
use cedar_elements::image_utils::{normalize_rows_mut, scale_image_mut};
use cedar_elements::astro_util::{
    angular_separation, position_angle, transform_to_image_coord};

pub struct SolveEngine {
    // The plate solver we are using.
    solver: Arc<tokio::sync::Mutex<dyn SolverTrait + Send + Sync>>,

    // Our state, shared between SolveEngine methods and the worker thread.
    state: Arc<tokio::sync::Mutex<SolveState>>,

    // Detect engine settings can be adjusted behind our back.
    detect_engine: Arc<tokio::sync::Mutex<DetectEngine>>,

    // Executes worker().
    worker_thread: Option<std::thread::JoinHandle<()>>,

    // Called whenever worker() finishes an evaluation when not in align mode.
    // Return value:
    // (sky coordinate of slew target (if any),
    //  sky coordinate of sync operation (if any))
    solution_callback: Arc<dyn Fn(Option<ImageCoord>,
                                  Option<DetectResult>,
                                  Option<PlateSolutionProto>)
                                  -> (Option<CelestialCoord>,
                                      Option<CelestialCoord>)
                           + Send + Sync>,
}

// State shared between worker thread and the SolveEngine methods.
struct SolveState {
    // In align mode, the `catalog_entry_match` is ignored and instead we
    // retrieve bright planets and IAU stars for the solved FOV.
    align_mode: bool,

    // Determines whether rows are normalized to have the same dark level.
    normalize_rows: bool,

    cedar_sky: Option<Arc<Mutex<dyn CedarSkyTrait + Send>>>,
    catalog_entry_match: Option<CatalogEntryMatch>,

    frame_id: Option<i32>,

    // Required number of detected stars, below which we don't attempt a plate
    // solution.
    minimum_stars: i32,

    // Parameters for plate solver. See documentation of Tetra3's
    // solve_from_centroids() function for a description of these items.
    fov_estimate: Option<f64>,
    match_radius: f64,
    match_threshold: f64,
    solve_timeout: Duration,
    boresight_pixel: Option<ImageCoord>,
    distortion: f64,
    match_max_error: f64,
    return_matches: bool,

    // Set if currently slewing to a target.
    slew_target: Option<CelestialCoord>,

    solve_latency_stats: ValueStatsAccumulator,
    solve_attempt_stats: ValueStatsAccumulator,
    solve_success_stats: ValueStatsAccumulator,

    // Estimated time at which `plate_solution` will next be updated.
    eta: Option<Instant>,

    plate_solution: Option<PlateSolution>,
    logged_error: bool,
}

impl SolveEngine {
    pub async fn new(
        normalize_rows: bool,
        solver: Arc<tokio::sync::Mutex<dyn SolverTrait + Send + Sync>>,
        cedar_sky: Option<Arc<Mutex<dyn CedarSkyTrait + Send>>>,
        detect_engine: Arc<tokio::sync::Mutex<DetectEngine>>,
        stats_capacity: usize,
        solution_callback: Arc<dyn Fn(Option<ImageCoord>,
                                      Option<DetectResult>,
                                      Option<PlateSolutionProto>)
                                      -> (Option<CelestialCoord>,
                                          Option<CelestialCoord>)
                               + Send + Sync>)
        -> Result<Self, CanonicalError>
    {
        Ok(SolveEngine{
            solver: solver.clone(),
            state: Arc::new(tokio::sync::Mutex::new(SolveState{
                align_mode: false,
                normalize_rows,
                cedar_sky,
                catalog_entry_match: None,
                frame_id: None,
                minimum_stars: 4,
                fov_estimate: None,
                match_radius: 0.01,
                match_threshold: 1e-5,  // TODO: pass in from cmdline arg.
                solve_timeout: Duration::from_secs(1),
                boresight_pixel: None,
                distortion: 0.0,
                match_max_error: 0.005,
                return_matches: true,
                slew_target: None,
                solve_latency_stats: ValueStatsAccumulator::new(stats_capacity),
                solve_attempt_stats: ValueStatsAccumulator::new(stats_capacity),
                solve_success_stats: ValueStatsAccumulator::new(stats_capacity),
                eta: None,
                plate_solution: None,
                logged_error: false,
            })),
            detect_engine,
            worker_thread: None,
            solution_callback,
        })
    }

    pub async fn set_align_mode(&mut self, align_mode: bool) {
        let mut locked_state = self.state.lock().await;
        locked_state.align_mode = align_mode;
    }

    // Sets the parameters used to retrieve sky catalog entries for the solved
    // FOV.
    pub async fn set_catalog_entry_match(
        &mut self, catalog_entry_match: Option<CatalogEntryMatch>) {
        let mut locked_state = self.state.lock().await;
        locked_state.catalog_entry_match = catalog_entry_match;
    }

    pub async fn set_fov_estimate(&mut self, fov_estimate: Option<f64>)
                                  -> Result<(), CanonicalError> {
        let mut locked_state = self.state.lock().await;
        if fov_estimate.is_some() && fov_estimate.unwrap() <= 0.0 {
            return Err(invalid_argument_error(
                format!("fov_estimate must be positive; got {}",
                        fov_estimate.unwrap()).as_str()));
        }
        locked_state.fov_estimate = fov_estimate;
        // Don't need to do anything, worker thread will pick up the change when
        // it finishes the current interval.
        Ok(())
    }

    pub async fn set_boresight_pixel(
        &mut self, boresight_pixel: Option<ImageCoord>)
        -> Result<(), CanonicalError>
    {
        let mut locked_state = self.state.lock().await;
        locked_state.boresight_pixel = boresight_pixel;
        // Don't need to do anything, worker thread will pick up the change when
        // it finishes the current interval.
        Ok(())
    }
    pub async fn boresight_pixel(&self) -> Option<ImageCoord> {
        let locked_state = self.state.lock().await;
        locked_state.boresight_pixel.clone()
    }

    pub async fn set_distortion(&mut self, distortion: f64)
                                -> Result<(), CanonicalError> {
        if distortion < -0.2 || distortion > 0.2 {
            return Err(invalid_argument_error(
                format!("distortion must be in [-0.2, 0.2]; got {}",
                        distortion).as_str()));
        }
        let mut locked_state = self.state.lock().await;
        locked_state.distortion = distortion;
        // Don't need to do anything, worker thread will pick up the change when
        // it finishes the current interval.
        Ok(())
    }

    pub async fn set_match_max_error(&mut self, match_max_error: f64)
                                     -> Result<(), CanonicalError> {
        if match_max_error < 0.0 {
            return Err(invalid_argument_error(
                format!("match_max_error must be non-negative; got {}",
                        match_max_error).as_str()));
        }
        let mut locked_state = self.state.lock().await;
        locked_state.match_max_error = match_max_error;
        // Don't need to do anything, worker thread will pick up the change when
        // it finishes the current interval.
        Ok(())
    }

    pub async fn set_minimum_stars(&mut self, minimum_stars: i32)
                                   -> Result<(), CanonicalError> {
        if minimum_stars < 4 {
            return Err(invalid_argument_error(
                format!("minimum_stars must be at least 4; got {}",
                        minimum_stars).as_str()));
        }
        let mut locked_state = self.state.lock().await;
        locked_state.minimum_stars = minimum_stars;
        // Don't need to do anything, worker thread will pick up the change when
        // it finishes the current interval.
        Ok(())
    }

    pub async fn set_solve_timeout(&mut self, solve_timeout: Duration)
                                   -> Result<(), CanonicalError> {
        let mut locked_state = self.state.lock().await;
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
    /// Returns: the processed result along with its frame_id value. Returns
    ///     None if non_blocking and a suitable result is not yet available.
    pub async fn get_next_result(
        &mut self, prev_frame_id: Option<i32>,
        non_blocking: bool) -> Option<PlateSolution>
    {
        // Start worker thread if terminated or not yet started.
        self.start().await;

        // Get the most recently posted result; wait if there is none yet or the
        // currently posted result is the same as the one the caller has already
        // obtained.
        loop {
            let mut sleep_duration = Duration::from_millis(1);
            {
                let locked_state = self.state.lock().await;
                if locked_state.plate_solution.is_some() &&
                    (prev_frame_id.is_none() ||
                     prev_frame_id.unwrap() !=
                     locked_state.plate_solution.as_ref().unwrap().detect_result.frame_id)
                {
                    // Don't consume it, other clients may want it.
                    return Some(locked_state.plate_solution.clone().unwrap());
                }
                if non_blocking {
                    return None;
                }
                if let Some(eta) = locked_state.eta {
                    let time_to_eta = eta.saturating_duration_since(Instant::now());
                    if time_to_eta > sleep_duration {
                        sleep_duration = time_to_eta;
                    }
                }
            }
            tokio::time::sleep(sleep_duration).await;
        }
    }

    pub async fn reset_session_stats(&mut self) {
        let mut locked_state = self.state.lock().await;
        locked_state.solve_latency_stats.reset_session();
        locked_state.solve_attempt_stats.reset_session();
        locked_state.solve_success_stats.reset_session();
    }

    // TODO: arg specifying directory to save to.
    pub async fn save_image(&self) -> Result<(), CanonicalError> {
        // Grab most recent image.
        let captured_image = &self.detect_engine.lock().await.get_next_result(
            /*frame_id=*/None, /*non_blocking=*/false).await.unwrap().captured_image;
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
                               exposure_duration_ms,
                               datetime_local.format("%Y%m%d_%H%M%S"));
        // Write to current directory.
        match image.save(filename) {
            Ok(()) => Ok(()),
            Err(x) => {
                Err(failed_precondition_error(
                    format!("Error saving file: {:?}", x).as_str()))
            }
        }
    }

    async fn solve_with_solver(
        solver: Arc<tokio::sync::Mutex<dyn SolverTrait>>,
        star_centroids: &[ImageCoord],
        width: usize, height: usize,
        extension: &SolveExtension,
        params: &SolveParams) -> Result<PlateSolutionProto, CanonicalError>
    {
        solver.lock().await.solve_from_centroids(
            star_centroids, width, height, extension, params).await
    }

    pub async fn start(&mut self) {
        // Has the worker terminated for some reason?
        if self.worker_thread.is_some() &&
            self.worker_thread.as_ref().unwrap().is_finished()
        {
            self.worker_thread.take().unwrap();
        }
        if self.worker_thread.is_none() {
            let cloned_solver = self.solver.clone();
            let cloned_state = self.state.clone();
            let cloned_detect_engine = self.detect_engine.clone();
            let cloned_callback = self.solution_callback.clone();
            // Allocate a thread for concurrent execution of solver with
            // other activities.
            self.worker_thread = Some(std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .thread_name("solve_engine")
                    .build().unwrap();
                runtime.block_on(async move {
                    SolveEngine::worker(cloned_solver, cloned_state,
                                        cloned_detect_engine, cloned_callback).await;
                });
            }));
        }
    }

    async fn worker(
        solver: Arc<tokio::sync::Mutex<dyn SolverTrait + Send + Sync>>,
        state: Arc<tokio::sync::Mutex<SolveState>>,
        detect_engine: Arc<tokio::sync::Mutex<DetectEngine>>,
        solution_callback: Arc<dyn Fn(Option<ImageCoord>,
                                      Option<DetectResult>,
                                      Option<PlateSolutionProto>)
                                      -> (Option<CelestialCoord>,
                                          Option<CelestialCoord>)
                               + Send + Sync>) {
        debug!("Starting solve engine");
        loop {
            let minimum_stars;
            let frame_id;
            let normalize_rows;
            let mut solve_extension = SolveExtension::default();
            let mut solve_params = SolveParams::default();
            {
                let mut locked_state = state.lock().await;
                locked_state.eta = None;

                minimum_stars = locked_state.minimum_stars;
                frame_id = locked_state.frame_id;
                normalize_rows = locked_state.normalize_rows;

                // Set up solve arguments.
                if let Some(fov) = locked_state.fov_estimate {
                    solve_params.fov_estimate = Some((fov, fov / 10.0));
                }
                solve_params.match_radius = Some(locked_state.match_radius);
                solve_params.match_threshold = Some(locked_state.match_threshold);
                solve_params.solve_timeout = Some(locked_state.solve_timeout);
                if let Some(boresight_pixel) = &locked_state.boresight_pixel {
                    solve_extension.target_pixel = Some(Vec::<ImageCoord>::new());
                    solve_extension.target_pixel
                        .as_mut().unwrap().push(boresight_pixel.clone());
                }
                if let Some(slew_target) = &locked_state.slew_target {
                    solve_extension.target_sky_coord =
                        Some(Vec::<CelestialCoord>::new());
                    solve_extension.target_sky_coord
                        .as_mut().unwrap().push(slew_target.clone());
                }
                solve_params.distortion = Some(locked_state.distortion);
                solve_params.match_max_error = Some(locked_state.match_max_error);
                solve_extension.return_matches = locked_state.return_matches;
                solve_extension.return_catalog = true;
                solve_extension.return_rotation_matrix = true;
            }
            // Get the most recent star detection result.
            if let Some(delay_est) =
                detect_engine.lock().await.estimate_delay(frame_id)
            {
                state.lock().await.eta = Some(Instant::now() + delay_est);
            }

            // Don't hold detect engine lock for the entirety of the time waiting for
            // the next result.
            let detect_result;
            loop {
                let dr = detect_engine.lock().await.get_next_result(
                    frame_id, /*non_blocking=*/true).await;
                if dr.is_none() {
                    let short_delay = Duration::from_millis(10);
                    let delay_est =
                        detect_engine.lock().await.estimate_delay(frame_id);
                    if let Some(delay_est) = delay_est {
                        tokio::time::sleep(max(delay_est, short_delay)).await;
                    } else {
                        tokio::time::sleep(short_delay).await;
                    }
                    continue;
                }
                detect_result = dr.unwrap();
                break;
            }
            state.lock().await.deref_mut().frame_id = Some(detect_result.frame_id);

            let image: &GrayImage = &detect_result.captured_image.image;
            let (width, height) = image.dimensions();

            // Plate-solve using the recently detected stars.
            let process_start_time = Instant::now();

            let mut star_centroids = Vec::<ImageCoord>::with_capacity(
                detect_result.star_candidates.len());
            for sc in &detect_result.star_candidates {
                star_centroids.push(ImageCoord{x: sc.centroid_x,
                                               y: sc.centroid_y});
            }

            let mut plate_solution_proto: Option<PlateSolutionProto> = None;
            let mut solve_finish_time: Option<SystemTime> = None;
            if detect_result.star_candidates.len() >= minimum_stars as usize {
                {
                    let mut locked_state = state.lock().await;
                    if let Some(recent_stats) =
                        &locked_state.solve_latency_stats.value_stats.recent
                    {
                        let solve_duration = Duration::from_secs_f64(recent_stats.min);
                        locked_state.eta = Some(Instant::now() + solve_duration);
                    }
                }
                match Self::solve_with_solver(
                    solver.clone(),
                    &star_centroids,
                    width as usize, height as usize,
                    &solve_extension, &solve_params).await
                {
                    Err(e) => {
                        // Let's not spam the log with solver failures. If the
                        // number of detected stars is low, don't bother to log,
                        // as this is a trivial source of solve failures (e.g.
                        // due to telescope motion).
                        // Empirically, solutions are possible at 6 centroids,
                        // and are ~reliable at 10 or more centroids. For logging,
                        // we split the difference.
                        if star_centroids.len() >= 8 {
                            let mut locked_state = state.lock().await;
                            // Secondly, don't log the error if we've just logged one.
                            if !locked_state.logged_error {
                                error!("Solver error {:?} with {} centroids",
                                       e, star_centroids.len());
                                locked_state.logged_error = true;
                            }
                        }
                    },
                    Ok(solution) => {
                        // Re-enable logging of the next non-trivial solve failure.
                        state.lock().await.logged_error = false;
                        plate_solution_proto = Some(solution);
                    }
                }
                solve_finish_time = Some(SystemTime::now());
            }

            let elapsed = process_start_time.elapsed();
            let mut fov_catalog_entries: Option<Vec<FovCatalogEntry>> = None;
            let mut decrowded_fov_catalog_entries: Option<Vec<FovCatalogEntry>> = None;
            let mut slew_request = None;
            let mut boresight_image: Option<GrayImage> = None;
            let mut boresight_image_region: Option<Rect> = None;
            let align_mode;
            let boresight_pixel;
            let cedar_sky;
            {
                let locked_state = state.lock().await;
                align_mode = locked_state.align_mode;
                boresight_pixel = locked_state.boresight_pixel.clone();
                cedar_sky = locked_state.cedar_sky.clone();
            }
            if plate_solution_proto.is_none() {
                if !align_mode {
                    state.lock().await.solve_attempt_stats.add_value(0.0);
                    solution_callback(boresight_pixel,
                                      Some(detect_result.clone()), None);
                }
            } else {
                if !align_mode {
                    state.lock().await.solve_attempt_stats.add_value(1.0);
                }
                let psp = plate_solution_proto.as_ref().unwrap();
                if !align_mode {
                    state.lock().await.solve_success_stats.add_value(1.0);
                }
                let boresight_coords = if psp.target_sky_coord.is_empty() {
                    psp.image_sky_coord.as_ref().unwrap().clone()
                } else {
                    psp.target_sky_coord[0].clone()
                };
                let mut rotation_matrix: [f64; 9] = [0.0; 9];
                for (idx, c) in psp.rotation_matrix.clone().into_iter().enumerate() {
                    rotation_matrix[idx] = c;
                }

                if !align_mode {
                    // Let integration layer pass solution to SkySafari telescope
                    // interface and MotionEstimator. Integration layer returns current
                    // slew target coords, if any.
                    let (slew_target, sync_coord) =
                        solution_callback(boresight_pixel.clone(),
                                          Some(detect_result.clone()), Some(psp.clone()));
                    state.lock().await.slew_target = slew_target;
                    // If we're slewing, see if the boresight is close enough to
                    // the slew target that Cedar Aim should display an inset image
                    // of the region around the boresight.
                    if let Some(target_coords) = &state.lock().await.slew_target {
                        (slew_request, boresight_image_region, boresight_image) =
                            Self::handle_slew(
                                &cedar_sky,
                                target_coords, image, &boresight_coords,
                                &boresight_pixel, psp,
                                normalize_rows, width, height).await;
                    }
                    if let Some(sync_coord) = sync_coord {
                        // SkySafari user has invoked "Sync" operation on some
                        // sky object, indicating that this object is centered at
                        // the telescope boresight. We update the boresight pixel
                        // accordingly.
                        // First, translate `sync_coord` to image coordinates.
                        let xy = transform_to_image_coord(
                            &[sync_coord.ra, sync_coord.dec],
                            width as usize, height as usize,
                            psp.fov,
                            &rotation_matrix,
                            psp.distortion.unwrap());
                        let img_coord = ImageCoord{x: xy[0], y: xy[1]};
                        state.lock().await.boresight_pixel = Some(img_coord);
                        // Note: we should update the boresight in the saved
                        // preferences, but we don't have access to the
                        // cedar_server logic here. Instead, we leave it to
                        // the cedar_server logic to notice the boresight
                        // change and update the saved prefs.
                    }
                }  // !align_mode

                if cedar_sky.is_some() {
                    let mut catalog_entry_match =
                        state.lock().await.catalog_entry_match.clone().unwrap();
                    catalog_entry_match.match_catalog_label = false;
                    catalog_entry_match.match_object_type_label = false;
                    if align_mode {
                        // Replace catalog_entry_match.
                        catalog_entry_match = CatalogEntryMatch {
                            faintest_magnitude: Some(4),
                            match_catalog_label: false,
                            catalog_label: vec![],
                            match_object_type_label: true,
                            object_type_label: vec!["star".to_string(),
                                                    "double star".to_string(),
                                                    "nova star".to_string(),
                                                    "planet".to_string()],
                        };
                    }
                    let result = Self::query_fov_catalog_entries(
                        &boresight_coords,
                        &boresight_pixel,
                        cedar_sky.as_ref().unwrap(),
                        &catalog_entry_match,
                        width, height,
                        psp.fov,
                        psp.distortion.unwrap(),
                        &rotation_matrix).await;
                    (fov_catalog_entries, decrowded_fov_catalog_entries) =
                        (Some(result.0), Some(result.1));
                }
            }
            if !align_mode {
                state.lock().await.solve_latency_stats.add_value(elapsed.as_secs_f64());
            }
            // Post the result.
            let solve_latency_stats;
            let solve_attempt_stats;
            let solve_success_stats;
            {
                let locked_state = state.lock().await;
                solve_latency_stats =
                    locked_state.solve_latency_stats.value_stats.clone();
                solve_attempt_stats =
                    locked_state.solve_attempt_stats.value_stats.clone();
                solve_success_stats =
                    locked_state.solve_success_stats.value_stats.clone();
            }
            state.lock().await.plate_solution = Some(PlateSolution{
                detect_result,
                plate_solution: plate_solution_proto,
                fov_catalog_entries,
                decrowded_fov_catalog_entries,
                slew_request,
                boresight_image,
                boresight_image_region,
                solve_finish_time,
                processing_duration: elapsed,
                solve_latency_stats,
                solve_attempt_stats,
                solve_success_stats,
            });
        }  // loop.
    }  // worker

    // Given a target, finds the closest catalog entry within 1 arcmin. Returns
    // the closest catalog entry, if any, and the distance in degrees between
    // the catalog entry and the target.
    async fn get_catalog_entry_for_target(
        cedar_sky: &Arc<Mutex<dyn CedarSkyTrait + Send>>,
        target_coords: &CelestialCoord)
        -> (Option<CatalogEntry>, Option<f64>) {
        let query_result = cedar_sky.lock().unwrap().query_catalog_entries(
            /*max_distance=*/Some(1.0 / 60.0),  // 1 arcmin.
            /*min_elevation=*/None,
            /*faintest_magnitude=*/None,
            /*match_catalog_label=*/false,
            /*catalog_label=*/&[],
            /*match_object_type_label=*/false,
            /*object_type_label=*/&[],
            /*text_search*/None,
            /*ordering=*/Some(Ordering::SkyLocation),
            /*decrowd_distance=*/None,
            /*limit_result*/None,
            /*sky_location*/Some(target_coords.clone()),
            /*location_info=*/None);
        if let Err(e) = query_result {
            warn!("Error querying sky catalog: {:?}", e);
            return (None, None);
        }
        let (selected_catalog_entries, _overflow) = query_result.unwrap();
        if selected_catalog_entries.is_empty() {
            return (None, None);
        }
        let closest_entry = selected_catalog_entries[0].entry.clone().unwrap();
        let (target_ra, target_dec) = (target_coords.ra.to_radians(),
                                       target_coords.dec.to_radians());
        let entry_coord = closest_entry.coord.as_ref().unwrap();
        let (entry_ra, entry_dec) = (entry_coord.ra.to_radians(),
                                     entry_coord.dec.to_radians());
        let distance = angular_separation(target_ra, target_dec,
                                          entry_ra, entry_dec).to_degrees();
        (Some(closest_entry), Some(distance))
    }

    async fn handle_slew(
        cedar_sky: &Option<Arc<Mutex<dyn CedarSkyTrait + Send>>>,
        target_coords: &CelestialCoord,
        image: &GrayImage,
        boresight_coords: &CelestialCoord,
        boresight_pixel: &Option<ImageCoord>,
        plate_solution: &PlateSolutionProto,
        normalize_rows: bool,
        width: u32, height: u32)
        -> (Option<SlewRequest>, Option<Rect>, Option<GrayImage>)
    {
        let mut slew_request = SlewRequest{
            target: Some(target_coords.clone()), ..Default::default()};
        let bs_ra = boresight_coords.ra.to_radians();
        let bs_dec = boresight_coords.dec.to_radians();
        let st_ra = target_coords.ra.to_radians();
        let st_dec = target_coords.dec.to_radians();
        slew_request.target_distance = Some(angular_separation(
            bs_ra, bs_dec, st_ra, st_dec).to_degrees());

        let mut angle = (position_angle(
            bs_ra, bs_dec, st_ra, st_dec).to_degrees() +
                         plate_solution.roll) % 360.0;
        // Arrange for angle to be 0..360.
        if angle < 0.0 {
            angle += 360.0;
        }
        slew_request.target_angle = Some(angle);

        if let Some(cedar_sky) = cedar_sky {
            // See if Cedar-sky has a catalog object corresponding to the slew
            // target's RA/Dec.
            let (catalog_entry, distance) = Self::get_catalog_entry_for_target(
                cedar_sky, target_coords).await;
            slew_request.target_catalog_entry = catalog_entry;
            slew_request.target_catalog_entry_distance = distance;
        }

        if plate_solution.target_pixel.is_empty() {
            return (Some(slew_request), None, None);
        }
        let img_coord = &plate_solution.target_pixel[0];
        if img_coord.x < 0.0 {
            return (Some(slew_request), None, None);
        }

        let target_image_coord = ImageCoord{x: img_coord.x, y: img_coord.y};
        slew_request.image_pos = Some(target_image_coord.clone());
        // Is the target's image_pos close to the boresight's image position?
        let boresight_pos;
        if let Some(bp) = boresight_pixel {
            boresight_pos = bp.clone();
        } else {
            boresight_pos = ImageCoord{
                x: width as f64 / 2.0, y: height as f64 / 2.0};
        }
        let target_close_threshold =
            std::cmp::min(width, height) as f64 / 16.0;
	let target_boresight_distance =
            ((target_image_coord.x - boresight_pos.x) *
             (target_image_coord.x - boresight_pos.x) +
             (target_image_coord.y - boresight_pos.y) *
             (target_image_coord.y - boresight_pos.y)).sqrt();
        if target_boresight_distance >= target_close_threshold {
            return (Some(slew_request), None, None);
        }

        let image_rect = Rect::at(0, 0).of_size(width, height);
        // Get a sub-image centered on the boresight.
        let bs_image_size = std::cmp::min(width, height) / 6;
        let mut boresight_image_region = Some(Rect::at(
            boresight_pos.x as i32 - bs_image_size as i32/2,
            boresight_pos.y as i32 - bs_image_size as i32/2)
                                              .of_size(bs_image_size, bs_image_size));
        boresight_image_region =
            Some(boresight_image_region.
                 unwrap().intersect(image_rect).unwrap());
        // We scale up the pixel values in the sub_image for good display
        // visibility.
        let mut boresight_image = Some(
            image.view(boresight_image_region.unwrap().left() as u32,
                       boresight_image_region.unwrap().top() as u32,
                       boresight_image_region.unwrap().width(),
                       boresight_image_region.unwrap().height()).to_image());
        if normalize_rows {
            normalize_rows_mut(boresight_image.as_mut().unwrap());
        }
        let mut histogram: [u32; 256] = [0_u32; 256];
        for pixel_value in boresight_image.as_ref().unwrap().pixels() {
            histogram[pixel_value.0[0] as usize] += 1;
        }
        // Compute peak_value as the average of the 5 brightest pixels.
        let peak_pixel_value = max(average_top_values(&histogram, 5), 64);
        remove_stars_from_histogram(&mut histogram, /*sigma=*/8.0);
        let min_pixel_value = get_level_for_fraction(&histogram, 0.9);
        scale_image_mut(boresight_image.as_mut().unwrap(),
                        min_pixel_value as u8,
                        peak_pixel_value,
                        /*gamma=*/0.7);

        (Some(slew_request), boresight_image_region, boresight_image)
    }  // handle_slew

    fn make_fov_catalog_entry(entry: &CatalogEntry, width: usize, height: usize,
                              fov: f64, distortion: f64,
                              rotation_matrix: &[f64; 9])
                              -> Option<FovCatalogEntry>
    {
        let coord = entry.coord.clone().unwrap();
        let img_coord = transform_to_image_coord(
            &[coord.ra, coord.dec],
            width, height,
            fov, rotation_matrix, distortion);
        let x = img_coord[0];
        if x < 0.0 || x >= width as f64 {
            return None;
        }
        let y = img_coord[1];
        if y < 0.0 || y >= height as f64 {
            return None;
        }
        Some(FovCatalogEntry {
            entry: Some(entry.clone()),
            image_pos: Some(ImageCoord { x, y }),
        })
    }

    // Returns two lists of FovCatalogEntry. The first one is the entries that
    // survived decrowding (they are brighter than very nearby entries); the
    // second one is the decrowded entries (close to an entry in the first
    // collection but fainter).
    async fn query_fov_catalog_entries(
        boresight_coords: &CelestialCoord,
        boresight_pixel: &Option<ImageCoord>,
        cedar_sky: &Arc<Mutex<dyn CedarSkyTrait + Send>>,
        catalog_entry_match: &CatalogEntryMatch,
        width: u32, height: u32,
        fov: f64, distortion: f64,
        rotation_matrix: &[f64; 9])
        -> (Vec<FovCatalogEntry>, Vec<FovCatalogEntry>) {
        let mut answer = Vec::<FovCatalogEntry>::new();  // Decrowd survivors.
        let mut culled = Vec::<FovCatalogEntry>::new();  // Decrowd victims.

        let bp = if boresight_pixel.is_some() {
            boresight_pixel.clone().unwrap()
        } else {
            ImageCoord{x: width as f64 / 2.0, y: height as f64 / 2.0}
        };

        // Figure out radius from boresight for the catalog entry search.
        let deg_per_pixel = fov / width as f64;
        let h = f64::max(bp.x, width as f64 - bp.x);
        let v = f64::max(bp.y, height as f64 - bp.y);
        let radius_deg = (h * h + v * v).sqrt() * deg_per_pixel;

        let query_result = cedar_sky.lock().unwrap().query_catalog_entries(
            /*max_distance=*/Some(radius_deg),
            /*min_elevation=*/None,
            catalog_entry_match.faintest_magnitude,
            catalog_entry_match.match_catalog_label,
            &catalog_entry_match.catalog_label,
            catalog_entry_match.match_object_type_label,
            &catalog_entry_match.object_type_label,
            /*text_search*/None,
            /*ordering=*/None,
            // TODO: parameter for decrowd factor.
            /*decrowd_distance=*/Some(3600.0 * fov / 15.0),  // Arcsec.
            /*limit_result*/Some(50),
            /*sky_location*/Some(boresight_coords.clone()),
            /*location_info=*/None);
        if let Err(e) = query_result {
            warn!("Error querying sky catalog: {:?}", e);
            return (answer, culled);
        }

        let selected_catalog_entries = query_result.unwrap().0;
        for sce in selected_catalog_entries {
            let entry = sce.entry.unwrap();
            // Convert each catalog entry's celesital coordinates to image
            // position, and discard those outside of our FOV.
            if let Some(fce) = Self::make_fov_catalog_entry(
                &entry, width as usize, height as usize, fov,
                distortion, rotation_matrix)
            {
                answer.push(fce);
            }
            for decrowded in sce.decrowded_entries {
                if let Some(fce) = Self::make_fov_catalog_entry(
                    &decrowded, width as usize, height as usize, fov,
                    distortion, rotation_matrix)
                {
                    culled.push(fce);
                }
            }
        }
        (answer, culled)
    }  // query_fov_catalog_entries
}  // impl SolveEngine

#[derive(Clone)]
pub struct PlateSolution {
    // The detect result used to produce the information in this solve result.
    pub detect_result: DetectResult,

    // The plate solution for `detect_result`. Omitted if a solve was not
    // attempted or it failed.
    pub plate_solution: Option<PlateSolutionProto>,

    // These are the catalog entries, if any, that are in the `detect_result`
    // image's FOV. Order is unspecified.
    // None if there is no Cedar Sky implementation.
    pub fov_catalog_entries: Option<Vec<FovCatalogEntry>>,
    pub decrowded_fov_catalog_entries: Option<Vec<FovCatalogEntry>>,

    // If the TelescopePosition has an active slew request, we populate
    // `slew_request` with its information.
    pub slew_request: Option<SlewRequest>,

    // A small crop of the full resolution `detect_result.captured_image`
    // centered at the boresight. Brightness scaled to full range for
    // visibility. This is present if `slew_request` is present and the slew
    // target is close to the boresight.
    pub boresight_image: Option<GrayImage>,

    // The location of `boresight_image`. Omitted if `boresight_image` is
    // omitted.
    pub boresight_image_region: Option<Rect>,

    // Time at which the plate solve completed. Omitted if a solve was not
    // attempted.
    pub solve_finish_time: Option<SystemTime>,

    // Time taken to produce this PlateSolution, excluding the time taken to
    // detect stars.
    pub processing_duration: std::time::Duration,

    // Distribution of `processing_duration` values.
    pub solve_latency_stats: ValueStats,

    // Fraction of cycles in which a plate solve was attempted.
    pub solve_attempt_stats: ValueStats,

    // Fraction of attempted plate solves succeeded.
    pub solve_success_stats: ValueStats,
}
