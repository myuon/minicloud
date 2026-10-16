//! `platform`: what the host's process and machine have left.
//!
//! Every app runs in one process on one pool of workers, so the limits that
//! bind them all are the process's own: the systemd unit's `MemoryMax=` and
//! `CPUQuota=` (enforced by the process's cgroup), the disk the data
//! directory is on, and how many workers there are. The per-app counters
//! ([`crate::stats`], [`crate::series`]) say what each app did; this says how
//! close the whole host is to the edge.
//!
//! # What is read
//!
//! - The process's cgroup (v2): `/proc/self/cgroup` names it (`0::<path>`),
//!   and `/sys/fs/cgroup<path>/` holds `memory.current`, `memory.max`,
//!   `cpu.stat` (`usage_usec`, `nr_throttled`, `throttled_usec`) and
//!   `cpu.max` (`<quota> <period>`, or `max <period>` for no quota).
//! - `/proc/self/status`'s `VmRSS`, `/proc/loadavg`'s first figure and
//!   `/proc/meminfo`'s `MemTotal` and `MemAvailable`.
//! - `statvfs` of the data directory, and the size of the history file
//!   (`series.sqlite3` and its `-wal`).
//! - The engine's own gauges: requests in flight and queued, over every
//!   app, workers busy and workers in all.
//!
//! Every figure but the engine's is an `Option`: a file that is not there
//! (macOS, cgroup v1, a sandbox that hides it) or does not parse is `None`,
//! and the sample is taken anyway. Nothing here fails.
//!
//! The parsing is pure functions over the files' text, and the reading takes
//! the root the files are under, so the tests read fixtures from a directory
//! of their own. Only the live read ([`read_system`]) is Linux's.

use std::path::Path;

/// What the host's process and machine look like at one moment.
///
/// The CPU figures are the cgroup's running totals, as the kernel keeps
/// them; the sampler ([`crate::ops::sample_now`]) keeps what they grew by
/// over each minute. The rest are gauges: their value now.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlatformSample {
    /// `memory.current`: what the cgroup is charged for, page cache
    /// included — what `MemoryMax=` is held against.
    pub memory_bytes: Option<u64>,
    /// `memory.max`; `None` for `max` (no limit) as well as unknown.
    pub memory_max_bytes: Option<u64>,
    /// `cpu.stat`'s `usage_usec`: CPU time used since the cgroup began.
    pub cpu_usage_usec: Option<u64>,
    /// `cpu.stat`'s `throttled_usec`: time the quota held the cgroup back.
    pub cpu_throttled_usec: Option<u64>,
    /// `cpu.stat`'s `nr_throttled`: periods the quota ran out in.
    pub cpu_throttled_periods: Option<u64>,
    /// `cpu.max` as thousandths of a CPU (`CPUQuota=200%` is 2000);
    /// `None` for no quota as well as unknown.
    pub cpu_quota_millicores: Option<u64>,
    /// The process's resident set (`VmRSS`).
    pub rss_bytes: Option<u64>,
    /// The size of the disk the data directory is on.
    pub disk_total_bytes: Option<u64>,
    /// What an unprivileged process may still write on it.
    pub disk_available_bytes: Option<u64>,
    /// The host's history file and its write-ahead log.
    pub store_bytes: Option<u64>,
    /// The machine's one-minute load average, in thousandths.
    pub load1_milli: Option<u64>,
    /// `MemTotal`: the machine's memory.
    pub host_memory_total_bytes: Option<u64>,
    /// `MemAvailable`: what the machine could still give without swapping.
    pub host_memory_available_bytes: Option<u64>,
    /// Requests admitted and not answered, over every app.
    pub in_flight: u64,
    /// Requests waiting to start, over every app.
    pub queued: u64,
    /// Workers running a run at the moment of the sample.
    pub busy: u64,
    /// Workers in all.
    pub workers: u64,
}

/// The cgroup v2 path in `/proc/self/cgroup`: the `0::` line's path.
pub fn parse_cgroup_path(text: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(|path| path.trim().to_string())
        .filter(|path| path.starts_with('/'))
}

