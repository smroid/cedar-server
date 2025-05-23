// Copyright (c) 2024 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use fast_image_resize::images::Image as FastImage;
use fast_image_resize::{FilterType, Resizer, ResizeOptions,
                        ResizeAlg::Interpolation as FastInterp};

use image::{GrayImage, ImageBuffer, Luma};
use image::imageops;
use imageproc::geometric_transformations::{Interpolation, rotate_about_center};

fn compute_lut(min_pixel_value: u8,
               mut peak_pixel_value: u8,
               gamma: f32) -> [u8; 256] {
    if peak_pixel_value < min_pixel_value {
        peak_pixel_value = min_pixel_value;
    }
    let mut lut: [u8; 256] = [0; 256];
    let scale = 256.0 / ((peak_pixel_value - min_pixel_value) as f32).powf(gamma);
    for n in 0..=255 {
        if n < min_pixel_value {
            lut[n as usize] = 0;
            continue;
        }
        if n >= peak_pixel_value {
            lut[n as usize] = 255;
            continue;
        }
        let mut scaled = scale * ((n - min_pixel_value) as f32).powf(gamma);
        if scaled < 0.0 {
            scaled = 0.0;
        } else if scaled > 255.0 {
            scaled = 255.0;
        }
        lut[n as usize] = scaled as u8;
    }
    lut
}

// Copy the image, mapping min_pixel_value..peak_pixel_value to 0..255 by
// applying a gamma and scale factor.
pub fn scale_image(
    image: &GrayImage, min_pixel_value: u8, peak_pixel_value: u8, gamma: f32)
    -> GrayImage {
    let lut = compute_lut(min_pixel_value, peak_pixel_value, gamma);

    // Apply the lut.
    let out_vec: Vec<u8> = image.as_raw().iter().map(|x| lut[*x as usize]).collect();

    let (width, height) = image.dimensions();
    GrayImage::from_raw(width, height, out_vec).unwrap()
}

// In-place variant of scale_image().
pub fn scale_image_mut(
    image: &mut GrayImage, min_pixel_value: u8, peak_pixel_value: u8, gamma: f32) {
    let lut = compute_lut(min_pixel_value, peak_pixel_value, gamma);

    for pixel in image.pixels_mut() {
        pixel[0] = lut[pixel[0] as usize];
    }
}

// Some cameras have a problem where some rows have a noise-induced level
// offset. This function heuristically normalizes each row to have similar
// black level.
pub fn normalize_rows_mut(image: &mut GrayImage) {
    for y in 0..image.height() {
        let mut min_value = 255_u8;
        for x in 0..image.width() {
            let value = image.get_pixel(x as u32, y as u32).0[0];
            if value < min_value {
                min_value = value;
            }
        }
        for x in 0..image.width() {
            let value = image.get_pixel_mut(x as u32, y as u32);
            value[0] -= min_value;
        }
    }
}

// Tool for rotating an image and performing related coordinate transforms.
#[derive(Clone)]
pub struct ImageRotator {
    angle_rad: f64,
    sin_term: f64,
    cos_term: f64,
    size_ratio: f64,
}
impl ImageRotator {
    // `angle` degrees, positive is counter-clockwise.
    // The supplied `width` and `height` must match the values passed to
    // rotate_image() and transform_xxx(), allowing for scaling up and down (but
    // not changing the aspect ratio).
    pub fn new(width: u32, height: u32, angle: f64) -> Self {
        let angle_rad = angle.to_radians();
        let sin_term = angle_rad.sin();
        let cos_term = angle_rad.cos();

        // Take the origin to be at the center of the image rectangle, and
        // express the coordinates of each rectangle vertex.
        let w = width;
        let h = height;
        let p1 = ( 0.5 * w as f64,  0.5 * h as f64);
        let p2 = (-0.5 * w as f64,  0.5 * h as f64);
        let p3 = (-0.5 * w as f64, -0.5 * h as f64);
        let p4 = ( 0.5 * w as f64, -0.5 * h as f64);

        // Find the rotated rectangle's vertices.
        let p1_rot = Self::rotate_vector(p1.0, p1.1, sin_term, cos_term);
        let p2_rot = Self::rotate_vector(p2.0, p2.1, sin_term, cos_term);
        let p3_rot = Self::rotate_vector(p3.0, p3.1, sin_term, cos_term);
        let p4_rot = Self::rotate_vector(p4.0, p4.1, sin_term, cos_term);

        // Compute the horizontal and vertical extent of the rotated rectangle.
        let mut x_min = 0.0_f64;
        let mut x_max = 0.0_f64;
        let mut y_min = 0.0_f64;
        let mut y_max = 0.0_f64;
        for p in [p1_rot, p2_rot, p3_rot, p4_rot] {
            let (x, y) = p;
            x_min = x_min.min(x);
            x_max = x_max.max(x);
            y_min = y_min.min(y);
            y_max = y_max.max(y);
        }
        let w_rot = x_max - x_min;
        let h_rot = y_max - y_min;

        // One or both of the rotated width or height will be larger than the
        // original width/height. Find out how much we need to scale down the
        // rotated rectangle to fit within the original dimensions.
        let w_ratio = w_rot / w as f64;
        let h_ratio = h_rot / h as f64;
        let ratio = w_ratio.max(h_ratio);
        assert!(ratio >= 1.0);

        // Note that we don't store the width and height. The caller passes the
        // run-time width/height into the rotate_image() and transform_xxx
        // methods, because the run-time coordinate transform requests might be
        // for a different scaling of the image size.
        ImageRotator{angle_rad, sin_term, cos_term, size_ratio: ratio}
    }

