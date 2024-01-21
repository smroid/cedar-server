use std::sync::{Arc, Mutex};
use std::time::Duration;

use camera_service::abstract_camera::{AbstractCamera, Gain, Offset};
use canonical_error::{CanonicalError, failed_precondition_error};
use imageproc::stats::histogram;

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
        let _restore_settings = RestoreSettings::new(self.camera.clone());
        // Goal: find the minimum camera offset setting that avoids
        //     black crush (too many zero-value pixels).
        // Assumption: camera is pointed at sky which is mostly dark. Camera
        //     ROI is full sensor, no binning.
        // Approach:
        // * Use camera's self-reported optimal gain.
        // * Use 1ms exposures.
        // * Starting at offset=0, as long as >1% of pixels have zero
        //   value, increase the offset.
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
}

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
