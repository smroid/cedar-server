use std::sync::{Arc, Mutex};
use std::time::Duration;

use image::GrayImage;
use imageproc::stats::histogram;
use log::warn;

use camera_service::abstract_camera::{AbstractCamera, Gain, Offset};
use canonical_error::{CanonicalError, failed_precondition_error};
use cedar_detect::algorithm::{StarDescription,
                              estimate_noise_from_image, get_stars_from_image};
use crate::solve_engine::SolveEngine;
use crate::tetra3_server::{ImageCoord, SolveRequest};

pub struct Calibrator {
    camera: Arc<Mutex<dyn AbstractCamera>>,
}

// By convention, all methods restore any camera settings that they
// alter.
impl Calibrator {
    pub fn new(camera: Arc<Mutex<dyn AbstractCamera>>) -> Self{
        Calibrator{camera}
    }

    pub fn calibrate_offset(&self) -> Result<Offset, CanonicalError> {
        // Goal: find the minimum camera offset setting that avoids
        // black crush (too many zero-value pixels).
        //
        // Assumption: camera is pointed at sky which is mostly dark.
        //
        // Approach:
        // * Use 1ms exposures.
        // * Starting at offset=0, as long as >1% of pixels have zero
        //   value, increase the offset.

        let _restore_settings = RestoreSettings::new(self.camera.clone());
        let mut locked_camera = self.camera.lock().unwrap();

        locked_camera.set_exposure_duration(Duration::from_millis(1))?;
        let (width, height) = locked_camera.dimensions();
        let total_pixels = width * height;

        let max_offset = 20;
        let mut prev_frame_id: Option<i32> = None;
        let mut num_zero_pixels = 0;
        for mut offset in 0..=max_offset {
            locked_camera.set_offset(Offset::new(offset))?;
            let (captured_image, frame_id) = locked_camera.capture_image(prev_frame_id)?;
            prev_frame_id = Some(frame_id);
            let channel_histogram = histogram(&captured_image.image);
            let histo = channel_histogram.channels[0];
            num_zero_pixels = histo[0];
            if num_zero_pixels < (total_pixels / 100) as u32 {
                if offset < max_offset {
                    offset += 1;  // One more for good measure.
                }
                return Ok(Offset::new(offset));
            }
        }
        Err(failed_precondition_error(format!("Still have {} zero pixels at offset={}",
                                              num_zero_pixels, max_offset).as_str()))
    }

    pub fn calibrate_exposure_duration(
        &self, setup_exposure_duration: Duration, star_count_goal: i32,
        detection_sigma: f32, detection_max_size: i32)
        -> Result<Duration, CanonicalError> {
        // Goal: find the camera exposure duration that yields the desired
        // number of detected stars.
        //
        // Assumption: camera is focused and pointed at sky with stars. The
        // passed `setup_exposure_duration` yields a large number of detected
        // stars (i.e. at least a good fraction of `star_count_goal`).
        //
        // Approach:
        // * Using the `setup_exposure_duration`
        //   * Grab an image.
        //   * Detect the stars.
        //   * If close enough to the goal, scale the exposure duration and
        //     return it.
        //   * If not close to the goal, scale the exposure duration and
        //     do one more exposure/detect/scale.

        let _restore_settings = RestoreSettings::new(self.camera.clone());

        self.camera.lock().unwrap().set_exposure_duration(setup_exposure_duration)?;
        let (_, mut stars, frame_id) = self.acquire_image_get_stars(
            /*frame_id=*/None, detection_sigma, detection_max_size)?;

        let mut num_stars_detected = stars.len();
        if num_stars_detected < (star_count_goal / 5) as usize {
            return Err(failed_precondition_error(
                format!("Too few stars detected ({})", num_stars_detected).as_str()))
        }
        // >1 if we have more stars than goal; <1 if fewer stars than goal.
        let mut star_goal_fraction =
            f32::max(num_stars_detected as f32, 1.0) / star_count_goal as f32;
        let mut scaled_exposure_duration_secs =
            setup_exposure_duration.as_secs_f32() / star_goal_fraction;
        if star_goal_fraction > 0.8 && star_goal_fraction < 1.2 {
            // Close enough to goal, the scaled exposure time is good.
            return Ok(Duration::from_secs_f32(scaled_exposure_duration_secs));
        }

        // Iterate with the refined exposure duration.
        self.camera.lock().unwrap().set_exposure_duration(
            Duration::from_secs_f32(scaled_exposure_duration_secs))?;
        (_, stars, _) = self.acquire_image_get_stars(
            Some(frame_id), detection_sigma, detection_max_size)?;

        num_stars_detected = stars.len();
        if num_stars_detected < (star_count_goal / 5) as usize {
            return Err(failed_precondition_error(
                format!("Too few stars detected ({})", num_stars_detected).as_str()))
        }
        // >1 if we have more stars than goal; <1 if fewer stars than goal.
        star_goal_fraction =
            f32::max(num_stars_detected as f32, 1.0) / star_count_goal as f32;
        scaled_exposure_duration_secs =
            setup_exposure_duration.as_secs_f32() / star_goal_fraction;
        if star_goal_fraction < 0.7 || star_goal_fraction > 1.3 {
            warn!("Exposure time calibration diverged, goal fraction {}",
                  star_goal_fraction);
        }
        Ok(Duration::from_secs_f32(scaled_exposure_duration_secs))
    }

