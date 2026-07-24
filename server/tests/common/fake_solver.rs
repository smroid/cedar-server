// A canned SolverTrait impl. Its job is to prove the harness is not
// tetra3-specific: the same engine stack, corpus, and gates must work against
// any solver behind the trait -- which is the whole point of the exercise,
// since tetra3rs is coming next.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use canonical_error::CanonicalError;
use cedar_elements::{
    cedar::{ImageCoord, PlateSolution},
    cedar_common::CelestialCoord,
    imu_trait::EquatorialCoordinates,
    solver_trait::{SolveExtension, SolveParams, SolverTrait},
};
use tokio::sync::Mutex;

use super::{corpus::Field, harness::expected_roll_deg};

pub struct FakeSolver {
    ra: f64,
    dec: f64,
    roll: f64,
    fov: f64,
}

impl FakeSolver {
    /// Returns exactly the field's ground truth, so it passes every gate.
    pub fn honest(field: &Field) -> FakeSolver {
        FakeSolver {
            ra: field.ra_deg,
            dec: field.dec_deg,
            roll: expected_roll_deg(field.rotation_deg),
            // The gnomonic FOV, which is what a correct solver reports -- not
            // the manifest's small-angle fov_x_deg. See Field::true_fov_x_deg.
            fov: field.true_fov_x_deg(),
        }
    }

    /// Off by 10 degrees in declination -- far outside the 5 arcmin gate. If
    /// the harness passes this, the gates are not actually firing.
    pub fn wrong(field: &Field) -> FakeSolver {
        FakeSolver {
            dec: field.dec_deg + 10.0,
            ..FakeSolver::honest(field)
        }
    }

    pub fn shared(self) -> Arc<Mutex<dyn SolverTrait + Send + Sync>> {
        Arc::new(Mutex::new(self))
    }
}

#[async_trait]
impl SolverTrait for FakeSolver {
    async fn solve_from_centroids(
        &self,
        _star_centroids: &[ImageCoord],
        _width: usize,
        _height: usize,
        _extension: &SolveExtension,
        _params: &SolveParams,
        _imu_estimate: Option<EquatorialCoordinates>,
    ) -> Result<PlateSolution, CanonicalError> {
        Ok(PlateSolution {
            // Required: solve_engine.rs unwraps this when deriving
            // boresight_coords and target_sky_coord is empty. Omitting it
            // panics the solve worker rather than failing a gate.
            image_sky_coord: Some(CelestialCoord {
                ra: self.ra,
                dec: self.dec,
                epoch: None,
            }),
            roll: self.roll,
            fov: self.fov,
            distortion: Some(0.0),
            rmse: 1.0,
            p90_error: 1.0,
            max_error: 2.0,
            num_matches: 30,
            prob: 1.0,
            solve_time: Some(prost_types::Duration {
                seconds: 0,
                nanos: 5_000_000, // 5 ms
            }),
            rotation_matrix: vec![1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
            ..Default::default()
        })
    }

    fn cancel(&self) {}

    fn default_timeout(&self) -> Duration {
        Duration::from_secs(1)
    }
}
