// Copyright (c) 2026 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use std::{
    cmp::max,
    sync::Arc,
    time::{Duration, Instant},
};

use cedar_detect::image_funcs::bin_and_histogram_2x2;
use cedar_elements::{
    astro_util::{
        alt_az_from_equatorial, equatorial_from_alt_az,
        fill_in_detections, magnitude_intensity_ratio, position_angle,
    },
    cedar::{
        CalibrationData, FixedSettings, FovCatalogEntry, FrameResult, Image,
        ImageCoord, LocationBasedInfo, MountType, OperatingMode,
        OperationSettings, Preferences, ProcessingStats, Rectangle,
        StarCentroid,
    },
    image_utils::{scale_image, ImageRotator},
    imu_trait::ImuTrait,
    value_stats::ValueStatsAccumulator,
};
use image::GrayImage;
use log::debug;
use prost_types;

use crate::detect_engine::{DetectEngine, DetectResult};
use crate::polar_analyzer::PolarAnalyzer;
use crate::solve_engine::{PlateSolution, SolveEngine};

// Server state snapshot provided to the serve engine. Updated by cedar_server
// via update_context() when relevant settings change.
pub struct ServeContext {
    pub fixed_settings: FixedSettings,
    pub preferences: Preferences,
    pub operation_settings: OperationSettings,
    pub calibration_data: Arc<tokio::sync::Mutex<CalibrationData>>,
    pub imu_tracker: Option<Arc<tokio::sync::Mutex<dyn ImuTrait + Send>>>,
    pub polar_analyzer: Arc<tokio::sync::Mutex<PolarAnalyzer>>,
    pub normalize_rows: bool,
    pub binning: u32,
    pub display_sampling: bool,
    pub is_color: bool,
    pub jpeg_quality: u8,
    pub landscape: bool,
}

// State shared between the worker thread and ServeEngine methods.
struct ServeState {
    frame_id: Option<i32>,

    // Most recently produced result.
    serve_result: Option<ServeResult>,

    // Estimated time at which serve_result will next be updated.
    eta: Option<Instant>,

    // Configuration, updated by cedar_server.
    context: ServeContext,

    // Latency stats.
    serve_latency_stats: ValueStatsAccumulator,

    // Persistent across frames: image rotator carries over on plate solve
    // dropout.
    image_rotator: ImageRotator,

    // For focus assist: center peak position in rotated image coords.
    center_peak_position: Arc<tokio::sync::Mutex<Option<ImageCoord>>>,
}

pub struct ServeEngine {
    state: Arc<tokio::sync::Mutex<ServeState>>,
    solve_engine: Arc<tokio::sync::Mutex<SolveEngine>>,
    detect_engine: Arc<tokio::sync::Mutex<DetectEngine>>,
    worker_thread: Option<std::thread::JoinHandle<()>>,
}

impl ServeEngine {
    pub fn new(
        solve_engine: Arc<tokio::sync::Mutex<SolveEngine>>,
        detect_engine: Arc<tokio::sync::Mutex<DetectEngine>>,
        context: ServeContext,
        stats_capacity: usize,
    ) -> Self {
        ServeEngine {
            state: Arc::new(tokio::sync::Mutex::new(ServeState {
                frame_id: None,
                serve_result: None,
                eta: None,
                context,
                serve_latency_stats: ValueStatsAccumulator::new(stats_capacity),
                image_rotator: ImageRotator::new(0.0),
                center_peak_position: Arc::new(tokio::sync::Mutex::new(None)),
            })),
            solve_engine,
            detect_engine,
            worker_thread: None,
        }
    }

    /// Updates the server state snapshot used by the serve engine worker.
    /// Called by cedar_server when relevant settings change.
    pub async fn update_context(&mut self, context: ServeContext) {
        self.state.lock().await.context = context;
    }

    /// Returns the center peak position in rotated image coordinates, for use
    /// by other handlers (e.g. focus assist).
    pub async fn center_peak_position(&self) -> Option<ImageCoord> {
        self.state
            .lock()
            .await
            .center_peak_position
            .lock()
            .await
            .clone()
    }