/// A file that holds one number, such as `memory.current`.
pub fn parse_number(text: &str) -> Option<u64> {
    text.trim().parse().ok()
}

/// `memory.max`: a number of bytes, or `max` for no limit (`None`).
pub fn parse_memory_max(text: &str) -> Option<u64> {
    match text.trim() {
        "max" => None,
        other => other.parse().ok(),
    }
}

/// `cpu.max` (`<quota> <period>` in microseconds) as thousandths of a CPU;
/// `None` for `max <period>`, which is no quota.
pub fn parse_cpu_max(text: &str) -> Option<u64> {
    let mut words = text.split_whitespace();
    let quota: u64 = words.next()?.parse().ok()?;
    let period: u64 = words.next()?.parse().ok()?;
    if period == 0 {
        return None;
    }
    Some(quota.saturating_mul(1000) / period)
}

/// `cpu.stat`: `usage_usec`, `throttled_usec` and `nr_throttled`, in that
/// order.
pub fn parse_cpu_stat(text: &str) -> (Option<u64>, Option<u64>, Option<u64>) {
    let field = |name: &str| {
        text.lines().find_map(|line| {
            let mut words = line.split_whitespace();
            (words.next() == Some(name))
                .then(|| words.next()?.parse().ok())
                .flatten()
        })
    };
    (
        field("usage_usec"),
        field("throttled_usec"),
        field("nr_throttled"),
    )
}

/// A `Name:   1234 kB` line of `/proc/meminfo` or `/proc/self/status`, in
/// bytes.
fn kib_field(text: &str, name: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let rest = line.strip_prefix(name)?.strip_prefix(':')?;
        let mut words = rest.split_whitespace();
        let n: u64 = words.next()?.parse().ok()?;
        match words.next() {
            Some("kB") => Some(n.saturating_mul(1024)),
            None => Some(n),
            Some(_) => None,
        }
    })
}

/// `/proc/meminfo`: `MemTotal` and `MemAvailable`, in bytes.
pub fn parse_meminfo(text: &str) -> (Option<u64>, Option<u64>) {
    (kib_field(text, "MemTotal"), kib_field(text, "MemAvailable"))
}

/// `/proc/self/status`'s `VmRSS`, in bytes.
pub fn parse_vm_rss(text: &str) -> Option<u64> {
    kib_field(text, "VmRSS")
}

/// `/proc/loadavg`'s first figure (the last minute), in thousandths.
pub fn parse_loadavg(text: &str) -> Option<u64> {
    let first = text.split_whitespace().next()?;
    let load: f64 = first.parse().ok()?;
    (load.is_finite() && load >= 0.0).then(|| (load * 1000.0).round() as u64)
}

/// The figures that come from files: the cgroup's, the process's and the
/// machine's, read under `root` (`/` for the live host, a directory of
/// fixtures in a test). The rest of the sample is left as it was.
pub fn read_files(root: &Path, into: &mut PlatformSample) {
    let read = |path: &Path| std::fs::read_to_string(path).ok();
    let proc_ = root.join("proc");
    if let Some(cgroup) = read(&proc_.join("self/cgroup")).and_then(|t| parse_cgroup_path(&t)) {
        let dir = root
            .join("sys/fs/cgroup")
            .join(cgroup.trim_start_matches('/'));
        into.memory_bytes = read(&dir.join("memory.current")).and_then(|t| parse_number(&t));
        into.memory_max_bytes = read(&dir.join("memory.max")).and_then(|t| parse_memory_max(&t));
        into.cpu_quota_millicores = read(&dir.join("cpu.max")).and_then(|t| parse_cpu_max(&t));
        if let Some(stat) = read(&dir.join("cpu.stat")) {
            let (usage, throttled, periods) = parse_cpu_stat(&stat);
            into.cpu_usage_usec = usage;
            into.cpu_throttled_usec = throttled;
            into.cpu_throttled_periods = periods;
        }
    }
    into.rss_bytes = read(&proc_.join("self/status")).and_then(|t| parse_vm_rss(&t));
    into.load1_milli = read(&proc_.join("loadavg")).and_then(|t| parse_loadavg(&t));
    if let Some(meminfo) = read(&proc_.join("meminfo")) {
        let (total, available) = parse_meminfo(&meminfo);
        into.host_memory_total_bytes = total;
        into.host_memory_available_bytes = available;
    }
}

