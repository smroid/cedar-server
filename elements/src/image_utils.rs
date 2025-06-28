// Copyright (c) 2024 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use image::{GrayImage, Luma};
use image::imageops;
use imageproc::geometric_transformations::{Interpolation, rotate_about_center};

use crate::cedar::Rectangle;

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
}
// This image rotator rotates the image and takes a central square crop of the
// result. The square crop is the full height of the original image, so the
// rotation of the original image can cause two corners of the square crop to be
// missing data. The maximum loss is less than 9% by area.
impl ImageRotator {
    // `angle` degrees, positive is counter-clockwise. A rotated central square
    //     of the original image is rotated by this amount to form an upright
    //     output square image.
    pub fn new(angle: f64) -> Self {
        let angle_rad = angle.to_radians();
        let sin_term = angle_rad.sin();
        let cos_term = angle_rad.cos();

        ImageRotator{angle_rad, sin_term, cos_term}
    }

    pub fn angle(&self) -> f64 {
        self.angle_rad.to_degrees()
    }

    // The supplied image is rotated counter-clockwise by this.angle_rad and a
    // central square crop of the rotated image is returned. This has the effect
    // of taking a clockwise-rotated central crop of the original image and
    // rotating it counter-clockwise to become upright as the output image.
    pub fn rotate_image_and_crop(&self, image: &GrayImage) -> GrayImage {
        let (w, h) = image.dimensions();
        let square_size = h;

        let rotated_image = rotate_about_center(
            &image,
            -1.0 * self.angle_rad as f32,
            // Almost as fast as Nearest, with much higher visual quality.
            Interpolation::Bilinear,
            Luma::<u8>([0]));

        // Take central crop of rotated image.
        let center_x = w / 2;
        imageops::crop_imm(&rotated_image,
                           center_x - square_size / 2, 0,
                           square_size, square_size).to_image()
    }

    pub fn get_cropped_region(&self, width: u32, height: u32) -> Rectangle {
        let square_size = height as i32;
        let center_x = (width / 2) as i32;

        Rectangle{origin_x: center_x - square_size / 2, origin_y: 0,
                  width: square_size, height: square_size,
        }
    }

    // Given (x, y), the image coordinates in the original image, returns the
    // coordinates within the output image.
    pub fn transform_to_rotated(&self, x: f64, y: f64,
                                width: u32, height: u32) -> (f64, f64) {
        let square_size = height;

        // The x, y origin is upper-left corner. Change to center-based
        // coordinates in the input image, where positive x,y is to upper
        // right.
        let x_cen = x - (width as f64 / 2.0);
        let y_cen = (height as f64 / 2.0) - y;

        // Rotate according to the transform.
        let (x_cen_rot, y_cen_rot) =
            Self::rotate_vector(x_cen, y_cen, self.sin_term, self.cos_term);

        // Move back to corner-based origin in the output image.
        (x_cen_rot + (square_size as f64 / 2.0),
         (square_size as f64 / 2.0) - y_cen_rot)
    }

    // Given (x, y), the image coordinates in the output image after rotating,
    // returns the coordinates within the original image (prior to rotating).
    pub fn transform_from_rotated(&self, x: f64, y: f64,
                                  width: u32, height: u32) -> (f64, f64) {
        let square_size = height;

        // The x, y origin is upper-left corner. Change to center-based
        // coordinates in the input image, where positive x,y is to upper
        // right.
        let x_cen = x - (square_size as f64 / 2.0);
        let y_cen = (square_size as f64 / 2.0) - y;

        // De-rotate according to the transform.
        let (x_cen_rot, y_cen_rot) =
            Self::rotate_vector(x_cen, y_cen, -1.0 * self.sin_term, self.cos_term);

        // Move back to original image corner-based origin.
        (x_cen_rot + (width as f64 / 2.0),
         (height as f64 / 2.0) - y_cen_rot)
    }

    fn rotate_vector(x: f64, y: f64, sin_term: f64, cos_term: f64) -> (f64, f64) {
        (x * cos_term - y * sin_term,
         x * sin_term + y * cos_term)
    }
}
