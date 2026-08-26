// Copyright (c) 2026 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

// CPU-related metrics: temperature, system-wide and Cedar-process load
// averages (as reported in ServerInformation), and an on-demand top-like
// snapshot of instantaneous per-process/per-thread CPU usage.

use std::time::{Duration, Instant};

use log::info;

// Applies to temperature, system_load, cedar_process_load, which are polled
// implicitly on every GetFrame/ServerInformation call and need throttling. The
// on-demand top_report() below is a separate, always-fresh path.
const CACHE_INTERVAL: Duration = Duration::from_secs(60);

/// Owns the cached CPU metrics that feed into ServerInformation, plus the
/// logic for an on-demand instantaneous "top"-like report.
pub struct CpuStats {
    // Cached CPU temperature (value, last read time, high water mark), read
    // at most once per CACHE_INTERVAL.
    temperature: tokio::sync::Mutex<(f32, Instant, f32)>,

    // System-wide CPU usage: cached result (in cores), and previous
    // (busy ticks, Instant) sample.
    system_load: tokio::sync::Mutex<(f32, u64, Instant)>,

    // Cedar process CPU usage: cached result, and previous (ticks, Instant)
    // sample.
    cedar_process_load: tokio::sync::Mutex<(f32, u64, Instant)>,

    core_count: i32,
}

impl CpuStats {
    pub fn new() -> Self {
        // Timestamps in the past so the first call to each getter triggers a
        // read.
        let stale = Instant::now() - CACHE_INTERVAL * 2;
        CpuStats {
            temperature: tokio::sync::Mutex::new((0.0_f32, stale, f32::MIN)),
            system_load: tokio::sync::Mutex::new((0.0_f32, 0_u64, stale)),
            cedar_process_load: tokio::sync::Mutex::new((
                0.0_f32, 0_u64, stale,
            )),
            core_count: std::thread::available_parallelism()
                .map(|n| n.get() as i32)
                .unwrap_or(0),
        }
    }

    pub fn core_count(&self) -> i32 {
        self.core_count
    }

    /// CPU temperature in degrees Celsius, reading from sysfs at most once
    /// per minute. Logs whenever a fresh reading exceeds the previous high
    /// water mark (which also covers the very first reading, since the
    /// mark starts at f32::MIN).
    pub async fn get_temperature(&self) -> f32 {
        let mut cached = self.temperature.lock().await;
        if cached.1.elapsed() >= CACHE_INTERVAL {
            if let Ok(temp_str) = tokio::fs::read_to_string(
                "/sys/class/thermal/thermal_zone0/temp",
            )
            .await
            {
                if let Ok(temp_millideg) = temp_str.trim().parse::<f32>() {
                    cached.0 = temp_millideg / 1000.0;
                    if cached.0 > cached.2 {
                        cached.2 = cached.0;
                        info!("CPU temperature: {:.1}°C", cached.0);
                    }
                }
            }
            cached.1 = Instant::now();
        }
        cached.0
    }

    /// Total system CPU usage (in cores, e.g. 1.0 = one full core), by
    /// sampling /proc/stat's aggregate "cpu" line at most once per minute
    /// and computing delta busy ticks / delta time.
    pub async fn get_system_load(&self) -> f32 {
        let mut cached = self.system_load.lock().await;
        if cached.2.elapsed() >= CACHE_INTERVAL {
            if let Ok(stat_str) = tokio::fs::read_to_string("/proc/stat").await
            {
                if let Some(cpu_line) = stat_str.lines().next() {
                    let fields: Vec<u64> = cpu_line
                        .split_whitespace()
                        .skip(1) // Skip "cpu" label.
                        .filter_map(|f| f.parse::<u64>().ok())
                        .collect();
                    // Fields are: user nice system idle iowait irq softirq
                    // steal [guest guest_nice]. Busy time is everything
                    // except idle and iowait.
                    if fields.len() >= 5 {
                        let idle = fields[3] + fields[4];
                        let total: u64 = fields.iter().sum();
                        let busy = total.saturating_sub(idle);
                        let elapsed = cached.2.elapsed().as_secs_f32();
                        let clk_tck =
                            unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f32;
                        if clk_tck > 0.0 && elapsed > 0.0 && cached.1 > 0 {
                            cached.0 = (busy.saturating_sub(cached.1)) as f32
                                / clk_tck
                                / elapsed;
                        }
                        cached.1 = busy;
                    }
                }
            }
            cached.2 = Instant::now();
        }
        cached.0
    }

    /// Cedar process CPU usage (in cores), by sampling /proc/self/stat at
    /// most once per minute and computing delta ticks / delta time.
    pub async fn get_cedar_process_load(&self) -> f32 {
        let mut cached = self.cedar_process_load.lock().await;
        if cached.2.elapsed() >= CACHE_INTERVAL {
            if let Ok(stat_str) =
                tokio::fs::read_to_string("/proc/self/stat").await
            {
                if let Some((utime, stime)) = parse_utime_stime(&stat_str) {
                    let ticks = utime + stime;
                    let elapsed = cached.2.elapsed().as_secs_f32();
                    let clk_tck =
                        unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f32;
                    if clk_tck > 0.0 && elapsed > 0.0 && cached.1 > 0 {
                        cached.0 = (ticks.saturating_sub(cached.1)) as f32
                            / clk_tck
                            / elapsed;
                    }
                    cached.1 = ticks;
                }
            }
            cached.2 = Instant::now();
        }
        cached.0
    }

