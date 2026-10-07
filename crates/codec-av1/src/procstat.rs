//! Process CPU and memory, from `/proc` (Linux), for the Stage 17A measurements: cheap enough to
//! sample every second and needs no dependency. It answers one question - *is the machine
//! actually saturated when the frame rate falls?* - so it reports the whole process (rav1e does
//! its work on its own thread pool, not just on the `av1-encoder` thread).

use crate::unix_us;

/// Kernel clock ticks per second (`_SC_CLK_TCK`), 100 on every mainstream Linux.
const CLK_TCK: u64 = 100;

/// One reading. CPU time is cumulative; take two readings and use [`cpu_percent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProcSample {
    /// User + system CPU time of all threads of this process, microseconds.
    pub cpu_us: u64,
    /// Resident memory, bytes.
    pub rss_bytes: u64,
    pub threads: u32,
    /// Wall clock of the reading ([`unix_us`]).
    pub at_unix_us: u64,
}

/// Read this process's usage now (`None` off Linux or if `/proc` is unreadable).
pub fn sample() -> Option<ProcSample> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // Fields after the parenthesised command name (which may itself contain spaces/parens).
    let rest = stat.rsplit_once(')')?.1.trim_start();
    let f: Vec<&str> = rest.split_whitespace().collect();
    // state ppid pgrp session tty_nr tpgid flags minflt cminflt majflt cmajflt utime stime ...
    // num_threads
    let utime: u64 = f.get(11)?.parse().ok()?;
    let stime: u64 = f.get(12)?.parse().ok()?;
    let threads: u32 = f.get(17)?.parse().ok()?;
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let rss_kb: u64 = status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    Some(ProcSample {
        cpu_us: (utime + stime) * 1_000_000 / CLK_TCK,
        rss_bytes: rss_kb * 1024,
        threads,
        at_unix_us: unix_us(),
    })
}

/// CPU used between two readings as a percentage of ONE core: 100 = one core fully busy, 400 =
/// four cores. (A multi-threaded encoder can exceed 100.)
pub fn cpu_percent(from: &ProcSample, to: &ProcSample) -> f64 {
    let wall = to.at_unix_us.saturating_sub(from.at_unix_us);
    if wall == 0 {
        return 0.0;
    }
    to.cpu_us.saturating_sub(from.cpu_us) as f64 * 100.0 / wall as f64
}

/// Logical CPUs available to this process.
pub fn cpu_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reading_is_plausible_and_busy_work_shows_as_cpu() {
        let a = sample().expect("/proc is readable on Linux");
        assert!(a.rss_bytes > 1_000_000 && a.threads >= 1, "{a:?}");
        let start = std::time::Instant::now();
        let mut x = 0u64;
        while start.elapsed() < std::time::Duration::from_millis(300) {
            x = x.wrapping_add(std::hint::black_box(1));
        }
        assert!(x > 0);
        let b = sample().unwrap();
        let pct = cpu_percent(&a, &b);
        assert!(
            pct > 40.0 && pct < 400.0 * cpu_count() as f64,
            "busy loop read as {pct}%"
        );
        assert_eq!(cpu_percent(&a, &a), 0.0);
        assert!(cpu_count() >= 1);
    }
}
