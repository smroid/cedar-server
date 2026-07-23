// Engine-level harness: drives cedar-server's real DetectEngine + SolveEngine
// over a static ImageCamera, with the solver as a swappable component.
//
// This deliberately stops short of the gRPC server: no tonic, no port 80, no
// Bluetooth, no Operate-mode state machine. What it does exercise is the same
// camera -> detect -> solve path the box runs in flight.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use cedar_camera::abstract_camera::AbstractCamera;
use cedar_camera::image_camera::ImageCamera;
use cedar_elements::astro_util::angular_separation;
use cedar_elements::cedar::{ImageCoord, LatLong, PlateSolution as PlateSolutionProto};
use cedar_elements::cedar_common::CelestialCoord;
use cedar_elements::solver_trait::SolverTrait;
use cedar_server::detect_engine::{DetectEngine, DetectResult};
use cedar_server::solve_engine::{PlateSolution, SolveEngine};
use image::GrayImage;
use tokio::sync::Mutex;

use super::corpus::Field;

// Production values, from server_main's `MyCedar::new` call and the pico-args
// defaults in cedar_server.rs.
const INITIAL_EXPOSURE: Duration = Duration::from_millis(100);
const MIN_EXPOSURE: Duration = Duration::from_micros(10); // --min_exposure 0.00001 s
const MAX_EXPOSURE: Duration = Duration::from_secs(1); // --max_exposure 1.0 s
const DETECTION_SIGMA: f64 = 8.0; // --sigma
const STAR_COUNT_GOAL: i32 = 20; // --star_count_goal
const STATS_CAPACITY: usize = 100;

/// Upper bound on solutions consumed while waiting for a swapped-in image to
/// reach the solver. Generous: in practice it settles within a few.
const MAX_SETTLE_RESULTS: usize = 32;

// Gates. Center/roll/FOV mirror cedar-solve/tests/test_solve_e2e.py.
pub const CENTER_TOL_ARCMIN: f64 = 5.0;
pub const ROLL_TOL_DEG: f64 = 1.0;
pub const FOV_TOL_FRAC: f64 = 0.02;

type SharedSolver = Arc<Mutex<dyn SolverTrait + Send + Sync>>;
type SharedCamera = Arc<Mutex<Box<dyn AbstractCamera + Send>>>;

fn wrap_camera(camera: ImageCamera) -> SharedCamera {
    Arc::new(Mutex::new(Box::new(camera) as Box<dyn AbstractCamera + Send>))
}

/// The engine stack, built once and reused across every field. Only the camera
/// is swapped per field, which is what `cedar_server.rs`'s demo-image path does.
pub struct Stack {
    detect: Arc<Mutex<DetectEngine>>,
    solve: SolveEngine,
    /// Cursor into SolveEngine's monotonically increasing `solution_id`.
    /// Deliberately not `frame_id`: that restarts at zero for each new
    /// ImageCamera and would alias across fields.
    last_id: Option<i32>,
}

impl Stack {
    pub async fn new(solver: SharedSolver, seed_image: GrayImage) -> Stack {
        let camera = wrap_camera(
            ImageCamera::new(seed_image)
                .await
                .expect("ImageCamera::new"),
        );

        let detect = Arc::new(Mutex::new(DetectEngine::new(
            INITIAL_EXPOSURE,
            MIN_EXPOSURE,
            MAX_EXPOSURE,
            DETECTION_SIGMA,
            STAR_COUNT_GOAL,
            camera,
            STATS_CAPACITY,
            /*hot_pixel_map=*/ None,
        )));

        // ImageCamera ignores exposure changes -- the pixels are baked into the
        // PNG -- so autoexposure cannot converge on anything. Disabling it keeps
        // the run deterministic and stops the worker from hunting.
        detect.lock().await.set_autoexposure_enabled(false).await;

        // Note: detect_binning is left at its default of 1, so CedarDetect runs
        // on the full-resolution frame. That matches cedar-box-server, which
        // passes default_total_binning=None -- and the binned buffer is only
        // used when DetectEngine's `detect_binning` differs from 1.

        let pre_solve: Arc<
            dyn Fn() -> Pin<
                    Box<dyn Future<Output = (Option<CelestialCoord>, Option<CelestialCoord>)> + Send>,
                > + Send
                + Sync,
        > = Arc::new(|| Box::pin(async { (None, None) }));

        let post_solve: Arc<
            dyn Fn(
                    Option<ImageCoord>,
                    Option<DetectResult>,
                    Option<PlateSolutionProto>,
                ) -> Pin<Box<dyn Future<Output = Option<LatLong>> + Send>>
                + Send
                + Sync,
        > = Arc::new(|_, _, _| Box::pin(async { None }));

        let solve = SolveEngine::new(
            solver,
            /*cedar_sky=*/ None,
            /*hot_pixel_map=*/ None,
            /*imu_tracker=*/ None,
            detect.clone(),
            STATS_CAPACITY,
            pre_solve,
            post_solve,
            /*observer_location=*/ None,
        )
        .expect("SolveEngine::new");

        Stack {
            detect,
            solve,
            last_id: None,
        }
    }

