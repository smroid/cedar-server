//! End-to-end plate-solve harness over cedar-server's own engines.
//!
//! Drives `ImageCamera -> DetectEngine -> SolveEngine` against a corpus of
//! synthetic Gaia fields with exact WCS ground truth, asserting pose, latency,
//! and solve rate. The solver is `SolveEngine::new`'s first argument, so the
//! same corpus and gates run against any `SolverTrait` impl -- which is how a
//! tetra3 -> tetra3rs swap will be evaluated.
//!
//! These tests are `#[ignore]`d: they need a staged corpus and the cedar-solve
//! venv. Run them with
//!
//! ```text
//! cd cedar-solve && source .cedar_venv/bin/activate && \
//!   python tools/fetch_corpus.py
//! export CEDAR_E2E_DATA_DIR=$PWD/tests/data/synthetic_large
//! cd ../cedar-server
//! cargo test --test e2e_plate_solve -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! tetra3_server's Python gRPC bindings are generated, not checked in, so a
//! fresh clone fails with `ModuleNotFoundError: No module named 'tetra3_pb2'`.
//! Generate them once:
//!
//! ```text
//! cd tetra3_server && python -m grpc_tools.protoc -I proto \
//!     --python_out=python --grpc_python_out=python proto/tetra3.proto
//! ```
//!
//! `--test-threads=1` is required, not cosmetic: `Tetra3Solver` connects to the
//! hardcoded Unix socket `/tmp/cedar.sock` (see `Tetra3Solver::new` in
//! tetra3_server), so only one may exist at a time.
//!
//! Each test that builds a real `Tetra3Solver` leaves a `tetra3_server.py`
//! subprocess running after the run. `Tetra3Solver`'s `Drop` would stop it, but
//! `SolveEngine`'s worker thread has no shutdown path and keeps a clone of the
//! solver `Arc` alive, so `Drop` never fires. Harmless for one run; reap them
//! between runs, or they accumulate:
//!
//! ```text
//! pkill -f '[t]etra3_server.py'
//! ```

use std::sync::{atomic::AtomicBool, Arc};

use cedar_elements::solver_trait::SolverTrait;
use image::GrayImage;
use tetra3_server::tetra3_solver::Tetra3Solver;
use tokio::sync::Mutex;

mod common;

use common::{
    corpus::{self, Env, Field, Preconditions},
    fake_solver::FakeSolver,
    harness::{evaluate, expected_roll_deg, Stack},
    report::{print_table, Report, SOLVE_P95_TARGET_MS},
};

type SharedSolver = Arc<Mutex<dyn SolverTrait + Send + Sync>>;

/// Corpus-wide gate: every committed field is expected to solve.
const MIN_SOLVE_RATE: f64 = 1.0;

/// Returns the env, or prints why it is skipping and returns None.
fn setup() -> Option<Env> {
    match corpus::preconditions() {
        Preconditions::Ready(env) => Some(env),
        Preconditions::Skip(why) => {
            eprintln!("\nSKIPPING e2e plate-solve test:\n{why}\n");
            None
        }
    }
}

async fn tetra3_solver(env: &Env) -> SharedSolver {
    let solver = Tetra3Solver::new(
        env.tetra3_script.to_str().expect("utf-8 script path"),
        &env.tetra3_database,
        Arc::new(AtomicBool::new(false)),
    )
    .await
    .expect("Tetra3Solver::new (is the cedar-solve venv active?)");
    Arc::new(Mutex::new(solver))
}

fn fields(env: &Env) -> Vec<Field> {
    corpus::load_manifest(&env.data_dir).expect("parse manifest.csv")
}

/// Gate 1: prove ONE field solves through the real engine stack, and pin the
/// roll convention empirically, before spending a full run on 99 fields.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs CEDAR_E2E_DATA_DIR and the cedar-solve venv on PATH"]
async fn gate1_single_field() {
    let Some(env) = setup() else { return };
    let fields = fields(&env);
    let field = &fields[0];
    let image = corpus::load_image(&env.data_dir, field).expect("load png");

    let solver = tetra3_solver(&env).await;
    let mut stack = Stack::new(solver, image.clone()).await;
    let ps = stack.solve_image(image).await;

    println!("\n=== Gate 1: {} ===", field.name);
    println!("  centroids       {}", ps.detect_result.star_candidates.len());
    println!("  frame_id        {}", ps.detect_result.frame_id);
    println!("  solution_id     {}", ps.solution_id);

    let p = ps
        .plate_solution
        .as_ref()
        .unwrap_or_else(|| panic!("{} did not solve", field.name));
    let coord = p.image_sky_coord.as_ref().unwrap();

    println!(
        "  ground truth    ra {:.4}  dec {:.4}  rotation {:.1}",
        field.ra_deg, field.dec_deg, field.rotation_deg
    );
    println!("  solved          ra {:.4}  dec {:.4}", coord.ra, coord.dec);
    println!(
        "  roll            raw {:.4}   expected (180+rot)%360 = {:.4}",
        p.roll,
        expected_roll_deg(field.rotation_deg)
    );
    println!(
        "  fov             {:.4} (gt {:.4} gnomonic; \
        manifest fov_x_deg {:.4})",
        p.fov,
        field.true_fov_x_deg(),
        field.fov_x_deg
    );

    let o = evaluate(field, &ps);
    println!(
        "  center {:.4}'   roll_err {:.4} deg   \
        fov_err {:.4}%   {:.1} ms   matches {}",
        o.center_arcmin,
        o.roll_err_deg,
        o.fov_err_frac * 100.0,
        o.solve_time_ms,
        o.num_matches
    );

    assert!(o.solved, "{} did not solve", field.name);
    // Loose: this gate is about conventions (flip/rotation/pixscale), not
    // accuracy.
    assert!(
        o.center_arcmin < 30.0,
        "center off by {:.2}' -- suspect a WCS convention mismatch",
        o.center_arcmin
    );
    assert!(
        o.roll_err_deg.abs() < 1.0,
        "roll {:.3} does not match (180 + {:.1}) % 360 = {:.3}; the pinned \
         convention is wrong at this boundary",
        p.roll,
        field.rotation_deg,
        expected_roll_deg(field.rotation_deg)
    );
    assert!(ps.detect_result.frame_id > 0, "camera never captured");
}