    /// Returns the most recent ImageRotator, for use by other handlers that
    /// need to transform coordinates (e.g. initiate_action, detect_frame_region).
    pub async fn image_rotator(&self) -> ImageRotator {
        self.state.lock().await.image_rotator.clone()
    }

    /// Returns the most recent serve result, waiting for a new one if the
    /// caller's prev_frame_id matches the current result's frame_id.
    ///
    /// Does not consume the result; multiple callers receive the same result.
    /// Returns None if non_blocking and no suitable result is available yet.
    pub async fn get_next_result(
        &mut self,
        prev_frame_id: Option<i32>,
        non_blocking: bool,
    ) -> Option<ServeResult> {
        self.start();

        loop {
            let mut sleep_duration = Duration::from_millis(1);
            {
                let locked_state = self.state.lock().await;
                if let Some(ref sr) = locked_state.serve_result {
                    if prev_frame_id.is_none()
                        || prev_frame_id.unwrap() != sr.frame_result.frame_id
                    {
                        // Clone the result for the caller without consuming it.
                        return Some(ServeResult {
                            frame_result: sr.frame_result.clone(),
                            image_rotator: sr.image_rotator.clone(),
                            scaled_image: sr.scaled_image.clone(),
                            scaled_image_binning_factor: sr.scaled_image_binning_factor,
                            scaled_image_frame_id: sr.scaled_image_frame_id,
                        });
                    }
                }
                if non_blocking {
                    return None;
                }
                if let Some(eta) = locked_state.eta {
                    let time_to_eta =
                        eta.saturating_duration_since(Instant::now());
                    if time_to_eta > sleep_duration {
                        sleep_duration = time_to_eta;
                    }
                }
            }
            tokio::time::sleep(sleep_duration).await;
        }
    }

    pub async fn reset_session_stats(&mut self) {
        self.state
            .lock()
            .await
            .serve_latency_stats
            .reset_session();
    }

