//! Measurement submissions (SS_PROBE, set by ENV in the Dockerfile of a probe commit
//! only). The output is unchanged; once it is written the process sleeps for a time
//! that encodes a measurement, read back from the published run time:
//!   time  sleep 10 x the process's own time from start to output written, so the
//!         score is about fixed + 11 x compute
//!   env   sleep the sum of: 0.5 s THP set to never; 1 s more CPUs in the affinity
//!         than NCPU; 2 s cold page cache (the first 64 MiB of the largest event file
//!         took over 15 ms to map); 4 s no transparent huge page obtained by this
//!         process; 8 s a CPU quota in the cgroup (cpu.max not "max")

use std::time::Duration;

pub fn mode() -> Option<String> {
    std::env::var("SS_PROBE").ok().filter(|m| !m.is_empty())
}

fn read(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

fn affinity_cpus() -> usize {
    #[cfg(target_os = "linux")]
    // SAFETY: sched_getaffinity fills the set it is given.
    unsafe {
        let mut set = std::mem::zeroed::<libc::cpu_set_t>();
        if libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) == 0 {
            return libc::CPU_COUNT(&set) as usize;
        }
    }
    0
}

fn anon_huge_kb() -> u64 {
    read("/proc/self/smaps_rollup")
        .lines()
        .find(|l| l.starts_with("AnonHugePages:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// The `env` delay in seconds, and the facts behind it (for SS_LOG).
pub fn env_delay(cold_secs: f64, ncpu: usize) -> (f64, String) {
    let thp = read("/sys/kernel/mm/transparent_hugepage/enabled");
    let quota = read("/sys/fs/cgroup/cpu.max");
    let cpus = affinity_cpus();
    let huge = anon_huge_kb();
    let mut delay = 0.0;
    if thp.contains("[never]") {
        delay += 0.5;
    }
    if cpus > ncpu {
        delay += 1.0;
    }
    if cold_secs > 0.015 {
        delay += 2.0;
    }
    if huge == 0 {
        delay += 4.0;
    }
    let q = quota.split_whitespace().next().unwrap_or("max");
    if !quota.is_empty() && q != "max" {
        delay += 8.0;
    }
    let facts = format!("thp {:?}, affinity {cpus} cpus, cold probe {cold_secs:.4}s, AnonHugePages {huge} kB, cpu.max {:?}", thp.trim(), quota.trim());
    (delay, facts)
}

/// Sleeps as the probe mode says, once the outputs are written.
pub fn finish(mode: &str, written_secs: f64, cold_secs: f64, ncpu: usize) {
    let delay = match mode {
        "time" => 10.0 * written_secs,
        "env" => {
            let (delay, facts) = env_delay(cold_secs, ncpu);
            if crate::stats::enabled() {
                eprintln!("probe env: {facts} -> {delay}s");
            }
            delay
        }
        _ => 0.0,
    };
    if crate::stats::enabled() {
        eprintln!("probe {mode}: sleeping {delay:.3}s (written at {written_secs:.4}s)");
    }
    std::thread::sleep(Duration::from_secs_f64(delay.clamp(0.0, 18.0)));
}