/// The full corpus, plus the controls that keep it honest.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs CEDAR_E2E_DATA_DIR and the cedar-solve venv on PATH"]
async fn e2e_corpus() {
    let Some(env) = setup() else { return };
    let fields = fields(&env);
    assert!(!fields.is_empty(), "empty manifest");
    println!(
        "\nRunning {} fields from {}",
        fields.len(),
        env.data_dir.display()
    );

    // ---- Real solver over the whole corpus -------------------------------
    let solver = tetra3_solver(&env).await;
    let seed = corpus::load_image(&env.data_dir, &fields[0]).expect("load png");
    let mut stack = Stack::new(solver, seed).await;

    let mut outcomes = Vec::with_capacity(fields.len());
    for field in &fields {
        let image = corpus::load_image(&env.data_dir, field).expect("load png");
        let ps = stack.solve_image(image).await;
        outcomes.push(evaluate(field, &ps));
    }
    let tetra3_report = Report::new("tetra3", outcomes);

    // ---- Negative control B: a blank frame must not produce a pose --------
    // Fewer than MINIMUM_STARS (4) centroids, so solve_engine never even calls
    // the solver.
    let blank = GrayImage::new(fields[0].nx, fields[0].ny);
    let blank_ps = stack.solve_image(blank).await;
    assert!(
        blank_ps.plate_solution.is_none(),
        "a blank frame produced a plate solution ({} centroids)",
        blank_ps.detect_result.star_candidates.len()
    );

    // ---- The seam: same harness, same gates, a different solver ----------
    let target = &fields[0];
    let target_image =
        corpus::load_image(&env.data_dir, target).expect("load png");

    let mut honest =
        Stack::new(FakeSolver::honest(target).shared(), target_image.clone())
            .await;
    let honest_outcome =
        evaluate(target, &honest.solve_image(target_image.clone()).await);
    let fake_report = Report::new("fake-honest", vec![honest_outcome.clone()]);

    // ---- Negative control A: a wrong solver must fail the gates ----------
    let mut wrong =
        Stack::new(FakeSolver::wrong(target).shared(), target_image.clone())
            .await;
    let wrong_outcome =
        evaluate(target, &wrong.solve_image(target_image).await);

    // ---- Report before asserting, so a failure prints its evidence -------
    print_table(&[&tetra3_report, &fake_report]);
    match tetra3_report.write_csv() {
        Ok(p) => println!("per-field CSV: {}", p.display()),
        Err(e) => eprintln!("could not write CSV: {e}"),
    }

    for o in tetra3_report.failures() {
        eprintln!(
            "FAIL {}: solved={} center={:.3}' roll_err={:.3} fov_err={:.3}% \
             centroids={} matches={} {:.1} ms",
            o.name,
            o.solved,
            o.center_arcmin,
            o.roll_err_deg,
            o.fov_err_frac * 100.0,
            o.num_centroids,
            o.num_matches,
            o.solve_time_ms
        );
    }

    // ---- Gates ------------------------------------------------------------
    assert!(
        honest_outcome.passed(),
        "the harness rejected a solver returning exact ground truth -- it is \
         coupled to tetra3, or the gates are miscalibrated: {honest_outcome:?}"
    );
    assert!(
        !wrong_outcome.passed(),
        "a solver 10 degrees off in declination PASSED -- the gates are not \
         firing: {wrong_outcome:?}"
    );

    let s = tetra3_report.summary();
    let solve_rate = s.solved as f64 / s.total as f64;
    assert!(
        solve_rate >= MIN_SOLVE_RATE,
        "solve rate {:.3} below {:.3} ({}/{} fields solved)",
        solve_rate,
        MIN_SOLVE_RATE,
        s.solved,
        s.total
    );
    assert_eq!(
        s.passed,
        s.total,
        "{} of {} fields missed the pose gates",
        s.total - s.passed,
        s.total
    );
    assert!(
        s.solve_p95_ms < SOLVE_P95_TARGET_MS,
        "solve_time p95 {:.1} ms exceeds the {:.0} ms target",
        s.solve_p95_ms,
        SOLVE_P95_TARGET_MS
    );

    // Keep the fake stacks alive until here so their workers are not dropped
    // mid-assert.
    drop(honest);
    drop(wrong);
}
