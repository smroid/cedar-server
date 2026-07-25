// Summary statistics for one solver's run over the corpus. The comparison table
// this emits is the artifact a solver swap gets judged on.

use std::path::PathBuf;

use super::harness::Outcome;

/// Time-to-resolve target: p95 solve latency under 300 ms, so pointing feels
/// instant and the stack can sustain 10+ updates/sec on low-power hardware.
/// Both the comparison table and the `e2e_corpus` test gate on this.
pub const SOLVE_P95_TARGET_MS: f64 = 300.0;

pub struct Report {
    pub solver: String,
    pub outcomes: Vec<Outcome>,
}

/// Nearest-rank percentile over an already-sorted slice. `p` in [0, 1].
fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let rank = (p * sorted.len() as f64).ceil().max(1.0) as usize;
    sorted[rank.min(sorted.len()) - 1]
}

fn sorted_finite(values: impl Iterator<Item = f64>) -> Vec<f64> {
    let mut v: Vec<f64> = values.filter(|x| x.is_finite()).collect();
    v.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
    v
}

pub struct Summary {
    pub total: usize,
    pub solved: usize,
    pub passed: usize,
    pub center_med_arcmin: f64,
    pub center_max_arcmin: f64,
    pub roll_max_deg: f64,
    pub fov_med_frac: f64,
    pub fov_max_frac: f64,
    pub solve_p50_ms: f64,
    pub solve_p95_ms: f64,
    pub solve_max_ms: f64,
}

impl Report {
    pub fn new(solver: &str, outcomes: Vec<Outcome>) -> Report {
        Report {
            solver: solver.to_string(),
            outcomes,
        }
    }

    pub fn summary(&self) -> Summary {
        let solved: Vec<&Outcome> =
            self.outcomes.iter().filter(|o| o.solved).collect();

        let centers = sorted_finite(solved.iter().map(|o| o.center_arcmin));
        let rolls = sorted_finite(solved.iter().map(|o| o.roll_err_deg.abs()));
        let fovs = sorted_finite(solved.iter().map(|o| o.fov_err_frac));
        let times = sorted_finite(solved.iter().map(|o| o.solve_time_ms));

        Summary {
            total: self.outcomes.len(),
            solved: solved.len(),
            passed: self.outcomes.iter().filter(|o| o.passed()).count(),
            center_med_arcmin: percentile(&centers, 0.5),
            center_max_arcmin: centers.last().copied().unwrap_or(f64::NAN),
            roll_max_deg: rolls.last().copied().unwrap_or(f64::NAN),
            fov_med_frac: percentile(&fovs, 0.5),
            fov_max_frac: fovs.last().copied().unwrap_or(f64::NAN),
            solve_p50_ms: percentile(&times, 0.5),
            solve_p95_ms: percentile(&times, 0.95),
            solve_max_ms: times.last().copied().unwrap_or(f64::NAN),
        }
    }

    /// Per-field CSV under target/e2e-report/, for diffing runs across solvers.
    pub fn write_csv(&self) -> std::io::Result<PathBuf> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../target/e2e-report");
        std::fs::create_dir_all(&dir)?;
        let dir = dir.canonicalize()?;
        let path = dir.join(format!("{}.csv", self.solver));

        let mut out = String::from(
            "name,solved,pass,center_arcmin,roll_err_deg,fov_err_frac,\
             solve_time_ms,num_matches,num_centroids\n",
        );
        for o in &self.outcomes {
            out.push_str(&format!(
                "{},{},{},{:.4},{:.4},{:.6},{:.3},{},{}\n",
                o.name,
                o.solved,
                o.passed(),
                o.center_arcmin,
                o.roll_err_deg,
                o.fov_err_frac,
                o.solve_time_ms,
                o.num_matches,
                o.num_centroids,
            ));
        }
        std::fs::write(&path, out)?;
        Ok(path)
    }

    pub fn failures(&self) -> Vec<&Outcome> {
        self.outcomes.iter().filter(|o| !o.passed()).collect()
    }
}

/// One row per solver, so two solvers can be read side by side.
pub fn print_table(reports: &[&Report]) {
    println!();
    println!(
        "{:<14} {:>7} {:>7} {:>10} {:>10} {:>8} {:>9} {:>9} {:>9} {:>9}",
        "solver",
        "solved",
        "passed",
        "cen_med'",
        "cen_max'",
        "roll_max",
        "fov_med%",
        "fov_max%",
        "t_p50ms",
        "t_p95ms"
    );
    println!("{}", "-".repeat(104));
    for r in reports {
        let s = r.summary();
        println!(
            "{:<14} {:>3}/{:<3} {:>3}/{:<3} {:>10.3} {:>10.3} \
            {:>8.3} {:>9.3} {:>9.3} {:>9.1} {:>9.1}",
            r.solver,
            s.solved,
            s.total,
            s.passed,
            s.total,
            s.center_med_arcmin,
            s.center_max_arcmin,
            s.roll_max_deg,
            s.fov_med_frac * 100.0,
            s.fov_max_frac * 100.0,
            s.solve_p50_ms,
            s.solve_p95_ms,
        );
    }
    println!();
    for r in reports {
        let s = r.summary();
        if s.solve_p95_ms.is_finite() {
            let verdict = if s.solve_p95_ms < SOLVE_P95_TARGET_MS {
                "OK"
            } else {
                "OVER"
            };
            println!(
                "  {}: solve_time p95 = {:.1} ms vs \
                {:.0} ms target [{}]  (max {:.1} ms)",
                r.solver,
                s.solve_p95_ms,
                SOLVE_P95_TARGET_MS,
                verdict,
                s.solve_max_ms
            );
        }
    }
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_use_nearest_rank() {
        let v: Vec<f64> = (1..=100).map(|i| i as f64).collect();
        assert_eq!(percentile(&v, 0.5), 50.0);
        assert_eq!(percentile(&v, 0.95), 95.0);
        assert_eq!(percentile(&v, 1.0), 100.0);
    }

    #[test]
    fn percentile_of_single_value() {
        assert_eq!(percentile(&[7.0], 0.5), 7.0);
        assert_eq!(percentile(&[7.0], 0.95), 7.0);
    }

    #[test]
    fn percentile_of_empty_is_nan() {
        assert!(percentile(&[], 0.5).is_nan());
    }

    #[test]
    fn sorted_finite_drops_nan() {
        let v = sorted_finite([3.0, f64::NAN, 1.0].into_iter());
        assert_eq!(v, vec![1.0, 3.0]);
    }
}
