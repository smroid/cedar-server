// Copyright (c) 2024 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use std::ops::Deref;
use std::sync::Arc;
use std::time::Duration;

use image::GrayImage;
use imageproc::stats::histogram;
use log::{debug, warn};

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

#[derive(Debug)]
pub enum ExposureCalibrationError {
    // See CalibrationFailureReason in cedar.proto.
    TooFewStars,
    BrightSky,
    // Cancel signaled.
    Aborted,
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
        &self, cancel_calibration: Arc<tokio::sync::Mutex<bool>>)
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
        if *cancel_calibration.lock().await {
            return Err(aborted_error("Cancelled during calibrate_offset()."));
        }

        // Set offset before changing exposure; if we can't set offset this
        // lets us avoid changing the exposure only to have to restore it.
        self.camera.lock().await.set_offset(Offset::new(0))?;

        // Restore the exposure duration that we change here.
        let mut restore_exposure = RestoreExposure::new(self.camera.clone()).await;
        self.camera.lock().await.set_exposure_duration(Duration::from_millis(1))?;
        let (width, height) = self.camera.lock().await.dimensions();
        let total_pixels = width * height;

        let max_offset = 20;
        let mut prev_frame_id: Option<i32> = None;
        let mut num_zero_pixels = 0;
        for mut offset in 0..=max_offset {
            if *cancel_calibration.lock().await {
                restore_exposure.restore().await;
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
                restore_exposure.restore().await;
                return Ok(Offset::new(offset));
            }
        }
        restore_exposure.restore().await;
        Err(failed_precondition_error(format!("Still have {} zero pixels at offset={}",
                                              num_zero_pixels, max_offset).as_str()))
    }

    async fn capture_image(
        camera: Arc<tokio::sync::Mutex<Box<dyn AbstractCamera + Send>>>,
        mut frame_id: Option<i32>) -> Result<(CapturedImage, i32), CanonicalError>
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
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            }
            let (image, id) = capture.unwrap();
            frame_id = Some(id);
            if !image.params_accurate {
                // Wait until image data is accurate w.r.t. the current camera
                // settings.
                continue;
            }
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
        cancel_calibration: Arc<tokio::sync::Mutex<bool>>)
        -> Result<Duration, ExposureCalibrationError> {
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
        if *cancel_calibration.lock().await {
            return Err(ExposureCalibrationError::Aborted);
        }

        // If we fail, restore the exposure duration that we change here.
        let mut restore_exposure = RestoreExposure::new(self.camera.clone()).await;

        self.camera.lock().await.set_exposure_duration(
            initial_exposure_duration).unwrap();
        let (_, mut exp_duration, mut stars, mut frame_id, mut histogram) =
            self.acquire_image_get_stars(
                /*frame_id=*/None, detection_binning, detection_sigma).await.unwrap();
        let mut stats = stats_for_histogram(&histogram);

        // >1 if we have more stars than goal; <1 if fewer stars than goal.
        let mut star_goal_fraction =
            f64::max(stars.len() as f64, 1.0) / star_count_goal as f64;
        // See the rationale in DetectEngine::worker() for how we relate
        // star_goal_fraction to exposure time adjustment.
        debug!("1: exp {:?}, {} stars, pix mean {:.2} ",
               exp_duration, stars.len(), stats.mean);

        let mut scaled_exp_duration =
            Duration::from_secs_f64(exp_duration.as_secs_f64() / star_goal_fraction);
        if scaled_exp_duration > max_exposure_duration {
            scaled_exp_duration = max_exposure_duration;
        }
        if star_goal_fraction > 0.8 && star_goal_fraction < 1.2 {
            // Close enough to goal, the scaled exposure time is good.
            self.camera.lock().await.set_exposure_duration(scaled_exp_duration).unwrap();
            return Ok(scaled_exp_duration);
        }

        const BRIGHTNESS_LIMIT: f64 = 192.0;
        if star_goal_fraction < 1.0 && stats.mean > BRIGHTNESS_LIMIT {
            // We are increasing exposure if necessary to increase star count.
            // Don't exceed a brightness limit.
            restore_exposure.restore().await;
            return Err(ExposureCalibrationError::BrightSky);
        }

        // Iterate with the refined exposure duration.
        if *cancel_calibration.lock().await {
            restore_exposure.restore().await;
            return Err(ExposureCalibrationError::Aborted);
        }
        self.camera.lock().await.set_exposure_duration(scaled_exp_duration).unwrap();
        (_, exp_duration, stars, frame_id, histogram) =
            self.acquire_image_get_stars(
                Some(frame_id), detection_binning, detection_sigma).await.unwrap();
        stats = stats_for_histogram(&histogram);
        star_goal_fraction =
            f64::max(stars.len() as f64, 1.0) / star_count_goal as f64;
        debug!("2: exp {:?}, {} stars, pix mean {:.2} ",
               exp_duration, stars.len(), stats.mean);

        scaled_exp_duration = Duration::from_secs_f64(
            exp_duration.as_secs_f64() / star_goal_fraction);
        if scaled_exp_duration > max_exposure_duration {
            scaled_exp_duration = max_exposure_duration;
        }
        if star_goal_fraction > 0.8 && star_goal_fraction < 1.2 {
            // Close enough to goal, the scaled exposure time is good.
            self.camera.lock().await.set_exposure_duration(scaled_exp_duration).unwrap();
            return Ok(scaled_exp_duration);
        }
        if star_goal_fraction < 1.0 {
            // We are increasing exposure as necessary to increase star count.
            // Don't exceed a brightness limit or maximum exposure time.
            if stats.mean > BRIGHTNESS_LIMIT {
                restore_exposure.restore().await;
                return Err(ExposureCalibrationError::BrightSky);
            }
            if exp_duration >= max_exposure_duration {
                restore_exposure.restore().await;
                return Err(ExposureCalibrationError::TooFewStars);
            }
        }

        // Iterate one more time.
        if *cancel_calibration.lock().await {
            restore_exposure.restore().await;
            return Err(ExposureCalibrationError::Aborted);
        }
        self.camera.lock().await.set_exposure_duration(scaled_exp_duration).unwrap();
        (_, exp_duration, stars, _, histogram) =
            self.acquire_image_get_stars(
                Some(frame_id), detection_binning, detection_sigma).await.unwrap();
        stats = stats_for_histogram(&histogram);
        star_goal_fraction =
            f64::max(stars.len() as f64, 1.0) / star_count_goal as f64;
        debug!("3: exp {:?}, {} stars, pix mean {:.2} ",
               exp_duration, stars.len(), stats.mean);

        scaled_exp_duration = Duration::from_secs_f64(
            exp_duration.as_secs_f64() / star_goal_fraction);
        if scaled_exp_duration > max_exposure_duration {
            scaled_exp_duration = max_exposure_duration;
        }
        if star_goal_fraction > 0.8 && star_goal_fraction < 1.2 {
            // Close enough to goal, the scaled exposure time is good.
            self.camera.lock().await.set_exposure_duration(scaled_exp_duration).unwrap();
            return Ok(scaled_exp_duration);
        }
        if star_goal_fraction < 1.0 {
            // Increased exposure was insufficent to increase star count, we'd
            // need to go even longer. Don't exceed a brightness limit or
            // maximum exposure time.
            if stats.mean > BRIGHTNESS_LIMIT {
                restore_exposure.restore().await;
                return Err(ExposureCalibrationError::BrightSky);
            }
            if exp_duration >= max_exposure_duration {
                restore_exposure.restore().await;
                return Err(ExposureCalibrationError::TooFewStars);
            }
        }
        if star_goal_fraction < 0.5 || star_goal_fraction > 2.0 {
            warn!("Exposure time calibration diverged, goal fraction {}",
                  star_goal_fraction);
        }

        self.camera.lock().await.set_exposure_duration(scaled_exp_duration).unwrap();
        Ok(scaled_exp_duration)
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
        cancel_calibration: Arc<tokio::sync::Mutex<bool>>)
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

        let (image, _, stars, _, _) = self.acquire_image_get_stars(
            /*frame_id=*/None, detection_binning, detection_sigma).await?;
        let (width, height) = image.dimensions();
        if *cancel_calibration.lock().await {
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

        if *cancel_calibration.lock().await {
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

        Ok((fov, distortion, match_max_error, solve_duration))
    }

    // Returns: acquired image, actual exposure duration, detected stars,
    // frame_id, histogram.
    async fn acquire_image_get_stars(
        &self, frame_id: Option<i32>,
        detection_binning: u32, detection_sigma: f64)
        -> Result<(Arc<GrayImage>, Duration, Vec<StarDescription>, i32, [u32; 256]),
                  CanonicalError>
    {
        let (captured_image, frame_id) =
            Self::capture_image(self.camera.clone(), frame_id).await?;
        // Run CedarDetect on the image.
        let image = &captured_image.image;
        let noise_estimate = estimate_noise_from_image(image);
        let (stars, _, _, histogram) =
            get_stars_from_image(image, noise_estimate, detection_sigma,
                                 self.normalize_rows, detection_binning,
                                 /*detect_hot_pixels*/true,
                                 /*return_binned_image=*/false);
        Ok((image.clone(), captured_image.capture_params.exposure_duration,
            stars, frame_id, histogram))
    }
}

// Convenience for saving/restoring camera exposure time.
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

    async fn restore(&mut self) {
        if self.do_restore {
            let mut locked_camera = self.camera.lock().await;
            locked_camera.set_exposure_duration(self.exp_duration).unwrap();
        }
    }
}
