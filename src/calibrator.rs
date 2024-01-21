use std::sync::{Arc, Mutex};
use std::time::Duration;

use imageproc::stats::histogram;
use log::{info, warn};

use camera_service::abstract_camera::{AbstractCamera, Gain, Offset};
use canonical_error::{CanonicalError, failed_precondition_error};
use cedar_detect::algorithm::{estimate_noise_from_image, get_stars_from_image};

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
        //     black crush (too many zero-value pixels).
        // Assumption: camera is pointed at sky which is mostly dark. Camera
        //     ROI is full sensor, no binning.
        // Approach:
        // * Use camera's self-reported optimal gain.
        // * Use 1ms exposures.
        // * Starting at offset=0, as long as >1% of pixels have zero
        //   value, increase the offset.

        let _restore_settings = RestoreSettings::new(self.camera.clone());
        let mut locked_camera = self.camera.lock().unwrap();

        let optimal_gain = locked_camera.optimal_gain();
        locked_camera.set_gain(optimal_gain)?;
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
        //     number of detected stars.
        // Assumption: camera is focused and pointed at sky with stars. ROI is
        //     full sensor, no binning. Camera offset is set properly.
        // Approach:
        // * Use camera's self-reported optimal gain.
        // * With the `setup_exposure_duration`
        //   * Grab an image.
        //   * Detect the stars.
        //   * If close enough to the goal, scale the exposure duration and
        //     return it.
        //   * If not close to the goal, scale the exposure duration and
        //     do one more exposure/detect/scale.

        let _restore_settings = RestoreSettings::new(self.camera.clone());
        let mut locked_camera = self.camera.lock().unwrap();

        let optimal_gain = locked_camera.optimal_gain();
        locked_camera.set_gain(optimal_gain)?;
        locked_camera.set_exposure_duration(setup_exposure_duration)?;
        let (mut captured_image, frame_id) =
            locked_camera.capture_image(/*prev_frame_id=*/None)?;
        let frame_id = Some(frame_id);

        // Run CedarDetect on the image.
        let mut image = &captured_image.image;
        let mut noise_estimate = estimate_noise_from_image(&image);
        let (mut stars, _, _) =
            get_stars_from_image(&image, noise_estimate,
                                 detection_sigma, detection_max_size as u32,
                                 /*use_binned_image=*/true,
                                 /*return_binned_image=*/false);
        let mut num_stars_detected = stars.len();
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
        locked_camera.set_exposure_duration(
            Duration::from_secs_f32(scaled_exposure_duration_secs))?;
        (captured_image, _) = locked_camera.capture_image(frame_id)?;

        image = &captured_image.image;
        noise_estimate = estimate_noise_from_image(&image);
        (stars, _, _) =
            get_stars_from_image(&image, noise_estimate,
                                 detection_sigma, detection_max_size as u32,
                                 /*use_binned_image=*/true,
                                 /*return_binned_image=*/false);
        num_stars_detected = stars.len();
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