/// The size of the disk `dir` is on and what is free on it for an
/// unprivileged process: total, then available.
#[cfg(unix)]
pub fn disk(dir: &Path) -> (Option<u64>, Option<u64>) {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else {
        return (None, None);
    };
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
    // SAFETY: `statvfs` reads a NUL-terminated path and fills the struct it
    // is given; the struct is only read when it says it did.
    let stat = unsafe {
        if libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) != 0 {
            return (None, None);
        }
        stat.assume_init()
    };
    #[allow(clippy::unnecessary_cast)] // the field types differ by platform
    let fragment = stat.f_frsize as u64;
    #[allow(clippy::unnecessary_cast)]
    let (blocks, available) = (stat.f_blocks as u64, stat.f_bavail as u64);
    (
        Some(blocks.saturating_mul(fragment)),
        Some(available.saturating_mul(fragment)),
    )
}

#[cfg(not(unix))]
pub fn disk(_dir: &Path) -> (Option<u64>, Option<u64>) {
    (None, None)
}

/// The size of the history file under `data` and its write-ahead log;
/// `None` when there is neither.
pub fn store_bytes(data: &Path) -> Option<u64> {
    let file = data.join("_host").join("series.sqlite3");
    let size = |path: &Path| std::fs::metadata(path).ok().map(|m| m.len());
    match (size(&file), size(&file.with_extension("sqlite3-wal"))) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
    }
}

