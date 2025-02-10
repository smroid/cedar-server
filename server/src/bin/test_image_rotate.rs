// Copyright (c) 2024 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use std::path::PathBuf;
use std::time::Instant;

use clap::Parser;
use env_logger;
use image::ImageReader;
use log::{info, warn};

use cedar_elements::image_utils::ImageRotator;

/// Test program for rotating an image.
#[derive(Parser, Debug)]
#[command(author, version, about, long_about=None)]
struct Args {
    /// Path of the image file to process.
    #[arg(short, long)]
    input: String,

    /// Rotation angle, degrees.
    #[arg(short, long)]
    angle: f64,

    /// Fill value.
    #[arg(short, long, default_value_t = 128)]
    fill: u8,
}

fn main() {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();

    let input = args.input.as_str();
    info!("Processing {}", input);
    let input_path = PathBuf::from(&input);
    let mut output_path = PathBuf::from(".");
    output_path.push(input_path.file_name().unwrap());
    output_path.set_extension("bmp");

    let img = match ImageReader::open(&input_path).unwrap().decode() {
        Ok(img) => img,
        Err(e) => {
            warn!("Skipping {:?} due to: {:?}", input_path, e);
            return;
        },
    };
    let input_img = img.to_luma8();
    let (width, height) = input_img.dimensions();
    let image_rotator = ImageRotator::new(width, height, args.angle);
    let rotate_start = Instant::now();
    let output_img = image_rotator.rotate_image(&input_img, args.fill);
    let elapsed = rotate_start.elapsed();
    info!("Rotated in {:?}", elapsed);

    let (rot_x, rot_y) = image_rotator.transform_to_rotated(0.0, 0.0, width, height);
    info!("Original 0,0 transforms to {:.2},{:.2}", rot_x, rot_y);

    let (x, y) = image_rotator.transform_from_rotated(rot_x, rot_y, width, height);
    info!("Transforms back to {:.2},{:.2}", x, y);

    output_img.save(output_path).unwrap();
}
