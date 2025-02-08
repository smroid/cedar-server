// Copyright (c) 2025 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use canonical_error::CanonicalError;

#[derive(Clone, Copy, Debug, Default)]
pub struct ImagePos {
    // The upper-left corner is 0, 0. The center of the upper-left pixel is
    // 0.5, 0.5.
    pub x: f64,
    pub y: f64,
}

impl From<ImagePos> for [f64; 2] {
    fn from(pos: ImagePos) -> Self {
        [pos.x, pos.y]
    }
}
impl From<[f64; 2]> for ImagePos {
    fn from(pos: [f64; 2]) -> Self {
        ImagePos{x: pos[0], y: pos[1]}
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SkyCoord {
    // Degrees.
    pub ra: f64,
    pub dec: f64,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Default)]
pub struct StarInfo {
    pub pixel: ImagePos,
    pub sky_coord: SkyCoord,
    pub mag: f32,
}

#[derive(Debug, Default)]
pub struct SolveExtension {
    // See tetra3.py for descriptions of fields.
    pub target_pixel: Option<Vec<ImagePos>>,
    pub target_sky_coord: Option<Vec<SkyCoord>>,
    pub return_matches: bool,
    pub return_catalog: bool,
    pub return_rotation_matrix: bool,
}

#[derive(Debug, Default)]
pub struct SolveParams {
    // See tetra3.py for descriptions of fields.

    // Estimated horizontal field of view, and the maximum tolerance (both in
    // degrees). None means solve blindly over the span of FOVs supported by
    // the pattern database.
    pub fov_estimate: Option<(f64, f64)>,

    pub match_radius: Option<f64>,  // Defaults to 0.01.
    pub match_threshold: Option<f64>,  // Defaults to 1e-4.
    pub solve_timeout: Option<Duration>,  // Defaults to 5 seconds.
    pub distortion: Option<f64>,
    pub match_max_error: Option<f64>,  // Defaults to pattern_max_error from database.
}

#[derive(Debug, Default)]
pub struct SolveResult {
    // See tetra3.py for descriptions of fields.
    pub image_sky_coord: SkyCoord,
    pub roll: f64,
    pub fov: f64,
    pub distortion: Option<f64>,

    // RMSE, p90 error, max error. Arcseconds.
    pub error_estimate: (f64, f64, f64),

    pub num_matches: i32,
    pub prob: f64,

    pub epoch_equinox: u16,
    pub epoch_proper_motion: f32,

    // Result of SolveExtension.target_pixel.
    pub target_sky_coord: Option<Vec<SkyCoord>>,

    // Result of SolveExtension.target_sky_coord.
    pub target_pixel: Option<Vec<Option<ImagePos>>>,

    pub matched_stars: Option<Vec<StarInfo>>,
    pub pattern_centroids: Option<Vec<ImagePos>>,
    pub catalog_stars: Option<Vec<StarInfo>>,

    pub rotation_matrix: Option<[[f64; 3]; 3]>,  // 3x3 matrix.
}

// See tetra3.py in cedar-solve for description of args.
// If SolveResult is not returned, an error is returned:
//   NotFound: no match was found.
//   DeadlineExceeded: the params.solve_timeout was reached.
//   DeadlineExceeded: the solve operation was canceled. This should return
//     CancelledError, but CanonicalError does not provide that.
//   InvalidArgument: too few centroids were provided.
pub trait SolverTrait {
    // Note: this is a blocking call, and can take up to several seconds in the
    // Python/Numpy implementation (50ms typical).
    fn solve_from_centroids(&self,
                            star_centroids: &Vec<ImagePos>,
                            width: usize, height: usize,
                            extension: &SolveExtension,
                            params: &SolveParams,
                            cancel: Option<Arc<AtomicBool>>)
                            -> Result<SolveResult, CanonicalError>;

    // Note: Equivalents for Tetra3's transform_to_image_coords() and
    // transform_to_celestial_coords() can be found in
    // cedar_server/src/astro_util.rs.
}