/// The live figures of this process and machine, but the engine's: the
/// files under `/` on Linux, nothing elsewhere; and the disk and history
/// file of `data`, where there is one.
pub fn read_system(data: Option<&Path>) -> PlatformSample {
    let mut sample = PlatformSample::default();
    #[cfg(target_os = "linux")]
    read_files(Path::new("/"), &mut sample);
    if let Some(data) = data {
        (sample.disk_total_bytes, sample.disk_available_bytes) = disk(data);
        sample.store_bytes = store_bytes(data);
    }
    sample
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cgroup_is_the_v2_line() {
        assert_eq!(
            parse_cgroup_path("0::/system.slice/cove-tools.service\n").as_deref(),
            Some("/system.slice/cove-tools.service")
        );
        // A hybrid host lists v1 hierarchies first.
        assert_eq!(
            parse_cgroup_path("12:cpu,cpuacct:/x\n1:name=systemd:/y\n0::/\n").as_deref(),
            Some("/")
        );
        assert_eq!(parse_cgroup_path("12:cpu,cpuacct:/x\n"), None);
        assert_eq!(parse_cgroup_path(""), None);
    }

    #[test]
    fn memory_max_of_max_is_no_limit() {
        assert_eq!(parse_memory_max("max\n"), None);
        assert_eq!(parse_memory_max("2147483648\n"), Some(2 << 30));
        assert_eq!(parse_memory_max("lots"), None);
        assert_eq!(parse_number(" 1234\n"), Some(1234));
        assert_eq!(parse_number(""), None);
    }

    #[test]
    fn cpu_max_is_thousandths_of_a_cpu() {
        assert_eq!(parse_cpu_max("200000 100000\n"), Some(2000));
        assert_eq!(parse_cpu_max("50000 100000"), Some(500));
        assert_eq!(parse_cpu_max("max 100000\n"), None);
        assert_eq!(parse_cpu_max("100000 0"), None);
        assert_eq!(parse_cpu_max(""), None);
    }

    #[test]
    fn cpu_stat_names_its_fields() {
        let text = "usage_usec 123456\nuser_usec 100000\nsystem_usec 23456\n\
                    nr_periods 50\nnr_throttled 7\nthrottled_usec 8800\n";
        assert_eq!(parse_cpu_stat(text), (Some(123456), Some(8800), Some(7)));
        // A cgroup without the cpu controller has only the usage.
        assert_eq!(
            parse_cpu_stat("usage_usec 5\nuser_usec 3\n"),
            (Some(5), None, None)
        );
    }

    #[test]
    fn meminfo_and_status_are_kib() {
        let meminfo = "MemTotal:        8000000 kB\nMemFree:  100 kB\n\
                       MemAvailable:    6000000 kB\n";
        assert_eq!(
            parse_meminfo(meminfo),
            (Some(8_000_000 * 1024), Some(6_000_000 * 1024))
        );
        assert_eq!(parse_meminfo("MemTotal: 1 kB\n"), (Some(1024), None));
        let status = "Name:\tminicloud\nVmPeak:\t  900 kB\nVmRSS:\t   51200 kB\n";
        assert_eq!(parse_vm_rss(status), Some(51200 * 1024));
        assert_eq!(parse_vm_rss("Name: x\n"), None);
    }

    #[test]
    fn loadavg_is_the_first_figure_in_thousandths() {
        assert_eq!(parse_loadavg("0.52 0.40 0.31 2/345 6789\n"), Some(520));
        assert_eq!(parse_loadavg("12.005 1 1 1/1 1"), Some(12005));
        assert_eq!(parse_loadavg(""), None);
        assert_eq!(parse_loadavg("nan 1 1"), None);
    }

    #[test]
    fn the_files_are_read_under_a_root() {
        let root = std::env::temp_dir().join(format!("minicloud-platform-{}", std::process::id()));
        let cgroup = root.join("sys/fs/cgroup/system.slice/x.service");
        std::fs::create_dir_all(&cgroup).unwrap();
        std::fs::create_dir_all(root.join("proc/self")).unwrap();
        let write = |path: std::path::PathBuf, text: &str| std::fs::write(path, text).unwrap();
        write(
            root.join("proc/self/cgroup"),
            "0::/system.slice/x.service\n",
        );
        write(root.join("proc/self/status"), "VmRSS:\t 1000 kB\n");
        write(root.join("proc/loadavg"), "1.50 1 1 1/1 1\n");
        write(
            root.join("proc/meminfo"),
            "MemTotal: 4 kB\nMemAvailable: 2 kB\n",
        );
        write(cgroup.join("memory.current"), "300\n");
        write(cgroup.join("memory.max"), "max\n");
        write(cgroup.join("cpu.max"), "200000 100000\n");
        write(
            cgroup.join("cpu.stat"),
            "usage_usec 10\nnr_throttled 2\nthrottled_usec 3\n",
        );
        let mut sample = PlatformSample::default();
        read_files(&root, &mut sample);
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(
            sample,
            PlatformSample {
                memory_bytes: Some(300),
                memory_max_bytes: None,
                cpu_usage_usec: Some(10),
                cpu_throttled_usec: Some(3),
                cpu_throttled_periods: Some(2),
                cpu_quota_millicores: Some(2000),
                rss_bytes: Some(1000 * 1024),
                load1_milli: Some(1500),
                host_memory_total_bytes: Some(4096),
                host_memory_available_bytes: Some(2048),
                ..PlatformSample::default()
            }
        );
        // And a root with nothing under it is a sample of nothing.
        let mut empty = PlatformSample::default();
        read_files(&root, &mut empty);
        assert_eq!(empty, PlatformSample::default());
    }

    #[cfg(unix)]
    #[test]
    fn the_disk_of_a_directory_has_a_size() {
        let (total, available) = disk(&std::env::temp_dir());
        assert!(total.is_some_and(|t| t > 0));
        assert!(available.is_some_and(|a| a <= total.unwrap()));
        assert_eq!(disk(Path::new("/no/such/directory/at/all")), (None, None));
    }
}
