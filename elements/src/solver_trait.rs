// Copyright (c) 2025 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use canonical_error::CanonicalError;
use tonic::async_trait;

use crate::cedar_common::CelestialCoord;
use crate::cedar::{ImageCoord, PlateSolution};

#[derive(Debug, Default)]
pub struct SolveExtension {
    // See tetra3.py for descriptions of fields.
    pub target_pixel: Option<Vec<ImageCoord>>,
    pub target_sky_coord: Option<Vec<CelestialCoord>>,
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
    pub solve_timeout: Option<Duration>,  // Default determined by implementation.
    pub distortion: Option<f64>,
    pub match_max_error: Option<f64>,  // Defaults to pattern_max_error from database.
}

// See tetra3.py in cedar-solve for description of args.
// If SolveResult is not returned, an error is returned:
//   NotFound: no match was found.
//   DeadlineExceeded: the params.solve_timeout was reached.
//   DeadlineExceeded: the solve operation was canceled. This should return
//     CancelledError, but CanonicalError does not provide that.
//   InvalidArgument: too few centroids were provided.
#[async_trait]
pub trait SolverTrait {
    // Note: this is a blocking call, and can take up to several seconds in the
    // Python/Numpy implementation (50ms typical).
    async fn solve_from_centroids(&self,
                                  star_centroids: &Vec<ImageCoord>,
                                  width: usize, height: usize,
                                  extension: &SolveExtension,
                                  params: &SolveParams,
                                  cancel: Option<Arc<AtomicBool>>)
                                  -> Result<PlateSolution, CanonicalError>;

    // TODO: cancel method.
    // TODO: method to return default timeout.

    // Note: Equivalents for Tetra3's transform_to_image_coords() and
    // transform_to_celestial_coords() can be found in
    // cedar_server/src/astro_util.rs.
}
