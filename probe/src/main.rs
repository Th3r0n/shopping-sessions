//! Measurement submissions only, never a final candidate (Dockerfile of a probe commit).
//!
//! Runs two pipeline variants, A and B, alternately (ABBA...) on the real input, each
//! writing to its own folder under SCRATCH_DIR, and keeps the best wall time of each.
//! Then it runs A for real into OUTPUT_DIR, so the submission is still checked, and
//! sleeps until BASE + SCALE x (best A - best B) seconds after its own start, so that
//! the published run time encodes the difference: A - B = (score - BASE - fixed) / SCALE.
//!
//! Env: AB_A, AB_B (binaries; /pipeline-a, /pipeline-b), AB_A_ENV, AB_B_ENV ("K=V,K=V"),
//! AB_ROUNDS (3), AB_BASE (10), AB_SCALE (100).
use std::process::Command;
use std::time::{Duration, Instant};

fn var(name: &str, default: &str) -> String {
    std::env::var(name).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| default.to_string())
}

fn pairs(spec: &str) -> Vec<(String, String)> {
    spec.split(',')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

/// Wall seconds of one run of `bin` writing to `out`, and whether it succeeded.
fn run(bin: &str, env: &[(String, String)], out: &str) -> (f64, bool) {
    let _ = std::fs::create_dir_all(out);
    let started = Instant::now();
    let status = Command::new(bin).env("OUTPUT_DIR", out).envs(env.iter().cloned()).status();
    (started.elapsed().as_secs_f64(), matches!(status, Ok(s) if s.success()))
}

fn main() {
    let started = Instant::now();
    let (a, b) = (var("AB_A", "/pipeline-a"), var("AB_B", "/pipeline-b"));
    let (ea, eb) = (pairs(&var("AB_A_ENV", "")), pairs(&var("AB_B_ENV", "")));
    let rounds: usize = var("AB_ROUNDS", "3").parse().unwrap_or(3);
    let base: f64 = var("AB_BASE", "10").parse().unwrap_or(10.0);
    let scale: f64 = var("AB_SCALE", "100").parse().unwrap_or(100.0);
    let scratch = var("SCRATCH_DIR", "/scratch");
    let output = var("OUTPUT_DIR", "/output");

    let mut best = [f64::MAX; 2];
    let mut ok = true;
    for r in 0..rounds {
        let order = if r % 2 == 0 { [0, 1] } else { [1, 0] };
        for v in order {
            let (bin, env) = if v == 0 { (&a, &ea) } else { (&b, &eb) };
            let (secs, good) = run(bin, env, &format!("{scratch}/ab-{v}-{r}"));
            ok &= good;
            best[v] = best[v].min(secs);
            eprintln!("probe: round {r} variant {} {secs:.4}s", if v == 0 { 'A' } else { 'B' });
        }
    }
    let (_, good) = run(&a, &ea, &output);
    let diff = best[0] - best[1];
    let target = if ok { base + scale * diff } else { 1.0 };
    let target = target.clamp(1.0, 19.0);
    eprintln!("probe: best A {:.4}s, best B {:.4}s, A-B {:+.4}s, ending at {target:.3}s", best[0], best[1], diff);
    if let Some(wait) = Duration::from_secs_f64(target).checked_sub(started.elapsed()) {
        std::thread::sleep(wait);
    }
    std::process::exit(if good { 0 } else { 1 });
}
