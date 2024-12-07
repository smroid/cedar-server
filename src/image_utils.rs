// Copyright (c) 2024 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use fast_image_resize::images::Image as FastImage;
use fast_image_resize::{FilterType, Resizer, ResizeOptions,
                        ResizeAlg::Interpolation as FastInterp};

use image::{GrayImage, ImageBuffer, Luma};
use image::imageops;
use imageproc::geometric_transformations::{Interpolation, rotate_about_center};

fn compute_lut(min_pixel_value: u8,
               peak_pixel_value: u8,
               gamma: f32) -> [u8; 256] {
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
        lut[n as usize] = (scale * ((n - min_pixel_value) as f32).powf(gamma)) as u8;
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

// `angle` degrees, positive is counter-clockwise. Must be on [-180..180].
// The returned image has the same dimensions as the argument; the input image
// is shrunk as needed such that the rotated image fits within the original
// dimensions.
// The `fill` value is used to fill in pixels outside of the shrunk/rotated
// image.
pub fn rotate_image(image: &GrayImage, angle: f64, fill: u8) -> GrayImage {
    assert!(angle >= -180.0);
    assert!(angle <= 180.0);

    let (w, h) = image.dimensions();

    // Take the origin to be at the center of the image rectangle, and
    // express the coordinates of each rectangle vertex.
    let p1 = ( 0.5 * w as f64,  0.5 * h as f64);
    let p2 = (-0.5 * w as f64,  0.5 * h as f64);
    let p3 = (-0.5 * w as f64, -0.5 * h as f64);
    let p4 = ( 0.5 * w as f64, -0.5 * h as f64);

    // Find the rotated rectangle's vertices.
    let p1_rot = rotate_vector(p1.0, p1.1, angle);
    let p2_rot = rotate_vector(p2.0, p2.1, angle);
    let p3_rot = rotate_vector(p3.0, p3.1, angle);
    let p4_rot = rotate_vector(p4.0, p4.1, angle);

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

    // Pad the image before shrinking.
    let padded_w = w as f64 * ratio;
    let padded_h = h as f64 * ratio;
    let mut new_img = ImageBuffer::from_pixel(padded_w as u32, padded_h as u32,
                                              Luma::<u8>([fill]));
    let border_w = (padded_w - w as f64) / 2.0;
    let border_h = (padded_h - h as f64) / 2.0;
    let x_offset = border_w as i64;
    let y_offset = border_h as i64;
    imageops::replace(&mut new_img, image, x_offset, y_offset);

    // Shrink the padded image before rotating.

    // Convert GrayImage to FastImage for fast_image_resize.
    let src_img = FastImage::from_vec_u8(padded_w as u32, padded_h as u32,
                                         new_img.into_raw(),
                                         fast_image_resize::PixelType::U8).unwrap();
    // Resize the image.
    let mut resizer = Resizer::new();
    let mut dst_img = FastImage::new(w, h, src_img.pixel_type());
    resizer.resize(
        &src_img, &mut dst_img,
        &ResizeOptions::new().resize_alg(FastInterp(
            // Almost as fast as Box, with higher visual quality.
            FilterType::Bilinear))).unwrap();

    let resized_img = GrayImage::from_raw(w, h, dst_img.into_vec()).unwrap();
    rotate_about_center(&resized_img,
                        -1.0 * angle.to_radians() as f32,
                        // Almost as fast as Nearest, with much higher visual quality.
                        Interpolation::Bilinear,
                        Luma::<u8>([fill]))
}

fn rotate_vector(x: f64, y: f64, angle: f64) -> (f64, f64) {
    let angle_rad = angle.to_radians();
    let x_new = x * angle_rad.cos() - y * angle_rad.sin();
    let y_new = x * angle_rad.sin() + y * angle_rad.cos();
    (x_new, y_new)
}
