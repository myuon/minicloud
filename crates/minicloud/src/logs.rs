//! Each app's recent log lines, kept in memory.
//!
//! `log.*` lines and the host's own lines about the app (a request that
//! failed, a run that would not yield) go into a ring of the last
//! [`LOG_LINES`], which `GET /_host/apps/<app>/logs` answers. Standard output
//! still gets the app's `log.*` lines unless the host runs `--quiet`.
//!
//! With a data directory, every line is also appended to
//! `<data>/<app>/log.txt`, rotated to `log.1.txt` past [`ROTATE_BYTES`] (one
//! old file is kept). The appends are made by one writer thread, fed by a
//! channel, so a worker or the I/O runtime never waits on a file.
//!
//! [`LogRing::follow`] is the same lines as they are written, for
//! `GET /_host/apps/<app>/logs?follow=1` and `minicloud logs --follow`.

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// A log file is rotated when it grows past this.
pub const ROTATE_BYTES: u64 = 1 << 20;

/// How many lines each app's ring keeps.
pub const LOG_LINES: usize = 1000;

/// The longest line kept; longer lines are cut, saying so.
const MAX_LINE: usize = 4096;

/// How many lines a follower may fall behind before it is told it missed
/// some. A reader that cannot keep up with a thousand lines is not reading.
pub const FOLLOW_BACKLOG: usize = 1024;

/// One app's recent lines.
pub struct LogRing {
    lines: Mutex<VecDeque<String>>,
    /// Where the lines are also written, once a data directory is known.
    file: OnceLock<PathBuf>,
    /// Every line, as it is written, to whoever is following
    /// ([`LogRing::follow`]). A broadcast with no receiver costs a failed
    /// send, which is why nothing has to know whether anyone is watching.
    live: tokio::sync::broadcast::Sender<String>,
}

impl Default for LogRing {
    fn default() -> LogRing {
        LogRing {
            lines: Mutex::new(VecDeque::new()),
            file: OnceLock::new(),
            live: tokio::sync::broadcast::channel(FOLLOW_BACKLOG).0,
        }
    }
}

impl LogRing {
    /// Every line written from now on.
    ///
    /// The receiver is told when it has fallen more than [`FOLLOW_BACKLOG`]
    /// lines behind rather than quietly skipping them, so a follower can say
    /// so instead of showing a log with a hole in it.
    pub fn follow(&self) -> tokio::sync::broadcast::Receiver<String> {
        self.live.subscribe()
    }

    /// Appends `line` at `level` (`info`, `warn`, `error`, or `host` for the
    /// host's own), stamped with the wall clock.
    pub fn push(&self, level: &str, line: &str) {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_millis());
        let mut text = if line.len() > MAX_LINE {
            let mut end = MAX_LINE;
            while !line.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}… ({} bytes)", &line[..end], line.len())
        } else {
            line.to_string()
        };
        text = text.replace('\n', "\\n");
        let entry = format!("{}.{:03} {level}: {text}", millis / 1000, millis % 1000);
        if let Some(file) = self.file.get() {
            let _ = writer().send((file.clone(), entry.clone()));
        }
        let _ = self.live.send(entry.clone());
        let mut lines = self.lines.lock().unwrap();
        if lines.len() == LOG_LINES {
            lines.pop_front();
        }
        lines.push_back(entry);
    }

    /// Writes every line from now on to `file` too. The first call wins.
    pub fn attach(&self, file: PathBuf) {
        let _ = self.file.set(file);
    }

    /// The last `n` lines, oldest first, one per line.
    pub fn tail(&self, n: usize) -> String {
        let lines = self.lines.lock().unwrap();
        let skip = lines.len().saturating_sub(n);
        let mut out = String::new();
        for line in lines.iter().skip(skip) {
            out.push_str(line);
            out.push('\n');
        }
        out
    }

    /// The last `n` lines, oldest first, each split into its parts.
    pub fn tail_lines(&self, n: usize) -> Vec<Line> {
        let lines = self.lines.lock().unwrap();
        let skip = lines.len().saturating_sub(n);
        lines.iter().skip(skip).map(|l| Line::parse(l)).collect()
    }
}

/// One line of an app's log: when it was written, at what level, and what it
/// said.
pub struct Line {
    /// Milliseconds since the epoch; 0 for a line this module cannot read.
    pub at_ms: u64,
    pub level: String,
    pub text: String,
}