    /// Swaps in `image` and returns the first solution that provably belongs to
    /// it.
    ///
    /// Identity is established by comparing pixels, not by counting results.
    /// Draining a fixed number of solutions does NOT work: while the solve
    /// worker is busy (a solve costs tens of ms), the detect worker keeps
    /// capturing from the camera it still holds, so several post-swap solutions
    /// can carry a DetectResult sourced from the *previous* image. Those solve
    /// successfully and look plausible -- they just describe the wrong field.
    /// `DetectResult` carries the frame it was computed from, so we match on it.
    pub async fn solve_image(&mut self, image: GrayImage) -> PlateSolution {
        let camera = wrap_camera(
            ImageCamera::new(image.clone())
                .await
                .expect("ImageCamera::new"),
        );
        self.detect.lock().await.replace_camera(camera).await;
        self.solve.clear_plate_solution().await;

        for _ in 0..MAX_SETTLE_RESULTS {
            let ps = self.next_result().await;
            if ps.detect_result.captured_image.image.as_raw() == image.as_raw() {
                return ps;
            }
        }
        panic!(
            "after {MAX_SETTLE_RESULTS} solutions the engine was still reporting \
             on a stale frame; the camera swap never took effect"
        );
    }

    async fn next_result(&mut self) -> PlateSolution {
        let ps = self
            .solve
            .get_next_result(self.last_id, /*non_blocking=*/ false)
            .await
            .expect("blocking get_next_result returns Some");
        self.last_id = Some(ps.solution_id);
        ps
    }
}

/// Per-field measurement. Errors are recorded whether or not they pass, so the
/// report can show near-misses.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub name: String,
    pub solved: bool,
    pub center_arcmin: f64,
    pub roll_err_deg: f64,
    pub fov_err_frac: f64,
    pub solve_time_ms: f64,
    pub num_matches: i32,
    pub num_centroids: usize,
}

impl Outcome {
    pub fn passed(&self) -> bool {
        self.solved
            && self.center_arcmin < CENTER_TOL_ARCMIN
            && self.roll_err_deg.abs() < ROLL_TOL_DEG
            && self.fov_err_frac < FOV_TOL_FRAC
    }
}

/// Smallest signed difference between two angles, in [-180, 180].
fn circular_diff_deg(a: f64, b: f64) -> f64 {
    (a - b + 180.0).rem_euclid(360.0) - 180.0
}

/// tetra3 reports Roll as celestial north relative to image up. Pinned
/// empirically by the earlier Python suite: Roll == (180 + rotation) mod 360.
/// Raw at this boundary -- serve_engine.rs derives `zenith_roll_angle` for
/// display, but that is downstream of get_next_result.
pub fn expected_roll_deg(rotation_deg: f64) -> f64 {
    (180.0 + rotation_deg).rem_euclid(360.0)
}

