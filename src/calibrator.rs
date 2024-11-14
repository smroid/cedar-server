// Copyright (c) 2024 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use std::ops::Deref;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use image::GrayImage;
use imageproc::stats::histogram;
use log::warn;

use cedar_camera::abstract_camera::{AbstractCamera, CapturedImage, Gain, Offset};
use canonical_error::{CanonicalError,
                      aborted_error, failed_precondition_error, internal_error,
                      deadline_exceeded_error, unknown_error};
use cedar_detect::algorithm::{StarDescription,
                              estimate_noise_from_image, get_stars_from_image};
use crate::solve_engine::SolveEngine;
use crate::tetra3_server::{ImageCoord, SolveRequest, SolveStatus};

pub struct Calibrator {
    camera: Arc<tokio::sync::Mutex<Box<dyn AbstractCamera + Send>>>,

    // Determines whether rows are normalized to have the same dark level.
    normalize_rows: bool,
}

// By convention, all methods restore any camera settings that they alter.
impl Calibrator {
    pub fn new(camera: Arc<tokio::sync::Mutex<Box<dyn AbstractCamera + Send>>>,
               normalize_rows: bool) -> Self{
        Calibrator{camera, normalize_rows}
    }

    pub fn replace_camera(
        &mut self, camera: Arc<tokio::sync::Mutex<Box<dyn AbstractCamera + Send>>>)
    {
        self.camera = camera.clone();
    }

    pub async fn calibrate_offset(
        &self, cancel_calibration: Arc<Mutex<bool>>)
        -> Result<Offset, CanonicalError> {
        // Goal: find the minimum camera offset setting that avoids
        // black crush (too many zero-value pixels).
        //
        // Assumption: camera is pointed at sky which is mostly dark.
        //
        // Approach:
        // * Use 1ms exposures.
        // * Starting at offset=0, as long as >0.1% of pixels have zero
        //   value, increase the offset.
        if *cancel_calibration.lock().unwrap() {
            return Err(aborted_error("Cancelled during calibrate_offset()."));
        }
        let _restore_settings = RestoreSettings::new(self.camera.clone());

        self.camera.lock().await.set_exposure_duration(Duration::from_millis(1))?;
        let (width, height) = self.camera.lock().await.dimensions();
        let total_pixels = width * height;

        let max_offset = 20;
        let mut prev_frame_id: Option<i32> = None;
        let mut num_zero_pixels = 0;
        for mut offset in 0..=max_offset {
            if *cancel_calibration.lock().unwrap() {
                return Err(aborted_error("Cancelled during calibrate_offset()."));
            }
            self.camera.lock().await.set_offset(Offset::new(offset))?;
            let (captured_image, frame_id) =
                Self::capture_image(self.camera.clone(), prev_frame_id).await?;
            prev_frame_id = Some(frame_id);
            let channel_histogram = histogram(captured_image.image.deref());
            let histo = channel_histogram.channels[0];
            num_zero_pixels = histo[0];
            if num_zero_pixels < (total_pixels / 1000) as u32 {
                if offset < max_offset {
                    offset += 1;  // One more for good measure.
                }
                return Ok(Offset::new(offset));
            }
        }
        Err(failed_precondition_error(format!("Still have {} zero pixels at offset={}",
                                              num_zero_pixels, max_offset).as_str()))
    }

    async fn capture_image(
        camera: Arc<tokio::sync::Mutex<Box<dyn AbstractCamera + Send>>>,
        frame_id: Option<i32>) -> Result<(CapturedImage, i32), CanonicalError>
    {
        // Don't hold camera lock for the entirety of the time waiting for
        // the next image.
        loop {
            let capture =
                match camera.lock().await.try_capture_image(frame_id).await
            {
                Ok(c) => c,
                Err(e) => { return Err(e); }
            };
            if capture.is_none() {
                tokio::time::sleep(Duration::from_millis(1)).await;
                continue;
            }
            let (image, id) = capture.unwrap();
            return Ok((image, id));
        }
    }