impl Line {
    /// Reads a line as [`LogRing::push`] writes it, `<secs>.<millis> <level>:
    /// <text>`; one of any other shape is all `text`, with no time or level.
    fn parse(entry: &str) -> Line {
        let parsed = (|| {
            let (stamp, rest) = entry.split_once(' ')?;
            let (secs, millis) = stamp.split_once('.')?;
            let (level, text) = rest.split_once(": ")?;
            if millis.len() != 3 || level.is_empty() || level.contains(char::is_whitespace) {
                return None;
            }
            let at_ms =
                secs.parse::<u64>().ok()?.checked_mul(1000)? + millis.parse::<u64>().ok()?;
            Some(Line {
                at_ms,
                level: level.to_string(),
                text: text.to_string(),
            })
        })();
        parsed.unwrap_or_else(|| Line {
            at_ms: 0,
            level: String::new(),
            text: entry.to_string(),
        })
    }
}

/// The one thread that appends to log files.
fn writer() -> &'static Sender<(PathBuf, String)> {
    static WRITER: OnceLock<Sender<(PathBuf, String)>> = OnceLock::new();
    WRITER.get_or_init(|| {
        let (send, receive) = channel::<(PathBuf, String)>();
        std::thread::Builder::new()
            .name("minicloud-log".into())
            .spawn(move || {
                let mut open: HashMap<PathBuf, (std::fs::File, u64)> = HashMap::new();
                for (path, line) in receive {
                    let _ = append(&mut open, &path, &line);
                }
            })
            .expect("the log writer starts");
        send
    })
}

fn append(
    open: &mut HashMap<PathBuf, (std::fs::File, u64)>,
    path: &PathBuf,
    line: &str,
) -> std::io::Result<()> {
    if !open.contains_key(path) {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        let size = file.metadata()?.len();
        open.insert(path.clone(), (file, size));
    }
    let (file, size) = open.get_mut(path).expect("just opened");
    writeln!(file, "{line}")?;
    *size += line.len() as u64 + 1;
    if *size > ROTATE_BYTES {
        open.remove(path);
        std::fs::rename(path, path.with_extension("1.txt"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_keeps_the_last_lines() {
        let ring = LogRing::default();
        for i in 0..LOG_LINES + 5 {
            ring.push("info", &format!("line {i}"));
        }
        let tail = ring.tail(2);
        let lines: Vec<&str> = tail.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[1].ends_with(&format!("info: line {}", LOG_LINES + 4)));
        assert_eq!(ring.tail(usize::MAX).lines().count(), LOG_LINES);
    }

    #[test]
    fn lines_split_into_time_level_and_text() {
        let ring = LogRing::default();
        ring.push("warn", "disk: nearly full");
        ring.push("info", "second");
        ring.push("info", "third");
        let lines = ring.tail_lines(2);
        assert_eq!(lines.len(), 2);
        assert_eq!(
            (lines[0].level.as_str(), lines[0].text.as_str()),
            ("info", "second")
        );
        assert!(lines[0].at_ms > 1_000_000_000_000);
        let all = ring.tail_lines(usize::MAX);
        assert_eq!(all[0].level, "warn");
        assert_eq!(all[0].text, "disk: nearly full");
        let line = Line::parse("1700000000.042 host: hello");
        assert_eq!(
            (line.at_ms, line.level.as_str(), line.text.as_str()),
            (1_700_000_000_042, "host", "hello")
        );
    }

    #[test]
    fn a_line_of_another_shape_is_all_text() {
        for broken in [
            "",
            "no stamp here",
            "12.5 info: short millis",
            "x.123 info: nan",
            "1.123 info no colon",
        ] {
            let line = Line::parse(broken);
            assert_eq!(
                (line.at_ms, line.level.as_str(), line.text.as_str()),
                (0, "", broken)
            );
        }
    }

    #[test]
    fn an_attached_ring_writes_its_file_and_rotates_it() {
        let dir = std::env::temp_dir().join(format!("minicloud-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("log.txt");
        let ring = LogRing::default();
        ring.attach(path.clone());
        let line = "x".repeat(1000);
        for _ in 0..1100 {
            ring.push("info", &line);
        }
        ring.push("info", "the last line");
        let until = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !std::fs::read_to_string(&path).is_ok_and(|text| text.contains("the last line")) {
            assert!(std::time::Instant::now() < until, "the writer never wrote");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(dir.join("log.1.txt").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