pub fn evaluate(field: &Field, ps: &PlateSolution) -> Outcome {
    let num_centroids = ps.detect_result.star_candidates.len();
    let Some(p) = ps.plate_solution.as_ref() else {
        return Outcome {
            name: field.name.clone(),
            solved: false,
            center_arcmin: f64::NAN,
            roll_err_deg: f64::NAN,
            fov_err_frac: f64::NAN,
            solve_time_ms: f64::NAN,
            num_matches: 0,
            num_centroids,
        };
    };

    let coord = p
        .image_sky_coord
        .as_ref()
        .expect("a solved PlateSolution always carries image_sky_coord");

    // angular_separation is radians in, radians out; proto coords are degrees.
    let center_arcmin = angular_separation(
        field.ra_deg.to_radians(),
        field.dec_deg.to_radians(),
        coord.ra.to_radians(),
        coord.dec.to_radians(),
    )
    .to_degrees()
        * 60.0;

    let roll_err_deg = circular_diff_deg(p.roll, expected_roll_deg(field.rotation_deg));
    // Against the gnomonic FOV, not the manifest's small-angle fov_x_deg -- see
    // Field::true_fov_x_deg.
    let true_fov = field.true_fov_x_deg();
    let fov_err_frac = (p.fov - true_fov).abs() / true_fov;

    let solve_time_ms = p
        .solve_time
        .as_ref()
        .map(|d| d.seconds as f64 * 1000.0 + d.nanos as f64 / 1.0e6)
        .unwrap_or(f64::NAN);

    Outcome {
        name: field.name.clone(),
        solved: true,
        center_arcmin,
        roll_err_deg,
        fov_err_frac,
        solve_time_ms,
        num_matches: p.num_matches,
        num_centroids,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(rotation_deg: f64) -> Field {
        Field {
            name: "t".into(),
            ra_deg: 10.0,
            dec_deg: 20.0,
            rotation_deg,
            fov_x_deg: 12.71,
            fov_y_deg: 7.149_375,
            pixscale_arcsec: 23.831_25,
            nx: 1920,
            ny: 1080,
            n_rendered: 50,
        }
    }

    fn outcome(center_arcmin: f64, roll_err_deg: f64, fov_err_frac: f64) -> Outcome {
        Outcome {
            name: "t".into(),
            solved: true,
            center_arcmin,
            roll_err_deg,
            fov_err_frac,
            solve_time_ms: 4.0,
            num_matches: 30,
            num_centroids: 50,
        }
    }

    #[test]
    fn on_truth_passes() {
        assert!(outcome(0.04, 0.0, 0.004).passed());
    }

    #[test]
    fn center_just_outside_tolerance_fails() {
        assert!(!outcome(6.0, 0.0, 0.004).passed());
    }

    #[test]
    fn fov_outside_tolerance_fails() {
        assert!(!outcome(0.04, 0.0, 0.03).passed());
    }

    #[test]
    fn unsolved_never_passes() {
        let mut o = outcome(0.0, 0.0, 0.0);
        o.solved = false;
        assert!(!o.passed());
    }

    #[test]
    fn roll_wraps_across_zero() {
        // rotation 179.5 -> expected roll 359.5; solver says 0.2. The true error
        // is 0.7 deg, not 359.3.
        let f = field(179.5);
        assert!((expected_roll_deg(f.rotation_deg) - 359.5).abs() < 1e-9);
        let err = circular_diff_deg(0.2, expected_roll_deg(f.rotation_deg));
        assert!((err - 0.7).abs() < 1e-9, "err = {err}");
        assert!(outcome(0.04, err, 0.004).passed());
    }

    #[test]
    fn roll_convention_matches_pinned_formula() {
        // Spot values for the pinned convention solver_Roll == (180 + rotation)
        // mod 360, which mirrors cedar-solve/tests/test_solve_e2e.py.
        assert_eq!(expected_roll_deg(0.0), 180.0);
        assert_eq!(expected_roll_deg(-180.0), 0.0);
        assert_eq!(expected_roll_deg(30.0), 210.0);
        assert_eq!(expected_roll_deg(-45.0), 135.0);
    }

    #[test]
    fn roll_error_beyond_tolerance_fails() {
        assert!(!outcome(0.04, 1.5, 0.004).passed());
    }
}