    /// Produces a human-readable, top-like snapshot of instantaneous CPU
    /// usage: the busiest processes system-wide, and a per-thread breakdown
    /// of this Cedar process's own threads (so named engine threads like
    /// serve_engine/detect_engine/solve_engine are identifiable). Takes
    /// about 1 second to run, since it samples /proc twice with a delay to
    /// compute instantaneous (not lifetime-average) CPU%.
    pub async fn top_report() -> String {
        let mut out = String::new();

        out.push_str("=== Top processes by CPU (instantaneous) ===\n");
        match sample_cpu_table(ProcGlob::AllProcesses).await {
            Ok(rows) => append_table(&mut out, &rows, 15),
            Err(e) => out.push_str(&format!("(error: {:?})\n", e)),
        }

        out.push_str("\n=== This process's threads by CPU (instantaneous) ===\n");
        match sample_cpu_table(ProcGlob::OwnThreads).await {
            Ok(rows) => append_table(&mut out, &rows, usize::MAX),
            Err(e) => out.push_str(&format!("(error: {:?})\n", e)),
        }

        out
    }
}

impl Default for CpuStats {
    fn default() -> Self {
        Self::new()
    }
}

/// Parses utime/stime (in clock ticks) out of a /proc/<pid>/stat-formatted
/// string. The process/thread name field "(name)" may itself contain spaces
/// or parens, so we skip past the last ')' rather than splitting naively.
fn parse_utime_stime(stat_str: &str) -> Option<(u64, u64)> {
    let after_comm = &stat_str[stat_str.rfind(')')? + 1..];
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    // utime/stime are fields 13/14 (1-indexed) in the full stat line, i.e.
    // indices 11/12 in `fields` (which starts right after ") <state>").
    if fields.len() > 12 {
        let utime = fields[11].parse::<u64>().ok()?;
        let stime = fields[12].parse::<u64>().ok()?;
        Some((utime, stime))
    } else {
        None
    }
}

/// Reads the process/thread name out of a /proc/<pid>/stat-formatted
/// string (the "(name)" field, which may contain spaces/parens itself).
fn parse_comm(stat_str: &str) -> Option<String> {
    let start = stat_str.find('(')? + 1;
    let end = stat_str.rfind(')')?;
    if end > start {
        Some(stat_str[start..end].to_string())
    } else {
        None
    }
}

enum ProcGlob {
    // All PIDs under /proc.
    AllProcesses,
    // All TIDs under /proc/self/task (this process's own threads).
    OwnThreads,
}

struct CpuRow {
    id: String,
    percent: f32,
    comm: String,
}

// How long to sample deltas over when computing instantaneous CPU% for the
// top-like report.
const REPORT_SAMPLE_INTERVAL: Duration = Duration::from_millis(1000);

/// Takes two /proc samples 1 second apart and returns per-id (pid or tid)
/// instantaneous CPU%, sorted highest first.
async fn sample_cpu_table(
    glob: ProcGlob,
) -> std::io::Result<Vec<CpuRow>> {
    let before = read_all_ticks(&glob).await?;
    tokio::time::sleep(REPORT_SAMPLE_INTERVAL).await;
    let after = read_all_ticks(&glob).await?;

    let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f32;
    let elapsed_secs = REPORT_SAMPLE_INTERVAL.as_secs_f32();

    let mut rows: Vec<CpuRow> = Vec::new();
    for (id, (ticks, comm)) in after {
        let Some((prev_ticks, _)) = before.get(&id) else {
            continue;
        };
        let delta = ticks.saturating_sub(*prev_ticks);
        let percent = if clk_tck > 0.0 && elapsed_secs > 0.0 {
            (delta as f32 / clk_tck / elapsed_secs) * 100.0
        } else {
            0.0
        };
        rows.push(CpuRow { id, percent, comm });
    }
    rows.sort_by(|a, b| {
        b.percent.partial_cmp(&a.percent).unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(rows)
}

/// Reads (utime+stime ticks, comm) for every id matched by `glob`.
async fn read_all_ticks(
    glob: &ProcGlob,
) -> std::io::Result<std::collections::HashMap<String, (u64, String)>> {
    let mut result = std::collections::HashMap::new();
    let mut dir = match glob {
        ProcGlob::AllProcesses => tokio::fs::read_dir("/proc").await?,
        ProcGlob::OwnThreads => {
            tokio::fs::read_dir("/proc/self/task").await?
        }
    };
    while let Some(entry) = dir.next_entry().await? {
        let name = entry.file_name();
        let Some(id) = name.to_str() else { continue };
        // /proc contains non-numeric entries (self, cpuinfo, etc.) alongside
        // pid directories; only numeric entries are processes/threads.
        if !id.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let stat_path = match glob {
            ProcGlob::AllProcesses => format!("/proc/{}/stat", id),
            ProcGlob::OwnThreads => {
                format!("/proc/self/task/{}/stat", id)
            }
        };
        let Ok(stat_str) = tokio::fs::read_to_string(&stat_path).await else {
            // Process/thread may have exited between listing and reading.
            continue;
        };
        let Some((utime, stime)) = parse_utime_stime(&stat_str) else {
            continue;
        };
        let comm = parse_comm(&stat_str).unwrap_or_else(|| "?".to_string());
        result.insert(id.to_string(), (utime + stime, comm));
    }
    Ok(result)
}

fn append_table(out: &mut String, rows: &[CpuRow], max_rows: usize) {
    out.push_str(&format!("{:<8} {:>7}  {}\n", "PID/TID", "%CPU", "COMMAND"));
    for row in rows.iter().take(max_rows) {
        out.push_str(&format!(
            "{:<8} {:>6.1}%  {}\n",
            row.id, row.percent, row.comm
        ));
    }
}
