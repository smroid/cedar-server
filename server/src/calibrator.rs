// Copyright (c) 2024 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use std::ops::Deref;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use image::GrayImage;
use imageproc::stats::histogram;
use log::warn;

use cedar_camera::abstract_camera::{AbstractCamera, CapturedImage, Offset};
use canonical_error::{CanonicalError,
                      aborted_error, failed_precondition_error, internal_error};
use cedar_detect::algorithm::{StarDescription,
                              estimate_noise_from_image, get_stars_from_image};
use cedar_detect::histogram_funcs::stats_for_histogram;
use cedar_elements::solver_trait::{
    SolveExtension, SolveParams, SolverTrait};
use cedar_elements::cedar::ImageCoord;

pub struct Calibrator {
    camera: Arc<tokio::sync::Mutex<Box<dyn AbstractCamera + Send>>>,

    // Determines whether rows are normalized to have the same dark level.
    normalize_rows: bool,
}

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

    // Leaves camera set to the returned calibrated offset value.
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

        // Set offset before changing exposure; if we can't set offset this
        // lets us avoid changing the exposure only to have to restore it.
        self.camera.lock().await.set_offset(Offset::new(0))?;

        // Restore the exposure duration that we change here.
        let _restore_exposure = RestoreExposure::new(self.camera.clone()).await;
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

    // Leaves camera set to the returned calibrated exposure duration. If an
    // error occurs, the camera's exposure duration is restored to its value on
    // entry.
    pub async fn calibrate_exposure_duration(
        &self,
        initial_exposure_duration: Duration,
        max_exposure_duration: Duration,
        star_count_goal: i32,
        detection_binning: u32, detection_sigma: f64,
        cancel_calibration: Arc<Mutex<bool>>)
        -> Result<Duration, CanonicalError> {
        // Goal: find the camera exposure duration that yields the desired
        // number of detected stars.
        //
        // Assumption: camera is focused and pointed at sky with stars. The
        // passed `initial_exposure_duration` is expected to yield at least one
        // star.
        //
        // Approach:
        // * Using the `initial_exposure_duration`
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

        // If we fail, restore the exposure duration that we change here.
        let mut restore_exposure = RestoreExposure::new(self.camera.clone()).await;

        self.camera.lock().await.set_exposure_duration(initial_exposure_duration)?;
        let (_, mut stars, frame_id, mut histogram) = self.acquire_image_get_stars(
            /*frame_id=*/None, detection_binning, detection_sigma,
            cancel_calibration.clone()).await?;

        let mut num_stars_detected = stars.len();
        // >1 if we have more stars than goal; <1 if fewer stars than goal.
        let mut star_goal_fraction =
            f64::max(num_stars_detected as f64, 1.0) / star_count_goal as f64;
        let mut scaled_exposure_duration_secs =
            initial_exposure_duration.as_secs_f64() / star_goal_fraction;
        if star_goal_fraction > 0.8 && star_goal_fraction < 1.2 {
            // Close enough to goal, the scaled exposure time is good.
            let exp = Duration::from_secs_f64(scaled_exposure_duration_secs);
            self.camera.lock().await.set_exposure_duration(exp)?;
            restore_exposure.deactivate();
            return Ok(exp);
        }
        if *cancel_calibration.lock().unwrap() {
            return Err(aborted_error(
                "Cancelled during calibrate_exposure_duration()."));
        }

        // Iterate with the refined exposure duration.
        if scaled_exposure_duration_secs >= max_exposure_duration.as_secs_f64() {
            // We've saturated available exposure time latitude based on detected
            // star count (or lack thereof). Keep things sane by adjusting the
            // overall scene exposure.
            let stats = stats_for_histogram(&histogram);
            let mean = if stats.mean < 1.0 { 1.0 } else { stats.mean };
            // Push image towards moderately low level.
            let correction_factor = 32.0 / mean;
            scaled_exposure_duration_secs =
                initial_exposure_duration.as_secs_f64() * correction_factor;
        }
        self.camera.lock().await.set_exposure_duration(
            Duration::from_secs_f64(scaled_exposure_duration_secs))?;
        (_, stars, _, histogram) = self.acquire_image_get_stars(
            Some(frame_id), detection_binning, detection_sigma,
            cancel_calibration.clone()).await?;

        num_stars_detected = stars.len();
        // >1 if we have more stars than goal; <1 if fewer stars than goal.
        star_goal_fraction =
            f64::max(num_stars_detected as f64, 1.0) / star_count_goal as f64;
        scaled_exposure_duration_secs /= star_goal_fraction;
        if star_goal_fraction > 0.8 && star_goal_fraction < 1.2 {
            // Close enough to goal, the scaled exposure time is good.
            let exp = Duration::from_secs_f64(scaled_exposure_duration_secs);
            self.camera.lock().await.set_exposure_duration(exp)?;
            restore_exposure.deactivate();
            return Ok(exp);
        }
        if *cancel_calibration.lock().unwrap() {
            return Err(aborted_error(
                "Cancelled during calibrate_exposure_duration()."));
        }

        // Iterate one more time.
        if scaled_exposure_duration_secs >= max_exposure_duration.as_secs_f64() {
            // We've saturated available exposure time latitude based on detected
            // star count (or lack thereof). Keep things sane by adjusting the
            // overall scene exposure.

            // Back out the scaling based on star count.
            scaled_exposure_duration_secs *= star_goal_fraction;

            let stats = stats_for_histogram(&histogram);
            let mean = if stats.mean < 1.0 { 1.0 } else { stats.mean };
            // Push image towards moderately low level.
            let correction_factor = 64.0 / mean;
            scaled_exposure_duration_secs *= correction_factor;
        }
        self.camera.lock().await.set_exposure_duration(
            Duration::from_secs_f64(scaled_exposure_duration_secs))?;
        (_, stars, _, _) = self.acquire_image_get_stars(
            Some(frame_id), detection_binning, detection_sigma,
            cancel_calibration.clone()).await?;

        num_stars_detected = stars.len();
        if num_stars_detected < (star_count_goal / 5) as usize {
            return Err(failed_precondition_error(
                format!("Too few stars detected ({})", num_stars_detected).as_str()))
        }
        star_goal_fraction =
            f64::max(num_stars_detected as f64, 1.0) / star_count_goal as f64;
        if star_goal_fraction > 0.8 && star_goal_fraction < 1.2 {
            // Close enough to goal, the scaled exposure time is good.
            let exp = Duration::from_secs_f64(scaled_exposure_duration_secs);
            self.camera.lock().await.set_exposure_duration(exp)?;
            restore_exposure.deactivate();
            return Ok(exp);
        }
        if star_goal_fraction < 0.5 || star_goal_fraction > 2.0 {
            warn!("Exposure time calibration diverged, goal fraction {}",
                  star_goal_fraction);
        }

        scaled_exposure_duration_secs /= star_goal_fraction;
        if scaled_exposure_duration_secs > max_exposure_duration.as_secs_f64() {
            self.camera.lock().await.set_exposure_duration(max_exposure_duration)?;
            restore_exposure.deactivate();
            return Ok(max_exposure_duration);
        }
        let exp = Duration::from_secs_f64(scaled_exposure_duration_secs);
        self.camera.lock().await.set_exposure_duration(exp)?;
        restore_exposure.deactivate();
        Ok(exp)
    }

    // Exposure duration is the result of calibrate_exposure_duration().
    // Result is (FOV (degrees), lens distortion, match_max_error,
    //            solve duration).
    // Errors:
    //   NotFound: no plate solution was found.
    //   DeadlineExceeded: the solve operation timed out.
    //   Aborted: the calibration was canceled.
    //   InvalidArgument: too few stars were found.
    pub async fn calibrate_optical(
        &self,
        solver: Arc<tokio::sync::Mutex<dyn SolverTrait + Send + Sync>>,
        detection_binning: u32, detection_sigma: f64,
        cancel_calibration: Arc<Mutex<bool>>)
        -> Result<(f64, f64, f64, Duration), CanonicalError> {
        // Goal: find the field of view, lens distortion, match_max_error solver
        // parameter, and representative plate solve time.
        //
        // Assumption: camera is focused and pointed at sky with stars.
        //
        // Approach:
        // * Grab an image, detect the stars.
        // * Do a plate solution with no FOV estimate and no distortion estimate.
        //   Use a generous match_max_error value and the default (generous)
        //   solve_timeout.
        // * Use the plate solution to obtain FOV and lens distortion, and determine
        //   an appropriate match_max_error value.
        // * Do another plate solution with the known FOV, lens distortion, and
        //   match_max_error to obtain a representative solution time.

        let (image, stars, _, _) = self.acquire_image_get_stars(
            /*frame_id=*/None, detection_binning, detection_sigma,
            cancel_calibration.clone()).await?;
        let (width, height) = image.dimensions();
        if *cancel_calibration.lock().unwrap() {
            return Err(aborted_error("Cancelled during calibrate_optical()."));
        }

        // Set up solve arguments.
        let solve_extension = SolveExtension::default();
        let mut solve_params = SolveParams{
            fov_estimate: None,  // Initially blind w.r.t. FOV.
            distortion: Some(0.0),
            match_max_error: Some(0.005),
            ..Default::default()
        };
        let mut star_centroids = Vec::<ImageCoord>::with_capacity(stars.len());
        for star in &stars {
            star_centroids.push(ImageCoord{x: star.centroid_x,
                                           y: star.centroid_y});
        }
        let plate_solution = solver.lock().await.solve_from_centroids(
            &star_centroids,
            width as usize, height as usize,
            &solve_extension, &solve_params).await?;

        if *cancel_calibration.lock().unwrap() {
            return Err(aborted_error("Cancelled during calibrate_optical()."));
        }

        let fov = plate_solution.fov;  // Degrees.
        let distortion = plate_solution.distortion.unwrap();

        // Use the 90th percentile error residual as a basis for determining the
        // 'match_max_error' argument to the solver.
        let p90_error_deg = plate_solution.p90_error / 3600.0;  // Degrees.
        let p90_err_frac = p90_error_deg / fov;  // As fraction of FOV.
        let match_max_error = p90_err_frac * 2.0;

        // Do another solve with now-known FOV, distortion, and
        // match_max_error, to get a more representative solve_duration.
        solve_params.fov_estimate = Some((fov, fov / 10.0));
        solve_params.distortion = Some(distortion);
        solve_params.match_max_error = Some(match_max_error);

        let plate_solution2 = match solver.lock().await.solve_from_centroids(
            &star_centroids,
            width as usize, height as usize,
            &solve_extension, &solve_params).await
        {
            Ok(ps) => ps,
            Err(e) => {
                return Err(internal_error(
                    &format!("Unexpected error during repeated plate solve: {:?}", e)));
            }
        };
        let solve_duration = std::time::Duration::try_from(
            plate_solution2.solve_time.unwrap()).unwrap();

        return Ok((fov, distortion, match_max_error, solve_duration));
    }

    async fn acquire_image_get_stars(
        &self, frame_id: Option<i32>,
        detection_binning: u32, detection_sigma: f64,
        cancel_calibration: Arc<Mutex<bool>>)
        -> Result<(Arc<GrayImage>, Vec<StarDescription>, i32, [u32; 256]),
                  CanonicalError>
    {
        let (captured_image, frame_id) =
            Self::capture_image(self.camera.clone(), frame_id).await?;
        if *cancel_calibration.lock().unwrap() {
            return Err(aborted_error(
                "Cancelled during calibrate_exposure_duration()."));
        }
        // Run CedarDetect on the image.
        let image = &captured_image.image;
        let noise_estimate = estimate_noise_from_image(&image);
        let (stars, _, _, histogram) =
            get_stars_from_image(&image, noise_estimate, detection_sigma,
                                 self.normalize_rows, detection_binning,
                                 /*detect_hot_pixels*/true,
                                 /*return_binned_image=*/false);
        Ok((image.clone(), stars, frame_id, histogram))
    }
}

// RAII gadget for saving/restoring camera exposure time.
struct RestoreExposure {
    camera: Arc<tokio::sync::Mutex<Box<dyn AbstractCamera + Send>>>,
    exp_duration: Duration,
    do_restore: bool,
}
impl RestoreExposure {
    async fn new(camera: Arc<tokio::sync::Mutex<Box<dyn AbstractCamera + Send>>>) -> Self {
        let locked_camera = camera.lock().await;
        RestoreExposure{
            camera: camera.clone(),
            exp_duration: locked_camera.get_exposure_duration(),
            do_restore: true,
        }
    }

    // Turn off restoration of the saved exposure time.
    fn deactivate(&mut self) {
        self.do_restore = false;
    }

    async fn restore(&mut self) {
        if self.do_restore {
            let mut locked_camera = self.camera.lock().await;
            locked_camera.set_exposure_duration(self.exp_duration).unwrap();
        }
    }
}
impl Drop for RestoreExposure {
    fn drop(&mut self) {
        // https://stackoverflow.com/questions/71541765/rust-async-drop
        futures::executor::block_on(self.restore());
    }
}