    fn start(&mut self) {
        // Restart if the worker terminated unexpectedly.
        if self.worker_thread.is_some()
            && self.worker_thread.as_ref().unwrap().is_finished()
        {
            self.worker_thread.take().unwrap();
        }
        if self.worker_thread.is_none() {
            let cloned_state = self.state.clone();
            let cloned_solve_engine = self.solve_engine.clone();
            let cloned_detect_engine = self.detect_engine.clone();
            self.worker_thread = Some(std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .thread_name("serve_engine")
                    // Single worker suffices: this runtime runs only the
                    // sequential serve worker loop with no concurrent tasks.
                    .worker_threads(1)
                    .build()
                    .unwrap();
                runtime.block_on(async move {
                    ServeEngine::worker(
                        cloned_state,
                        cloned_solve_engine,
                        cloned_detect_engine,
                    )
                    .await;
                });
            }));
        }
    }

    async fn worker(
        state: Arc<tokio::sync::Mutex<ServeState>>,
        solve_engine: Arc<tokio::sync::Mutex<SolveEngine>>,
        detect_engine: Arc<tokio::sync::Mutex<DetectEngine>>,
    ) {
        debug!("Starting serve engine");
        loop {
            let frame_id;
            {
                let mut locked_state = state.lock().await;
                locked_state.eta = None;
                frame_id = locked_state.frame_id;
            }

            // Determine mode from config (read briefly, release lock).
            let (operating_mode, focus_assist_mode, daylight_from_config) = {
                let locked_state = state.lock().await;
                let cfg = &locked_state.context;
                (
                    cfg.operation_settings.operating_mode.unwrap(),
                    cfg.operation_settings.focus_assist_mode.unwrap(),
                    cfg.operation_settings.daylight_mode.unwrap(),
                )
            };

            // Poll the appropriate upstream engine for a new result.
            let (detect_result, plate_solution) =
                if operating_mode == OperatingMode::Setup as i32
                    && (focus_assist_mode || daylight_from_config)
                {
                    // Setup/focus/daylight mode: poll detect engine directly.
                    if let Some(delay_est) = detect_engine
                        .lock()
                        .await
                        .estimate_delay(frame_id)
                        .await
                    {
                        state.lock().await.eta =
                            Some(Instant::now() + delay_est);
                    }
                    loop {
                        let dr = detect_engine
                            .lock()
                            .await
                            .get_next_result(frame_id, /* non_blocking= */ true)
                            .await;
                        if let Some(dr) = dr {
                            break (dr, None);
                        }
                        let short_delay = Duration::from_millis(1);
                        let delay_est = detect_engine
                            .lock()
                            .await
                            .estimate_delay(frame_id)
                            .await;
                        tokio::time::sleep(
                            delay_est.map_or(short_delay, |d| max(d, short_delay)),
                        )
                        .await;
                    }
                } else {
                    // Operate/setup-align mode: poll solve engine.
                    if let Some(delay_est) = solve_engine
                        .lock()
                        .await
                        .estimate_delay(frame_id)
                        .await
                    {
                        state.lock().await.eta =
                            Some(Instant::now() + delay_est);
                    }
                    loop {
                        let ps = solve_engine
                            .lock()
                            .await
                            .get_next_result(frame_id, /* non_blocking= */ true)
                            .await;
                        if let Some(ps) = ps {
                            let dr = ps.detect_result.clone();
                            break (dr, Some(ps));
                        }
                        let short_delay = Duration::from_millis(1);
                        let delay_est = solve_engine
                            .lock()
                            .await
                            .estimate_delay(frame_id)
                            .await;
                        tokio::time::sleep(
                            delay_est.map_or(short_delay, |d| max(d, short_delay)),
                        )
                        .await;
                    }
                };

            // Update our frame_id now that we have a new result.
            state.lock().await.frame_id = Some(detect_result.frame_id);

            let serve_start = Instant::now();

            // Do the serve work outside the state lock.
            let mut serve_result = Self::produce_serve_result(
                &state,
                detect_result,
                plate_solution,
                &solve_engine,
            )
            .await;

            // Post the result.
            {
                let mut locked_state = state.lock().await;
                let elapsed = serve_start.elapsed();
                locked_state
                    .serve_latency_stats
                    .add_value(elapsed.as_secs_f64());
                // Populate serve_latency in the frame result now that we have
                // the timing.
                if let Some(ref mut stats) = serve_result
                    .frame_result
                    .processing_stats
                    .as_mut()
                {
                    stats.serve_latency = Some(
                        locked_state.serve_latency_stats.value_stats.clone(),
                    );
                }
                locked_state.image_rotator =
                    serve_result.image_rotator.clone();
                locked_state.serve_result = Some(serve_result);
            }
        }
    }

    async fn produce_serve_result(
        state: &Arc<tokio::sync::Mutex<ServeState>>,
        detect_result: DetectResult,
        plate_solution: Option<PlateSolution>,
        solve_engine: &Arc<tokio::sync::Mutex<SolveEngine>>,
    ) -> ServeResult {
        // Read context and persistent state (brief lock).
        let (
            ctx_fixed_settings,
            ctx_preferences,
            ctx_operation_settings,
            ctx_calibration_data_arc,
            ctx_imu_tracker,
            ctx_polar_analyzer,
            ctx_normalize_rows,
            ctx_binning,
            ctx_display_sampling,
            ctx_is_color,
            ctx_jpeg_quality,
            ctx_landscape,
            prev_image_rotator,
            center_peak_position_arc,
        ) = {
            let locked_state = state.lock().await;
            let ctx = &locked_state.context;
            (
                ctx.fixed_settings.clone(),
                ctx.preferences.clone(),
                ctx.operation_settings.clone(),
                ctx.calibration_data.clone(),
                ctx.imu_tracker.clone(),
                ctx.polar_analyzer.clone(),
                ctx.normalize_rows,
                ctx.binning,
                ctx.display_sampling,
                ctx.is_color,
                ctx.jpeg_quality,
                ctx.landscape,
                locked_state.image_rotator.clone(),
                locked_state.center_peak_position.clone(),
            )
        };

        let plate_solution_proto =
            if let Some(ref ps) = plate_solution {
                ps.plate_solution.clone()
            } else {
                None
            };

        let captured_image = &detect_result.captured_image;
        let (width, height) = captured_image.image.dimensions();

        let mut frame_result = FrameResult {
            ..Default::default()
        };

        frame_result.frame_id = detect_result.frame_id;
        frame_result.exposure_time = Some(
            prost_types::Duration::try_from(
                captured_image.capture_params.exposure_duration,
            )
            .unwrap(),
        );
        frame_result.capture_time =
            Some(prost_types::Timestamp::from(captured_image.readout_time));
        frame_result.fixed_settings = Some(ctx_fixed_settings.clone());
        frame_result.preferences = Some(ctx_preferences.clone());
        frame_result.operation_settings = Some(ctx_operation_settings.clone());

        let daylight_mode = detect_result.daylight_mode;
        frame_result
            .operation_settings
            .as_mut()
            .unwrap()
            .daylight_mode = Some(daylight_mode);

        // Star candidates.
        let mut centroids = Vec::<StarCentroid>::new();
        for star in &detect_result.star_candidates {
            centroids.push(StarCentroid {
                centroid_position: Some(ImageCoord {
                    x: star.centroid_x,
                    y: star.centroid_y,
                }),
                brightness: star.brightness,
                num_saturated: star.num_saturated as i32,
            });
        }
        frame_result.star_candidates = centroids;
        frame_result.star_count_moving_average =
            detect_result.star_count_moving_average;
        frame_result.noise_estimate = detect_result.noise_estimate;

        let binning = ctx_binning;
        let display_sampling = ctx_display_sampling;

        // Build display image.
        let mut disp_image = &captured_image.image;
        let mut resized_disp_image = disp_image;
        let mut resize_result: Arc<GrayImage>;
        let mut black_level = detect_result.display_black_level;
        let mut peak_value = detect_result.peak_value;

        if detect_result.binned_image.is_some() {
            disp_image = detect_result.binned_image.as_ref().unwrap();
            resized_disp_image = disp_image;
        } else if binning > 1 {
            // This can happen in focus mode, wherein detect engine is skipping
            // Cedar detect and thus not creating a binned image.
            resize_result = Arc::new(
                bin_and_histogram_2x2(disp_image, ctx_normalize_rows).binned,
            );
            resized_disp_image = &resize_result;
            if binning == 4 {
                resize_result = Arc::new(
                    bin_and_histogram_2x2(&resize_result, false).binned,
                );
                resized_disp_image = &resize_result;
            }
        }
        if display_sampling {
            resize_result = Arc::new(
                bin_and_histogram_2x2(resized_disp_image, false).binned,
            );
            resized_disp_image = &resize_result;
            // Adjust peak_value; binning can make point sources dimmer.
            peak_value /= 4;
        }
        if black_level > peak_value {
            black_level = peak_value;
        }
        let binning_factor = binning * if display_sampling { 2 } else { 1 };

        // Location-based info (alt/az, zenith roll) from plate solution.
        if let Some(ref psp) = plate_solution_proto {
            let celestial_coords = if psp.target_sky_coord.is_empty() {
                psp.image_sky_coord.as_ref().unwrap().clone()
            } else {
                psp.target_sky_coord[0].clone()
            };
            let bs_ra = celestial_coords.ra.to_radians();
            let bs_dec = celestial_coords.dec.to_radians();
            if ctx_fixed_settings.observer_location.is_some() {
                let geo_location =
                    ctx_fixed_settings.observer_location.clone().unwrap();
                let lat = geo_location.latitude.to_radians();
                let long = geo_location.longitude.to_radians();
                let time = &captured_image.readout_time;
                let (bs_alt, bs_az, bs_ha) =
                    alt_az_from_equatorial(bs_ra, bs_dec, lat, long, time);
                let (z_ra, z_dec) = equatorial_from_alt_az(
                    90_f64.to_radians(),
                    0.0,
                    lat,
                    long,
                    time,
                );
                let mut zenith_roll_angle =
                    (position_angle(bs_ra, bs_dec, z_ra, z_dec).to_degrees()
                        + psp.roll)
                        % 360.0;
                if zenith_roll_angle < 0.0 {
                    zenith_roll_angle += 360.0;
                }
                frame_result.location_based_info = Some(LocationBasedInfo {
                    zenith_roll_angle,
                    altitude: bs_alt.to_degrees(),
                    azimuth: bs_az.to_degrees(),
                    hour_angle: bs_ha.to_degrees(),
                });
            }
        }

        // Determine image rotation.
        let image_rotator =
            if detect_result.focus_aid.is_some() || daylight_mode {
                ImageRotator::new(0.0)
            } else if let Some(ref mut lbi) =
                frame_result.location_based_info.as_mut()
            {
                let zenith_roll_angle = lbi.zenith_roll_angle;
                let image_rotate_angle = if ctx_landscape {
                    -zenith_roll_angle
                } else {
                    90.0 - zenith_roll_angle
                };
                lbi.zenith_roll_angle += image_rotate_angle;
                if let Some(ref mut psp) =
                    plate_solution_proto.as_ref().map(|p| p.clone())
                {
                    psp.roll = (psp.roll + image_rotate_angle) % 360.0;
                    if psp.roll < 0.0 {
                        psp.roll += 360.0;
                    }
                }
                ImageRotator::new(image_rotate_angle)
            } else {
                // Plate solve dropout: carry over previous rotator.
                prev_image_rotator
            };

        let irr = &image_rotator;
        resize_result =
            Arc::new(irr.rotate_image_and_crop(resized_disp_image));
        resized_disp_image = &resize_result;
        let disp_image_rectangle = irr.get_cropped_region(width, height);

        // Focus assist / center peak handling.
        if let Some(fa) = &detect_result.focus_aid {
            if let Some(center_peak_pos) = fa.center_peak_position {
                let mut ic = ImageCoord {
                    x: center_peak_pos.0,
                    y: center_peak_pos.1,
                };
                (ic.x, ic.y) =
                    irr.transform_to_rotated(ic.x, ic.y, width, height);
                *center_peak_position_arc.lock().await = Some(ic.clone());
                frame_result.center_peak_position = Some(ic);
            }
            if let Some(center_peak_val) = fa.center_peak_value {
                frame_result.center_peak_value = Some(center_peak_val as i32);
            }

            if let (Some(center_peak_image), Some(peak_image_region)) =
                (&fa.peak_image, &fa.peak_image_region)
            {
                let (cp_binning_factor, center_peak_jpg_buf) = if ctx_is_color {
                    let binned = Self::bin_2x2(center_peak_image);
                    (2, Self::jpeg_encode(&binned, ctx_jpeg_quality))
                } else {
                    (1, Self::jpeg_encode(center_peak_image, ctx_jpeg_quality))
                };
                frame_result.center_peak_image = Some(Image {
                    binning_factor: cp_binning_factor,
                    rotation_size_ratio: 1.0,
                    rectangle: Some(Rectangle {
                        origin_x: peak_image_region.left(),
                        origin_y: peak_image_region.top(),
                        width: peak_image_region.width() as i32,
                        height: peak_image_region.height() as i32,
                    }),
                    image_data: center_peak_jpg_buf,
                });
            }

            if let (Some(daylight_focus_image), Some(daylight_focus_region)) =
                (&fa.daylight_focus_zoom_image, &fa.daylight_focus_zoom_region)
            {
                let (df_binning_factor, daylight_focus_jpg_buf) =
                    if ctx_is_color {
                        let binned = Self::bin_2x2(daylight_focus_image);
                        (2, Self::jpeg_encode(&binned, ctx_jpeg_quality))
                    } else {
                        (
                            1,
                            Self::jpeg_encode(
                                daylight_focus_image,
                                ctx_jpeg_quality,
                            ),
                        )
                    };
                frame_result.daylight_focus_zoom_image = Some(Image {
                    binning_factor: df_binning_factor,
                    rotation_size_ratio: 1.0,
                    rectangle: Some(Rectangle {
                        origin_x: daylight_focus_region.left(),
                        origin_y: daylight_focus_region.top(),
                        width: daylight_focus_region.width() as i32,
                        height: daylight_focus_region.height() as i32,
                    }),
                    image_data: daylight_focus_jpg_buf,
                });
            }
        } else {
            *center_peak_position_arc.lock().await = None;
        }

        // Scale and encode main display image.
        let gamma = if daylight_mode { 1.0 } else { 0.7 };
        let scaled_image =
            scale_image(resized_disp_image, black_level, peak_value, gamma);
        let scaled_image = Arc::new(scaled_image);
        let jpg_buf = Self::jpeg_encode(&scaled_image, ctx_jpeg_quality);
        let scaled_image_frame_id = frame_result.frame_id;
        frame_result.image = Some(Image {
            binning_factor: binning_factor as i32,
            rotation_size_ratio: 1.0,
            rectangle: Some(disp_image_rectangle),
            image_data: jpg_buf,
        });

        // Processing stats.
        frame_result.processing_stats = Some(ProcessingStats {
            ..Default::default()
        });
        let stats = frame_result.processing_stats.as_mut().unwrap();
        stats.acquire_latency = Some(detect_result.acquire_latency_stats);
        stats.detect_latency = Some(detect_result.detect_latency_stats);
        // serve_latency is populated in the worker after timing completes.

        if let Some(mut ps) = plate_solution {
            stats.solve_latency = Some(ps.solve_latency_stats.clone());
            stats.solve_attempt_fraction =
                Some(ps.solve_attempt_stats.clone());
            stats.solve_success_fraction =
                Some(ps.solve_success_stats.clone());
            stats.solve_interval = Some(ps.solve_interval_stats.clone());
            frame_result.slew_request = ps.slew_request.clone();

            if let Some(boresight_image) = &ps.boresight_image {
                let (bs_binning_factor, resized_boresight_image) =
                    if ctx_is_color {
                        (2, Self::bin_2x2(boresight_image))
                    } else {
                        (1, boresight_image.clone())
                    };
                let rotated_boresight_image =
                    irr.rotate_image_and_crop(&resized_boresight_image);
                let jpg_buf = Self::jpeg_encode(
                    &rotated_boresight_image,
                    ctx_jpeg_quality,
                );
                let bsi_rect = ps.boresight_image_region.unwrap();
                frame_result.boresight_image = Some(Image {
                    binning_factor: bs_binning_factor,
                    rotation_size_ratio: 1.0,
                    rectangle: Some(Rectangle {
                        origin_x: bsi_rect.left(),
                        origin_y: bsi_rect.top(),
                        width: bsi_rect.width() as i32,
                        height: bsi_rect.height() as i32,
                    }),
                    image_data: jpg_buf,
                });
            }

            // Slew request image position and angle transforms.
            if let Some(ref mut slew_request) = frame_result.slew_request {
                if slew_request.image_pos.is_some() {
                    let pos = slew_request.image_pos.as_ref().unwrap();
                    let (rx, ry) =
                        irr.transform_to_rotated(pos.x, pos.y, width, height);
                    let square_size = height as f64;
                    if rx >= 0.0
                        && rx < square_size
                        && ry >= 0.0
                        && ry < square_size
                    {
                        let slew_target_image_pos =
                            slew_request.image_pos.as_mut().unwrap();
                        slew_target_image_pos.x = rx;
                        slew_target_image_pos.y = ry;
                    } else {
                        slew_request.image_pos = None;
                    }
                }
                if let Some(ta) = slew_request.target_angle {
                    slew_request.target_angle =
                        Some((ta + irr.angle()) % 360.0);
                }
            }

            // Slew request offsets (equatorial and alt/az mounts).
            if let Some(ref psp) = ps.plate_solution {
                if let Some(ref mut slew_request) = frame_result.slew_request {
                    let celestial_coords = if psp.target_sky_coord.is_empty() {
                        psp.image_sky_coord.as_ref().unwrap().clone()
                    } else {
                        psp.target_sky_coord[0].clone()
                    };
                    let bs_ra = celestial_coords.ra.to_radians();
                    let bs_dec = celestial_coords.dec.to_radians();
                    let target_ra =
                        slew_request.target.as_ref().unwrap().ra;
                    let target_dec =
                        slew_request.target.as_ref().unwrap().dec;
                    let mount_type = ctx_preferences.mount_type;
                    if mount_type == Some(MountType::Equatorial.into()) {
                        let mut rel_ra = target_ra - bs_ra.to_degrees();
                        if rel_ra < -180.0 {
                            rel_ra += 360.0;
                        }
                        if rel_ra > 180.0 {
                            rel_ra -= 360.0;
                        }
                        slew_request.offset_rotation_axis = Some(rel_ra);
                        let rel_dec = target_dec - bs_dec.to_degrees();
                        slew_request.offset_tilt_axis = Some(rel_dec);
                    }
                    if ctx_fixed_settings.observer_location.is_some()
                        && mount_type == Some(MountType::AltAz.into())
                    {
                        let geo_location = ctx_fixed_settings
                            .observer_location
                            .clone()
                            .unwrap();
                        let lat = geo_location.latitude.to_radians();
                        let long = geo_location.longitude.to_radians();
                        let time = &captured_image.readout_time;
                        let (bs_alt, bs_az, _) = alt_az_from_equatorial(
                            bs_ra, bs_dec, lat, long, time,
                        );
                        let (target_alt, target_az, _) =
                            alt_az_from_equatorial(
                                target_ra.to_radians(),
                                target_dec.to_radians(),
                                lat,
                                long,
                                time,
                            );
                        let mut rel_az =
                            target_az.to_degrees() - bs_az.to_degrees();
                        if rel_az < -180.0 {
                            rel_az += 360.0;
                        }
                        if rel_az > 180.0 {
                            rel_az -= 360.0;
                        }
                        slew_request.offset_rotation_axis = Some(rel_az);
                        let rel_alt =
                            target_alt.to_degrees() - bs_alt.to_degrees();
                        slew_request.offset_tilt_axis = Some(rel_alt);
                    }
                }
            }

            // FOV catalog entries.
            if let Some(fces) = &mut ps.fov_catalog_entries {
                frame_result.labeled_catalog_entries =
                    Vec::<FovCatalogEntry>::with_capacity(fces.len());
                for fce in fces.iter_mut() {
                    let pos = fce.image_pos.as_mut().unwrap();
                    (pos.x, pos.y) =
                        irr.transform_to_rotated(pos.x, pos.y, width, height);
                    frame_result.labeled_catalog_entries.push(fce.clone());
                }
            }
            if let Some(decrowded_fces) =
                &mut ps.decrowded_fov_catalog_entries
            {
                frame_result.unlabeled_catalog_entries =
                    Vec::<FovCatalogEntry>::with_capacity(decrowded_fces.len());
                for fce in decrowded_fces.iter_mut() {
                    let pos = fce.image_pos.as_mut().unwrap();
                    (pos.x, pos.y) =
                        irr.transform_to_rotated(pos.x, pos.y, width, height);
                    frame_result
                        .unlabeled_catalog_entries
                        .push(fce.clone());
                }
            }

            if let Some(ref psp) = ps.plate_solution {
                frame_result.plate_solution = Some(psp.clone());
            }
        } // plate_solution

        // Boresight position.
        let boresight_position =
            solve_engine.lock().await.boresight_pixel().await;
        let (bs_x, bs_y) = if let Some(bs) = boresight_position {
            (bs.x, bs.y)
        } else {
            (width as f64 / 2.0, height as f64 / 2.0)
        };
        frame_result.boresight_position = Some(ImageCoord {
            x: bs_x,
            y: bs_y,
        });

        let operating_mode = ctx_operation_settings.operating_mode.unwrap();
        let focus_assist_mode = detect_result.focus_aid.is_some();

        // Setup align mode: replace star candidates with plate solve catalog
        // stars.
        if operating_mode == OperatingMode::Setup as i32 && !focus_assist_mode {
            if let Some(ref psp) = plate_solution_proto {
                frame_result.star_candidates = Vec::<StarCentroid>::new();
                for star in &psp.catalog_stars {
                    let ic = star.pixel.clone().unwrap();
                    let distance_from_center = ((width as f64 / 2.0 - ic.x)
                        * (width as f64 / 2.0 - ic.x)
                        + (height as f64 / 2.0 - ic.y)
                            * (height as f64 / 2.0 - ic.y))
                        .sqrt();
                    if distance_from_center > height as f64 / 2.0 {
                        continue;
                    }
                    frame_result.star_candidates.push(StarCentroid {
                        centroid_position: Some(ImageCoord {
                            x: ic.x,
                            y: ic.y,
                        }),
                        brightness: magnitude_intensity_ratio(
                            6.0,
                            star.mag as f64,
                        ),
                        num_saturated: 0,
                    });
                }
            }
        }

        // Transform boresight and star centroid coordinates.
        let bp = frame_result.boresight_position.as_mut().unwrap();
        (bp.x, bp.y) = irr.transform_to_rotated(bp.x, bp.y, width, height);
        for star_centroid in &mut frame_result.star_candidates {
            let cp = star_centroid.centroid_position.as_mut().unwrap();
            (cp.x, cp.y) = irr.transform_to_rotated(cp.x, cp.y, width, height);
        }

        // Setup align mode: augment detections with catalog items.
        if operating_mode == OperatingMode::Setup as i32 && !focus_assist_mode {
            frame_result.star_candidates = fill_in_detections(
                &frame_result.star_candidates,
                &frame_result.labeled_catalog_entries,
            );
        }

        // Calibration data.
        frame_result.calibration_data =
            Some(ctx_calibration_data_arc.lock().await.clone());

        // IMU calibration quality.
        if let Some(imu_tracker) = &ctx_imu_tracker {
            let locked_imu = imu_tracker.lock().await;
            let cal_data = frame_result.calibration_data.as_mut().unwrap();
            let (zero_bias, transform_calibration) =
                locked_imu.get_calibration().await;
            if let Some(zb) = zero_bias {
                cal_data.gyro_zero_bias_x = Some(zb.x);
                cal_data.gyro_zero_bias_y = Some(zb.y);
                cal_data.gyro_zero_bias_z = Some(zb.z);
            }
            if let Some(tc) = transform_calibration {
                cal_data.gyro_transform_error_fraction =
                    Some(tc.transform_error_fraction);
                cal_data.camera_view_gyro_axis =
                    Some(tc.camera_view_gyro_axis);
                cal_data.camera_view_misalignment =
                    Some(tc.camera_view_misalignment);
                cal_data.camera_up_gyro_axis = Some(tc.camera_up_gyro_axis);
                cal_data.camera_up_misalignment =
                    Some(tc.camera_up_misalignment);
            }
        }

        // Polar alignment advice.
        frame_result.polar_align_advice = Some(
            ctx_polar_analyzer
                .lock()
                .await
                .get_polar_align_advice(),
        );

        ServeResult {
            frame_result,
            image_rotator: image_rotator.clone(),
            scaled_image: Some(scaled_image),
            scaled_image_binning_factor: binning_factor,
            scaled_image_frame_id,
        }
    }

    fn bin_2x2(image: &GrayImage) -> GrayImage {
        bin_and_histogram_2x2(image, /* normalize_rows= */ false).binned
    }

    fn jpeg_encode(img: &GrayImage, jpeg_quality: u8) -> Vec<u8> {
        let (width, height) = img.dimensions();
        let image = turbojpeg::Image {
            pixels: img.as_raw().as_slice(),
            width: width as usize,
            pitch: width as usize,
            height: height as usize,
            format: turbojpeg::PixelFormat::GRAY,
        };
        let mut compressor = turbojpeg::Compressor::new().unwrap();
        compressor.set_quality(jpeg_quality as i32).unwrap();
        compressor.set_subsamp(turbojpeg::Subsamp::Gray).unwrap();
        compressor.compress_to_vec(image).unwrap()
    }
}

// The result produced by the serve engine each frame.
pub struct ServeResult {
    // NOT populated (filled in by get_frame): calibrating,
    // calibration_progress, skip_focus_active, has_result,
    // processing_stats.serve_latency.
    pub frame_result: FrameResult,

    // The most recently computed image rotator, for use by other handlers.
    pub image_rotator: ImageRotator,

    // Cached scaled display image, used during calibration.
    pub scaled_image: Option<Arc<GrayImage>>,
    pub scaled_image_binning_factor: u32,
    pub scaled_image_frame_id: i32,
}