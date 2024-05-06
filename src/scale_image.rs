use image::GrayImage;

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
    image: &GrayImage, mut min_pixel_value: u8, peak_pixel_value: u8, gamma: f32)
    -> GrayImage {
    if min_pixel_value > peak_pixel_value / 2 {
        min_pixel_value = peak_pixel_value / 2;
    }
    let lut = compute_lut(min_pixel_value, peak_pixel_value, gamma);

    // Apply the lut
    let out_vec: Vec<u8> = image.as_raw().iter().map(|x| lut[*x as usize]).collect();

    let (width, height) = image.dimensions();
    GrayImage::from_raw(width, height, out_vec).unwrap()
}

// In-place variant of scale_image().
pub fn scale_image_mut(
    image: &mut GrayImage, mut min_pixel_value: u8, peak_pixel_value: u8, gamma: f32) {
    if min_pixel_value > peak_pixel_value / 2 {
        min_pixel_value = peak_pixel_value / 2;
    }
    let lut = compute_lut(min_pixel_value, peak_pixel_value, gamma);

    for pixel in image.pixels_mut() {
        pixel[0] = lut[pixel[0] as usize];
    }
}