    // TODO: calibrate_gain()

    // Result is FOV (degrees), lens distortion, solve duration.
    pub fn calibrate_optical(&self, solve_engine: Arc<Mutex<SolveEngine>>,
                             exposure_duration: Duration,
                             detection_sigma: f32, detection_max_size: i32)
                             -> Result<(f32, f32, Duration), CanonicalError> {
        // Goal: find the field of view, lens distortion, and representative
        // plate solve time.
        //
        // Assumption: camera is focused and pointed at sky with stars.
        //
        // Approach:
        // * Grab an image, detect the stars.
        // * Do a plate solution with no FOV estimate and distortion estimate.
        //   Use a generous match_max_error value and a very generous
        //   solve_timeout.

        let _restore_settings = RestoreSettings::new(self.camera.clone());

        self.camera.lock().unwrap().set_exposure_duration(exposure_duration)?;
        let (image, stars, _) = self.acquire_image_get_stars(
            /*frame_id=*/None, detection_sigma, detection_max_size)?;
        let (width, height) = image.dimensions();

        let num_stars_detected = stars.len();
        if num_stars_detected < 4 {
            return Err(failed_precondition_error(
                format!("Too few stars detected ({})", num_stars_detected).as_str()))
        }

        // Set up SolveRequest.
        let mut solve_request = SolveRequest::default();
        solve_request.fov_estimate = None;
        solve_request.fov_max_error = None;
        solve_request.solve_timeout = Some(prost_types::Duration {
            seconds: 5, nanos: 0,
        });
        solve_request.distortion = Some(0.0);
        solve_request.return_matches = false;
        solve_request.match_max_error = Some(0.005);

        for star in &stars {
            solve_request.star_centroids.push(ImageCoord{x: star.centroid_x,
                                                         y: star.centroid_y});
        }
        solve_request.image_width = width as i32;
        solve_request.image_height = height as i32;

        let solve_result_proto = solve_engine.lock().unwrap().solve(solve_request)?;
        log::info!("solve result {:?}", solve_result_proto);  // TEMPORARY
        let solve_duration = std::time::Duration::try_from(
            solve_result_proto.solve_time.unwrap()).unwrap();
        if solve_result_proto.image_center_coords.is_some() {
            return Ok((solve_result_proto.fov.unwrap(),
                       solve_result_proto.distortion.unwrap(),
                       solve_duration));
        } else {
            return Err(failed_precondition_error(
                format!("No plate solution; elapsed time {:?}",
                        solve_duration).as_str()));
        }
    }

    // TODO: calibrate detection_sigma, detection_max_size? How...

    fn acquire_image_get_stars(&self, frame_id: Option<i32>,
                               detection_sigma: f32, detection_max_size: i32)
                               -> Result<(Arc<GrayImage>,
                                          Vec<StarDescription>,
                                          i32), CanonicalError> {
        let (captured_image, frame_id) =
            self.camera.lock().unwrap().capture_image(frame_id)?;
        // Run CedarDetect on the image.
        let image = &captured_image.image;
        let noise_estimate = estimate_noise_from_image(&image);
        let (stars, _, _) =
            get_stars_from_image(&image, noise_estimate,
                                 detection_sigma, detection_max_size as u32,
                                 /*use_binned_image=*/true,
                                 /*return_binned_image=*/false);
        Ok((image.clone(), stars, frame_id))
    }
}

// RAII gadget for saving/restoring camera settings.
struct RestoreSettings {
    camera: Arc<Mutex<dyn AbstractCamera>>,
    gain: Gain,
    offset: Offset,
    exp_duration: Duration,
}
impl RestoreSettings {
    fn new(camera: Arc<Mutex<dyn AbstractCamera>>) -> Self {
        let locked_camera = camera.lock().unwrap();
        RestoreSettings{
            camera: camera.clone(),
            gain: locked_camera.get_gain(),
            offset: locked_camera.get_offset(),
            exp_duration: locked_camera.get_exposure_duration(),
        }
    }
}
impl Drop for RestoreSettings {
    fn drop(&mut self) {
        let mut locked_camera = self.camera.lock().unwrap();
        locked_camera.set_gain(self.gain).unwrap();
        locked_camera.set_offset(self.offset).unwrap();
        locked_camera.set_exposure_duration(self.exp_duration).unwrap();
    }
}
