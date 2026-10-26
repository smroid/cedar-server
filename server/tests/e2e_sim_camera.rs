//! End-to-end: the simulated camera (`--sim_source`) playing back a recorded
//! session through cedar-server's own `DetectEngine -> SolveEngine`, graded
//! against the plate solutions recorded with it (`manifest.csv`, as written by
//! the benchmark capture).
//!
//! `#[ignore]`d: needs the recorded corpus and the cedar-solve venv. Run with
//!
//! ```text
//! source cedar-solve/.cedar_venv/bin/activate
//! cd cedar-server
//! cargo test --release --test e2e_sim_camera -- \
//!     --ignored --test-threads=1 --nocapture
//! ```
//!
//! The corpus defaults to `benchmarks/bursts1`; override with
//! `CEDAR_E2E_SIM_DIR`. Like `e2e_plate_solve`, each run strands
//! `tetra3_server.py` subprocesses; reap with `pkill -f '[t]etra3_server.py'`.

use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::{atomic::AtomicBool, Arc},
};

use cedar_camera::{
    abstract_camera::AbstractCamera,
    sim_camera::{SimCamera, SimController},
};
use cedar_elements::astro_util::angular_separation;
use image::GrayImage;
use tetra3_server::tetra3_solver::Tetra3Solver;
use tokio::sync::Mutex;

#[allow(dead_code)]
mod common;

use common::harness::{SharedCamera, Stack};

/// Frames graded by the seek test.
const SEEK_FRAMES: usize = 12;
/// Results a seek may take to reach its frame (see `Stack::solve_image`).
const MAX_SETTLE_RESULTS: usize = 32;
/// The same images solved by the same solver; disagreement is detect noise.
const CENTER_TOL_DEG: f64 = 0.1;
/// cedar_server's binning for a 1920x1080 sensor (`compute_binning`).
const DETECT_BINNING: u32 = 2;

struct Recorded {
    filename: String,
    solved: bool,
    ra_deg: f64,
    dec_deg: f64,
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .to_path_buf()
}

fn read_manifest(dir: &Path) -> Vec<Recorded> {
    let text = std::fs::read_to_string(dir.join("manifest.csv")).unwrap();
    let mut lines = text.lines();
    let header: Vec<&str> = lines.next().unwrap().split(',').collect();
    let col = |name: &str| header.iter().position(|h| *h == name).unwrap();
    let (f, s, ra, dec) =
        (col("filename"), col("solved"), col("ra_deg"), col("dec_deg"));
    lines
        .map(|l| l.split(',').collect::<Vec<_>>())
        .map(|v| Recorded {
            filename: v[f].to_string(),
            solved: v[s] == "true",
            ra_deg: v[ra].parse().unwrap_or(f64::NAN),
            dec_deg: v[dec].parse().unwrap_or(f64::NAN),
        })
        .collect()
}

/// Returns None (skip) if the corpus or the venv is missing.
fn sim_dir() -> Option<PathBuf> {
    let dir = std::env::var_os("CEDAR_E2E_SIM_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| repo_root().join("benchmarks/bursts1"));
    if !dir.join("manifest.csv").is_file() {
        eprintln!("SKIP: no manifest.csv in {}", dir.display());
        return None;
    }
    let venv_ok = Command::new("python")
        .args(["-c", "import tetra3"])
        .output()
        .is_ok_and(|o| o.status.success());
    if !venv_ok {
        eprintln!("SKIP: activate cedar-solve/.cedar_venv");
        return None;
    }
    Some(dir)
}

async fn stack(ctl: &SimController) -> Stack {
    let script = repo_root().join("tetra3_server/python/tetra3_server.py");
    let solver = Tetra3Solver::new(
        script.to_str().unwrap(),
        "default_database",
        Arc::new(AtomicBool::new(false)),
    )
    .await
    .expect("Tetra3Solver::new");
    let camera: SharedCamera = Arc::new(Mutex::new(Box::new(
        SimCamera::new(ctl.clone()).await.unwrap(),
    )
        as Box<dyn AbstractCamera + Send>));
    let mut stack =
        Stack::with_camera(Arc::new(Mutex::new(solver)), camera).await;
    stack.set_detect_binning(DETECT_BINNING).await;
    stack
}

fn load(dir: &Path, filename: &str) -> GrayImage {
    image::open(dir.join(filename)).unwrap().to_luma8()
}

/// Seeking (what `POST /sim/seek` does) lands the engine on that frame, and
/// it solves to where the recorded session said it pointed.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs benchmarks/bursts1 and the cedar-solve venv"]
async fn seek_solves_recorded_frames() {
    let Some(dir) = sim_dir() else { return };
    let recorded = read_manifest(&dir);
    let ctl = SimController::new(&dir, Path::new(".")).unwrap();
    let mut stack = stack(&ctl).await;

    let mut graded = 0;
    let mut agreed = 0;
    for (index, rec) in recorded.iter().enumerate().take(SEEK_FRAMES) {
        ctl.seek(index).unwrap();
        let want = load(&dir, &rec.filename);
        let mut reached = None;
        for _ in 0..MAX_SETTLE_RESULTS {
            let ps = stack.next_result().await;
            if ps.detect_result.captured_image.image.as_raw() == want.as_raw() {
                reached = Some(ps);
                break;
            }
        }
        let ps =
            reached.unwrap_or_else(|| panic!("never reached frame {index}"));
        if !rec.solved {
            continue;
        }
        graded += 1;
        let Some(sol) = ps.plate_solution else {
            println!("{index:3} {}: no solution", rec.filename);
            continue;
        };
        let c = sol.image_sky_coord.unwrap();
        let sep = angular_separation(
            rec.ra_deg.to_radians(),
            rec.dec_deg.to_radians(),
            c.ra.to_radians(),
            c.dec.to_radians(),
        )
        .to_degrees();
        println!("{index:3} {}: off by {:.4} deg", rec.filename, sep);
        if sep < CENTER_TOL_DEG {
            agreed += 1;
        }
    }
    println!("{agreed}/{graded} recorded solutions reproduced");
    assert!(graded > 0, "no recorded solved frames in the first {SEEK_FRAMES}");
    // Allow one miss: the box solved these live, from its own detections.
    assert!(agreed + 1 >= graded, "{agreed}/{graded} agreed");
}

/// Playing (what `POST /sim/play` does) feeds successive recorded frames, in
/// order, through the engine.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs benchmarks/bursts1 and the cedar-solve venv"]
async fn play_advances_through_frames() {
    let Some(dir) = sim_dir() else { return };
    let recorded = read_manifest(&dir);
    let ctl = SimController::new(&dir, Path::new(".")).unwrap();
    let mut stack = stack(&ctl).await;
    let frames: Vec<GrayImage> = recorded
        .iter()
        .take(10)
        .map(|r| load(&dir, &r.filename))
        .collect();

    ctl.set_speed(4.0).unwrap();
    ctl.play();
    let mut seen: Vec<usize> = vec![];
    for _ in 0..60 {
        let ps = stack.next_result().await;
        let raw = ps.detect_result.captured_image.image.as_raw();
        let Some(i) = frames.iter().position(|f| f.as_raw() == raw) else {
            break; // Past the frames we loaded.
        };
        if seen.last() != Some(&i) {
            seen.push(i);
        }
    }
    ctl.pause();
    println!("frames seen in order: {seen:?}");
    assert!(seen.len() >= 4, "only saw frames {seen:?}");
    assert!(seen.windows(2).all(|w| w[0] < w[1]), "out of order: {seen:?}");
}