    pub fn angle(&self) -> f64 {
        self.angle_rad.to_degrees()
    }

    // Returns >= 1.0, factor by which image was shrunk when rotating.
    pub fn size_ratio(&self) -> f64 {
        self.size_ratio
    }

    // The returned image has the same dimensions as the argument; the input image
    // is shrunk as needed such that the rotated image fits within the original
    // dimensions.
    // The `fill` value is used to fill in pixels outside of the shrunk/rotated
    // image.
    pub fn rotate_image(&self, image: &GrayImage, fill: u8) -> GrayImage
    {
        let (w, h) = image.dimensions();
        let ratio = self.size_ratio;

        // Pad the image before rotating and shrinking.
        let padded_w = w as f64 * ratio + 0.5;
        let padded_h = h as f64 * ratio + 0.5;
        let mut new_img = ImageBuffer::from_pixel(padded_w as u32, padded_h as u32,
                                                  Luma::<u8>([fill]));
        let border_w = (padded_w - w as f64) / 2.0;
        let border_h = (padded_h - h as f64) / 2.0;
        let x_offset = border_w as i64;
        let y_offset = border_h as i64;
        imageops::replace(&mut new_img, image, x_offset, y_offset);

        let rotated_image = rotate_about_center(
            &new_img,
            -1.0 * self.angle_rad as f32,
            // Almost as fast as Nearest, with much higher visual quality.
            Interpolation::Bilinear,
            Luma::<u8>([fill]));

        // Convert GrayImage to FastImage for fast_image_resize.
        let src_img = FastImage::from_vec_u8(padded_w as u32, padded_h as u32,
                                             rotated_image.into_raw(),
                                             fast_image_resize::PixelType::U8).unwrap();
        // Shrink the image.
        let mut resizer = Resizer::new();
        let mut dst_img = FastImage::new(w, h, src_img.pixel_type());
        resizer.resize(
            &src_img, &mut dst_img,
            &ResizeOptions::new().resize_alg(FastInterp(
                // Almost as fast as Box, with higher visual quality.
                FilterType::Hamming))).unwrap();

        let resized_img = GrayImage::from_raw(w, h, dst_img.into_vec()).unwrap();
        resized_img
    }

    // Given (x, y), the image coordinates in the original image, returns the
    // coordinates of the downscaled/rotated image within the output image.
    pub fn transform_to_rotated(&self, x: f64, y: f64,
                                width: u32, height: u32) -> (f64, f64) {
        // The x, y origin is upper-left corner. Change to center-based
        // coordinates.
        let x_cen = x - (width as f64 / 2.0);
        let y_cen = (height as f64 / 2.0) - y;

        // Rotate according to the transform.
        let (x_cen_rot, y_cen_rot) =
            Self::rotate_vector(x_cen, y_cen, self.sin_term, self.cos_term);

        // Scale down.
        let x_cen_rot_scaled = x_cen_rot / self.size_ratio;
        let y_cen_rot_scaled = y_cen_rot / self.size_ratio;

        // Move back to corner-based origin.
        (x_cen_rot_scaled + (width as f64 / 2.0),
         (height as f64 / 2.0) - y_cen_rot_scaled)
    }

    // Given (x, y), the image coordinates in the output image after
    // downscaling/rotating, returns the coordinates within the original
    // image (prior to downscaling/rotating).
    pub fn transform_from_rotated(&self, x: f64, y: f64,
                                  width: u32, height: u32) -> (f64, f64) {
        // The x, y origin is upper-left corner. Change to center-based
        // coordinates.
        let x_cen = x - (width as f64 / 2.0);
        let y_cen = (height as f64 / 2.0) - y;

        // De-rotate according to the transform.
        let (x_cen_rot, y_cen_rot) =
            Self::rotate_vector(x_cen, y_cen, -1.0 * self.sin_term, self.cos_term);

        // Scale up.
        let x_cen_rot_scaled = x_cen_rot * self.size_ratio;
        let y_cen_rot_scaled = y_cen_rot * self.size_ratio;

        // Move back to corner-based origin.
        (x_cen_rot_scaled + (width as f64 / 2.0),
         (height as f64 / 2.0) - y_cen_rot_scaled)
    }

    fn rotate_vector(x: f64, y: f64, sin_term: f64, cos_term: f64) -> (f64, f64) {
        (x * cos_term - y * sin_term,
         x * sin_term + y * cos_term)
    }
}
