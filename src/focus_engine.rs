use camera_service::abstract_camera::AbstractCamera;

use image::GrayImage;
use imageproc::rect::Rect;

struct FocusEngine<'a> {
    // Note: camera settings can be adjusted behind our back.
    camera: &'a mut dyn AbstractCamera,

    update_interval: f32,  // -1 means go fast as possible.

    // TODO: update interval

    // optional exposure time; if None use auto exposure
    // TODO: current exposure duration;

    // TODO: worker thread.
}

struct FocusResult {
    image: GrayImage,

    exposure_time_ms: i32,

    center_region: Rect,

    peak_position: (u32, u32),

    zoomed_peak_image: GrayImage,

    center_peak_fwhm: f32,

    // TODO: capture time, camera temperature.

    // TODO: candidates, hot pixel count, etc. from StarGate (which we run
    // alongside the focusing logic)
}