    pub async fn calibrate_exposure_duration(
        &self,
        setup_exposure_duration: Duration,
        max_exposure_duration: Duration,
        star_count_goal: i32,
        detection_binning: u32, detection_sigma: f64,
        cancel_calibration: Arc<Mutex<bool>>)
        -> Result<Duration, CanonicalError> {
        // Goal: find the camera exposure duration that yields the desired
        // number of detected stars.
        //
        // Assumption: camera is focused and pointed at sky with stars. The
        // passed `setup_exposure_duration` yields at least one star (the
        // brightest star in the central region) which was used for focusing
        // and aligning.
        //
        // Approach:
        // * Using the `setup_exposure_duration`
        //   * Grab an image.
        //   * Detect the stars.
        //   * If close enough to the goal, scale the exposure duration and
        //     return it.
        //   * If not close to the goal, scale the exposure duration and
        //     do one more exposure/detect/scale.
        if *cancel_calibration.lock().unwrap() {
            return Err(aborted_error(
                "Cancelled during calibrate_exposure_duration()."));
        }
        let _restore_settings = RestoreSettings::new(self.camera.clone());

        self.camera.lock().await.set_exposure_duration(setup_exposure_duration)?;
        let (_, mut stars, frame_id) = self.acquire_image_get_stars(
            /*frame_id=*/None, detection_binning, detection_sigma).await?;

        let mut num_stars_detected = stars.len();
        // >1 if we have more stars than goal; <1 if fewer stars than goal.
        let mut star_goal_fraction =
            f64::max(num_stars_detected as f64, 1.0) / star_count_goal as f64;
        let mut scaled_exposure_duration_secs =
            setup_exposure_duration.as_secs_f64() / star_goal_fraction;
        if star_goal_fraction > 0.8 && star_goal_fraction < 1.2 {
            // Close enough to goal, the scaled exposure time is good.
            return Ok(Duration::from_secs_f64(scaled_exposure_duration_secs));
        }
        if *cancel_calibration.lock().unwrap() {
            return Err(aborted_error(
                "Cancelled during calibrate_exposure_duration()."));
        }

        // Iterate with the refined exposure duration.
        if scaled_exposure_duration_secs > max_exposure_duration.as_secs_f64() {
            scaled_exposure_duration_secs = max_exposure_duration.as_secs_f64();
        }
        self.camera.lock().await.set_exposure_duration(
            Duration::from_secs_f64(scaled_exposure_duration_secs))?;
        (_, stars, _) = self.acquire_image_get_stars(
            Some(frame_id), detection_binning, detection_sigma).await?;

        num_stars_detected = stars.len();
        // >1 if we have more stars than goal; <1 if fewer stars than goal.
        star_goal_fraction =
            f64::max(num_stars_detected as f64, 1.0) / star_count_goal as f64;
        scaled_exposure_duration_secs /= star_goal_fraction;
        if star_goal_fraction > 0.8 && star_goal_fraction < 1.2 {
            // Close enough to goal, the scaled exposure time is good.
            return Ok(Duration::from_secs_f64(scaled_exposure_duration_secs));
        }

        // Iterate one more time.
        if scaled_exposure_duration_secs > max_exposure_duration.as_secs_f64() {
            scaled_exposure_duration_secs = max_exposure_duration.as_secs_f64();
        }
        self.camera.lock().await.set_exposure_duration(
            Duration::from_secs_f64(scaled_exposure_duration_secs))?;
        (_, stars, _) = self.acquire_image_get_stars(
            Some(frame_id), detection_binning, detection_sigma).await?;

        num_stars_detected = stars.len();
        if num_stars_detected < (star_count_goal / 5) as usize {
            return Err(failed_precondition_error(
                format!("Too few stars detected ({})", num_stars_detected).as_str()))
        }
        star_goal_fraction =
            f64::max(num_stars_detected as f64, 1.0) / star_count_goal as f64;
        if star_goal_fraction < 0.5 || star_goal_fraction > 2.0 {
            warn!("Exposure time calibration diverged, goal fraction {}",
                  star_goal_fraction);
        }

        scaled_exposure_duration_secs /= star_goal_fraction;
        Ok(Duration::from_secs_f64(scaled_exposure_duration_secs))
    }

