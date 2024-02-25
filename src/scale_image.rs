use image::GrayImage;

fn get_peak_pixel(image: &GrayImage) -> u8 {
    let mut peak_value = 0_u8;
    for pixel in image.pixels() {
        if pixel[0] > peak_value {
            peak_value = pixel[0];
        }
    }
    peak_value
}

fn compute_lut(peak_pixel_value: Option<u8>, gamma: f32) -> [u8; 256] {
    let mut lut: [u8; 256] = [0; 256];
    let scale = 256.0 / (peak_pixel_value.unwrap() as f32).powf(gamma);
    for n in 0..256 {
        let mapped = scale * (n as f32).powf(gamma);
        let clamped = if mapped >= 255.0 {
            255_u8
        } else {
            mapped as u8
        };
        lut[n] = clamped;
    }
    lut
}

// Copy the image, mapping 0..peak_pixel_value to 0..255 by applying a
// gamma and scale factor. If `peak_pixel_value` is omitted it is computed
// from the image.
pub fn scale_image(image: &GrayImage, mut peak_pixel_value: Option<u8>, gamma: f32)
                   -> GrayImage {
    if peak_pixel_value.is_none() {
        peak_pixel_value = Some(get_peak_pixel(image));
    }
    let lut = compute_lut(peak_pixel_value, gamma);

    // Apply the lut
    let out_vec: Vec<u8> = image.as_raw().iter().map(|x| lut[*x as usize]).collect();

    let (width, height) = image.dimensions();
    GrayImage::from_raw(width, height, out_vec).unwrap()
}

// In-place variant of scale_image().
pub fn scale_image_mut(
    image: &mut GrayImage, mut peak_pixel_value: Option<u8>, gamma: f32) {
    if peak_pixel_value.is_none() {
        peak_pixel_value = Some(get_peak_pixel(image));
    }
    let lut = compute_lut(peak_pixel_value, gamma);

    for pixel in image.pixels_mut() {
        pixel[0] = lut[pixel[0] as usize];
    }
}