    // Result is FOV (degrees), lens distortion, match_max_error, solve duration.
    pub async fn calibrate_optical(
        &self,
        solve_engine: Arc<tokio::sync::Mutex<SolveEngine>>,
        exposure_duration: Duration,
        solve_timeout: Duration,
        detection_binning: u32, detection_sigma: f64)
        -> Result<(f64, f64, f64, Duration), CanonicalError> {
        // Goal: find the field of view, lens distortion, match_max_error solver
        // parameter, and representative plate solve time.
        //
        // Assumption: camera is focused and pointed at sky with stars.
        //
        // Approach:
        // * Grab an image, detect the stars.
        // * Do a plate solution with no FOV estimate and no distortion estimate.
        //   Use a generous match_max_error value and a generous solve_timeout.
        // * Use the plate solution to obtain FOV and lens distortion, and determine
        //   an appropriate match_max_error value.
        // * Do another plate solution with the known FOV, lens distortion, and
        //   match_max_error to obtain a representative solution time.
        let _restore_settings = RestoreSettings::new(self.camera.clone());

        self.camera.lock().await.set_exposure_duration(exposure_duration)?;
        let (image, stars, _) = self.acquire_image_get_stars(
            /*frame_id=*/None, detection_binning, detection_sigma).await?;
        let (width, height) = image.dimensions();

        // Set up SolveRequest.
        let mut solve_request = SolveRequest::default();
        solve_request.fov_estimate = None;
        solve_request.fov_max_error = None;
        solve_request.solve_timeout =
            Some(prost_types::Duration::try_from(solve_timeout).unwrap());
        solve_request.distortion = Some(0.0);
        solve_request.return_matches = false;
        solve_request.match_max_error = Some(0.005);
        for star in &stars {
            solve_request.star_centroids.push(ImageCoord{x: star.centroid_x,
                                                         y: star.centroid_y});
        }
        solve_request.image_width = width as i32;
        solve_request.image_height = height as i32;

        let mut solve_result_proto = solve_engine.lock().await.solve(solve_request.clone()).await?;
        let mut solve_duration = std::time::Duration::try_from(
            solve_result_proto.solve_time.unwrap()).unwrap();
        if solve_result_proto.status.unwrap() == SolveStatus::MatchFound as i32 {
            let fov = solve_result_proto.fov.unwrap();  // Degrees.
            let distortion = solve_result_proto.distortion.unwrap();

            // Use the 90th percentile error residual as a basis for determining the
            // 'match_max_error' argument to the solver.
            let p90_error_deg = solve_result_proto.p90e.unwrap() / 3600.0;
            let p90_err_frac = p90_error_deg / fov;  // As fraction of FOV.
            let match_max_error = p90_err_frac * 2.0;

            // Do another solve with now-known FOV, distortion, and
            // match_max_error, to get a more representative solve_duration.
            solve_request.fov_estimate = Some(fov);
            solve_request.fov_max_error = Some(fov / 10.0);
            solve_request.distortion = Some(distortion);
            solve_request.match_max_error = Some(match_max_error);

            solve_result_proto = solve_engine.lock().await.solve(solve_request).await?;
            solve_duration = std::time::Duration::try_from(
                solve_result_proto.solve_time.unwrap()).unwrap();
            if solve_result_proto.status.unwrap() == SolveStatus::MatchFound as i32 {
                return Ok((fov, distortion, match_max_error, solve_duration));
            }
        }
        let status_enum =
            SolveStatus::try_from(solve_result_proto.status.unwrap()).unwrap();
        let msg = format!("SolveStatus::{:?}: elapsed time {:?}",
                          status_enum, solve_duration);
        match status_enum {
            SolveStatus::Unspecified => Err(unknown_error(msg.as_str())),
            SolveStatus::MatchFound => Err(internal_error(msg.as_str())),
            SolveStatus::NoMatch => Err(failed_precondition_error(msg.as_str())),
            SolveStatus::Timeout => Err(deadline_exceeded_error(msg.as_str())),
            SolveStatus::Cancelled => Err(aborted_error(msg.as_str())),
            SolveStatus::TooFew => Err(failed_precondition_error(msg.as_str())),
        }
    }

    async fn acquire_image_get_stars(
        &self, frame_id: Option<i32>,
        detection_binning: u32, detection_sigma: f64)
        -> Result<(Arc<GrayImage>, Vec<StarDescription>, i32), CanonicalError>
    {
        let (captured_image, frame_id) =
            Self::capture_image(self.camera.clone(), frame_id).await?;
        // Run CedarDetect on the image.
        let image = &captured_image.image;
        let noise_estimate = estimate_noise_from_image(&image);
        let (stars, _, _, _) =
            get_stars_from_image(&image, noise_estimate,
                                 detection_sigma, /*deprecated_max_size=*/1,
                                 self.normalize_rows, detection_binning,
                                 /*detect_hot_pixels*/true,
                                 /*return_binned_image=*/false);
        Ok((image.clone(), stars, frame_id))
    }
}

// RAII gadget for saving/restoring camera settings.
struct RestoreSettings {
    camera: Arc<tokio::sync::Mutex<Box<dyn AbstractCamera + Send>>>,
    gain: Gain,
    offset: Offset,
    exp_duration: Duration,
}
impl RestoreSettings {
    async fn new(camera: Arc<tokio::sync::Mutex<Box<dyn AbstractCamera + Send>>>) -> Self {
        let locked_camera = camera.lock().await;
        RestoreSettings{
            camera: camera.clone(),
            gain: locked_camera.get_gain(),
            offset: locked_camera.get_offset(),
            exp_duration: locked_camera.get_exposure_duration(),
        }
    }

    async fn restore(&mut self) {
        let mut locked_camera = self.camera.lock().await;
        locked_camera.set_gain(self.gain).unwrap();
        let _ = locked_camera.set_offset(self.offset);  // Ignore unsupported offset.
        locked_camera.set_exposure_duration(self.exp_duration).unwrap();
    }
}
impl Drop for RestoreSettings {
    fn drop(&mut self) {
        // https://stackoverflow.com/questions/71541765/rust-async-drop
        futures::executor::block_on(self.restore());
    }
}
