//! `series`: the host's counters over time.
//!
//! The counters the host keeps are running totals, so they answer "how many
//! so far" and nothing else: yesterday's requests per second, or the moment
//! errors started to climb, are gone. This store keeps the *increase* per app
//! per minute, so a graph or an alert can ask for any stretch of the past.
//!
//! # Storage
//!
//! SQLite (bundled, through `rusqlite`), like the per-app stores: one file, a
//! format that outlives this program, WAL mode with `synchronous = NORMAL`. A
//! committed write survives the host process crashing; a power loss may lose
//! the last transactions but never corrupts the file. Losing a minute of a
//! graph is a fair price for not waiting on a disk flush every minute.
//!
//! # Two resolutions
//!
//! Minutes are kept for [`KEEP_MINUTES_SECS`], then only the hour they
//! belong to remains, kept for [`KEEP_HOURS_SECS`]. That bounds the file: an
//! app costs 2880 + 2160 rows however long it runs.
//!
//! # The host's own rows
//!
//! Beside the apps' rows, `host_minute` and `host_hour` keep the host's
//! process and machine ([`crate::platform`]): one [`HostRow`] a minute, rolled
//! up and pruned with the apps' rows and kept as long. A row's CPU figures
//! are what the cgroup's counters grew by over its span; everything else is
//! a gauge read at the end of it. How an hour is made of its minutes is
//! [`HOST_COLUMNS`]: the CPU time is summed, memory and the engine's gauges
//! take the worst (largest) minute, free disk and free memory the worst
//! (smallest), and the load average the mean.
//!
//! Error messages never carry the file path: they reach HTTP responses and
//! logs that outsiders may read, and the data directory's layout is not
//! theirs to learn.

use std::path::Path;

use rusqlite::{params, params_from_iter, Connection};

/// How long minute rows are kept by default: two days.
pub const KEEP_MINUTES_SECS: i64 = 48 * 3600;

/// How long hour rows are kept by default: ninety days.
pub const KEEP_HOURS_SECS: i64 = 90 * 24 * 3600;

const MINUTE: i64 = 60;
const HOUR: i64 = 3600;

/// What one app did in one minute: the increase over the minute, not the
/// running total.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sample {
    pub app: String,
    /// The minute this covers, as seconds since the epoch, rounded down to
    /// a multiple of 60.
    pub at_unix: i64,
    pub served: u64,
    pub ok: u64,
    pub errors: u64,
    pub rejected: u64,
    /// Milliseconds workers spent on this app's code.
    pub worker_ms: u64,
    /// Microseconds, estimated from the request-time histogram.
    pub p50_us: u64,
    pub p99_us: u64,
}

/// What the host's process and machine looked like over one minute or one
/// hour ([`crate::platform::PlatformSample`] as the history keeps it).
///
/// A figure the host could not read is `None` (a `NULL` in the store), which
/// is not zero: a host on macOS has no cgroup, and a graph should say so
/// rather than draw a floor.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HostRow {
    /// The minute (or hour) this covers, as seconds since the epoch.
    pub at_unix: i64,
    pub memory_bytes: Option<u64>,
    pub memory_max_bytes: Option<u64>,
    pub rss_bytes: Option<u64>,
    /// CPU time the cgroup used over the span, in microseconds.
    pub cpu_usec: Option<u64>,
    /// Time the quota held the cgroup back over the span, in microseconds.
    pub cpu_throttled_usec: Option<u64>,
    /// Periods the quota ran out in over the span.
    pub cpu_throttled_periods: Option<u64>,
    pub cpu_quota_millicores: Option<u64>,
    pub disk_total_bytes: Option<u64>,
    pub disk_available_bytes: Option<u64>,
    pub store_bytes: Option<u64>,
    pub load1_milli: Option<u64>,
    pub host_memory_total_bytes: Option<u64>,
    pub host_memory_available_bytes: Option<u64>,
    pub in_flight: u64,
    pub queued: u64,
    pub busy: u64,
    pub workers: u64,
}

/// The host rows' columns after `at`, in [`HostRow`]'s order, each with how
/// an hour is made of its minutes. SQLite's aggregates skip `NULL`, so an
/// hour with some unknown minutes is made of the known ones, and an hour of
/// none is `NULL` again.
pub const HOST_COLUMNS: [(&str, &str); 17] = [
    ("memory_bytes", "MAX"),
    ("memory_max_bytes", "MAX"),
    ("rss_bytes", "MAX"),
    ("cpu_usec", "SUM"),
    ("cpu_throttled_usec", "SUM"),
    ("cpu_throttled_periods", "SUM"),
    ("cpu_quota_millicores", "MAX"),
    ("disk_total_bytes", "MAX"),
    ("disk_available_bytes", "MIN"),
    ("store_bytes", "MAX"),
    ("load1_milli", "AVG"),
    ("host_memory_total_bytes", "MAX"),
    ("host_memory_available_bytes", "MIN"),
    ("in_flight", "MAX"),
    ("queued", "MAX"),
    ("busy", "MAX"),
    ("workers", "MAX"),
];

/// The four engine gauges, which are always known.
const HOST_NOT_NULL: [&str; 4] = ["in_flight", "queued", "busy", "workers"];

impl HostRow {
    /// The values after `at`, in [`HOST_COLUMNS`]' order.
    fn values(&self) -> [Option<u64>; 17] {
        [
            self.memory_bytes,
            self.memory_max_bytes,
            self.rss_bytes,
            self.cpu_usec,
            self.cpu_throttled_usec,
            self.cpu_throttled_periods,
            self.cpu_quota_millicores,
            self.disk_total_bytes,
            self.disk_available_bytes,
            self.store_bytes,
            self.load1_milli,
            self.host_memory_total_bytes,
            self.host_memory_available_bytes,
            Some(self.in_flight),
            Some(self.queued),
            Some(self.busy),
            Some(self.workers),
        ]
    }

    fn from_values(at_unix: i64, v: [Option<u64>; 17]) -> HostRow {
        HostRow {
            at_unix,
            memory_bytes: v[0],
            memory_max_bytes: v[1],
            rss_bytes: v[2],
            cpu_usec: v[3],
            cpu_throttled_usec: v[4],
            cpu_throttled_periods: v[5],
            cpu_quota_millicores: v[6],
            disk_total_bytes: v[7],
            disk_available_bytes: v[8],
            store_bytes: v[9],
            load1_milli: v[10],
            host_memory_total_bytes: v[11],
            host_memory_available_bytes: v[12],
            in_flight: v[13].unwrap_or(0),
            queued: v[14].unwrap_or(0),
            busy: v[15].unwrap_or(0),
            workers: v[16].unwrap_or(0),
        }
    }
}

/// How far back `over` reaches, in seconds, and whether the hour table is the
/// one to read it from: `"day"` is a day of minutes, `"month"` thirty days of
/// hours, and anything else an hour of minutes.
///
/// A window and not a row count, because the two tables differ in what a row
/// is. The HTTP read ([`crate::ops::series`]) and the admin module's
/// `host.series` both ask here, so "a day" cannot come to mean one thing on
/// the operations page and another on the admin app's graph; and an `over`
/// nobody knows falls back to the hour in both, rather than being an error
/// in one and a guess in the other.
pub fn window(over: &str) -> (i64, bool) {
    match over {
        "day" => (24 * HOUR, false),
        "month" => (30 * 24 * HOUR, true),
        _ => (HOUR, false),
    }
}

/// The time series store.
pub struct Series {
    conn: Connection,
}

const COLUMNS: &str = "app, at, served, ok, errors, rejected, worker_ms, p50_us, p99_us";

fn fail(e: rusqlite::Error) -> String {
    e.to_string()
}

/// SQLite integers are signed; counters beyond `i64::MAX` are clamped rather
/// than wrapped into negative numbers.
fn to_sql(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn floor_to(t: i64, step: i64) -> i64 {
    t.div_euclid(step) * step
}

impl Series {
    /// Opens (and creates) the file.
    pub fn open(path: &Path) -> Result<Series, String> {
        // The host's own directory may not be there yet: the first start of
        // a host whose data directory is new makes it here, as the apps'
        // stores make theirs.
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("cannot create the host's directory: {e}"))?;
        }
        let conn =
            Connection::open(path).map_err(|_| "cannot open the series store".to_string())?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(fail)?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(fail)?;
        Series::with(conn)
    }

    /// A store that lives and dies with the process, for a host with no data
    /// directory, which keeps nothing across restarts.
    pub fn in_memory() -> Result<Series, String> {
        Series::with(Connection::open_in_memory().map_err(fail)?)
    }

    fn with(conn: Connection) -> Result<Series, String> {
        for table in ["minute", "hour"] {
            conn.execute_batch(&format!(
                "CREATE TABLE IF NOT EXISTS {table} (
                    app TEXT NOT NULL,
                    at INTEGER NOT NULL,
                    served INTEGER NOT NULL,
                    ok INTEGER NOT NULL,
                    errors INTEGER NOT NULL,
                    rejected INTEGER NOT NULL,
                    worker_ms INTEGER NOT NULL,
                    p50_us INTEGER NOT NULL,
                    p99_us INTEGER NOT NULL,
                    PRIMARY KEY (app, at)
                ) WITHOUT ROWID;"
            ))
            .map_err(fail)?;
        }
        for table in ["host_minute", "host_hour"] {
            let columns: Vec<String> = HOST_COLUMNS
                .iter()
                .map(|(name, _)| {
                    let null = if HOST_NOT_NULL.contains(name) {
                        " NOT NULL"
                    } else {
                        ""
                    };
                    format!("{name} INTEGER{null}")
                })
                .collect();
            conn.execute_batch(&format!(
                "CREATE TABLE IF NOT EXISTS {table} (
                    at INTEGER PRIMARY KEY NOT NULL,
                    {}
                );",
                columns.join(",\n")
            ))
            .map_err(fail)?;
        }
        Ok(Series { conn })
    }

    /// Writes the host's row for one minute, replacing one already there.
    pub fn record_host(&self, row: &HostRow) -> Result<(), String> {
        let names: Vec<&str> = HOST_COLUMNS.iter().map(|(name, _)| *name).collect();
        let holes: Vec<String> = (2..=HOST_COLUMNS.len() + 1)
            .map(|i| format!("?{i}"))
            .collect();
        let updates: Vec<String> = names
            .iter()
            .map(|name| format!("{name} = excluded.{name}"))
            .collect();
        let sql = format!(
            "INSERT INTO host_minute (at, {}) VALUES (?1, {})
             ON CONFLICT(at) DO UPDATE SET {}",
            names.join(", "),
            holes.join(", "),
            updates.join(", ")
        );
        let mut args: Vec<rusqlite::types::Value> = vec![floor_to(row.at_unix, MINUTE).into()];
        for value in row.values() {
            args.push(match value {
                Some(n) => to_sql(n).into(),
                None => rusqlite::types::Value::Null,
            });
        }
        self.conn
            .prepare_cached(&sql)
            .map_err(fail)?
            .execute(params_from_iter(args))
            .map_err(fail)?;
        Ok(())
    }

    /// The host's minutes from `since_unix` onwards, oldest first.
    pub fn host_minutes(&self, since_unix: i64) -> Result<Vec<HostRow>, String> {
        self.read_host("host_minute", since_unix)
    }

    /// The same from the hour table.
    pub fn host_hours(&self, since_unix: i64) -> Result<Vec<HostRow>, String> {
        self.read_host("host_hour", since_unix)
    }

    fn read_host(&self, table: &str, since: i64) -> Result<Vec<HostRow>, String> {
        let names: Vec<&str> = HOST_COLUMNS.iter().map(|(name, _)| *name).collect();
        let sql = format!(
            "SELECT at, {} FROM {table} WHERE at >= ?1 ORDER BY at",
            names.join(", ")
        );
        let mut statement = self.conn.prepare_cached(&sql).map_err(fail)?;
        let rows = statement
            .query_map(params![since], |row| {
                let mut values = [None; 17];
                for (i, value) in values.iter_mut().enumerate() {
                    *value = row.get::<_, Option<i64>>(i + 1)?.map(|v| v.max(0) as u64);
                }
                Ok(HostRow::from_values(row.get(0)?, values))
            })
            .map_err(fail)?;
        rows.collect::<Result<_, _>>().map_err(fail)
    }

    /// How many rows the host's two tables hold: minutes, then hours.
    pub fn host_rows(&self) -> Result<(u64, u64), String> {
        Ok((self.count("host_minute")?, self.count("host_hour")?))
    }

    fn count(&self, table: &str) -> Result<u64, String> {
        self.conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| {
                r.get::<_, i64>(0)
            })
            .map(|n| n as u64)
            .map_err(fail)
    }

    /// Writes one minute's samples. A minute already written for an app is
    /// replaced, so a second write of the same minute is not a double
    /// count. All of the samples land together or not at all.
    pub fn record(&self, samples: &[Sample]) -> Result<(), String> {
        let tx = self.conn.unchecked_transaction().map_err(fail)?;
        {
            let mut insert = tx
                .prepare_cached(
                    "INSERT INTO minute (app, at, served, ok, errors, rejected, worker_ms, p50_us, p99_us)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                     ON CONFLICT(app, at) DO UPDATE SET
                        served = excluded.served, ok = excluded.ok,
                        errors = excluded.errors, rejected = excluded.rejected,
                        worker_ms = excluded.worker_ms,
                        p50_us = excluded.p50_us, p99_us = excluded.p99_us",
                )
                .map_err(fail)?;
            for s in samples {
                insert
                    .execute(params![
                        s.app,
                        floor_to(s.at_unix, MINUTE),
                        to_sql(s.served),
                        to_sql(s.ok),
                        to_sql(s.errors),
                        to_sql(s.rejected),
                        to_sql(s.worker_ms),
                        to_sql(s.p50_us),
                        to_sql(s.p99_us),
                    ])
                    .map_err(fail)?;
            }
        }
        tx.commit().map_err(fail)
    }

    /// Rolls the minutes of every hour that has ended into the hour table,
    /// then drops minutes older than `keep_minutes_secs` and hours older
    /// than `keep_hours_secs`. Answers how many rows it dropped.
    ///
    /// The roll-up is a replace, not an add, so sweeping twice never counts
    /// an hour twice. For that to stay true a minute row is only dropped
    /// once its whole hour is past the retention, because rolling up an hour
    /// from a part of its minutes would overwrite the full figure with a
    /// smaller one. Minutes are therefore kept up to an hour longer than
    /// asked.
    pub fn sweep(
        &self,
        now_unix: i64,
        keep_minutes_secs: i64,
        keep_hours_secs: i64,
    ) -> Result<u64, String> {
        let this_hour = floor_to(now_unix, HOUR);
        let minutes_before = floor_to(now_unix.saturating_sub(keep_minutes_secs), HOUR);
        let hours_before = now_unix.saturating_sub(keep_hours_secs);

        let tx = self.conn.unchecked_transaction().map_err(fail)?;
        // Counts add up over the hour. The percentiles take the maximum: the
        // worst minute is what an operator looking at an hour wants to see,
        // and an average of percentiles would smooth a bad minute into
        // nothing.
        tx.execute(
            "INSERT INTO hour (app, at, served, ok, errors, rejected, worker_ms, p50_us, p99_us)
             SELECT app, at / 3600 * 3600, SUM(served), SUM(ok), SUM(errors),
                    SUM(rejected), SUM(worker_ms), MAX(p50_us), MAX(p99_us)
             FROM minute WHERE at < ?1
             GROUP BY app, at / 3600 * 3600
             ON CONFLICT(app, at) DO UPDATE SET
                served = excluded.served, ok = excluded.ok,
                errors = excluded.errors, rejected = excluded.rejected,
                worker_ms = excluded.worker_ms,
                p50_us = excluded.p50_us, p99_us = excluded.p99_us",
            params![this_hour],
        )
        .map_err(fail)?;
        // The host's rows, by [`HOST_COLUMNS`]: the mean of the load is the
        // one aggregate that is not a whole number, and is rounded to one.
        let names: Vec<&str> = HOST_COLUMNS.iter().map(|(name, _)| *name).collect();
        let rolled: Vec<String> = HOST_COLUMNS
            .iter()
            .map(|(name, how)| match *how {
                "AVG" => format!("CAST(ROUND(AVG({name})) AS INTEGER)"),
                how => format!("{how}({name})"),
            })
            .collect();
        let updates: Vec<String> = names
            .iter()
            .map(|name| format!("{name} = excluded.{name}"))
            .collect();
        tx.execute(
            &format!(
                "INSERT INTO host_hour (at, {})
                 SELECT at / 3600 * 3600, {} FROM host_minute WHERE at < ?1
                 GROUP BY at / 3600 * 3600
                 ON CONFLICT(at) DO UPDATE SET {}",
                names.join(", "),
                rolled.join(", "),
                updates.join(", ")
            ),
            params![this_hour],
        )
        .map_err(fail)?;
        let mut dropped = 0;
        for (table, before) in [
            ("minute", minutes_before),
            ("hour", hours_before),
            ("host_minute", minutes_before),
            ("host_hour", hours_before),
        ] {
            dropped += tx
                .execute(
                    &format!("DELETE FROM {table} WHERE at < ?1"),
                    params![before],
                )
                .map_err(fail)?;
        }
        tx.commit().map_err(fail)?;
        Ok(dropped as u64)
    }

    /// The minutes of `app` (or every app, for `None`) from `since_unix`
    /// onwards, oldest first.
    pub fn minutes(&self, app: Option<&str>, since_unix: i64) -> Result<Vec<Sample>, String> {
        self.read("minute", app, since_unix)
    }

    /// The same from the hour table.
    pub fn hours(&self, app: Option<&str>, since_unix: i64) -> Result<Vec<Sample>, String> {
        self.read("hour", app, since_unix)
    }

    fn read(&self, table: &str, app: Option<&str>, since: i64) -> Result<Vec<Sample>, String> {
        let mut sql = format!("SELECT {COLUMNS} FROM {table} WHERE at >= ?1");
        let mut args: Vec<rusqlite::types::Value> = vec![since.into()];
        if let Some(app) = app {
            sql.push_str(" AND app = ?2");
            args.push(app.to_string().into());
        }
        sql.push_str(" ORDER BY at, app");
        let mut statement = self.conn.prepare_cached(&sql).map_err(fail)?;
        let rows = statement
            .query_map(params_from_iter(args), |row| {
                let n = |i: usize| row.get::<_, i64>(i).map(|v| v.max(0) as u64);
                Ok(Sample {
                    app: row.get(0)?,
                    at_unix: row.get(1)?,
                    served: n(2)?,
                    ok: n(3)?,
                    errors: n(4)?,
                    rejected: n(5)?,
                    worker_ms: n(6)?,
                    p50_us: n(7)?,
                    p99_us: n(8)?,
                })
            })
            .map_err(fail)?;
        rows.collect::<Result<_, _>>().map_err(fail)
    }

    /// How many rows each table holds, for the operations views: minutes,
    /// then hours.
    pub fn rows(&self) -> Result<(u64, u64), String> {
        Ok((self.count("minute")?, self.count("hour")?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(app: &str, at: i64, served: u64, p99: u64) -> Sample {
        Sample {
            app: app.into(),
            at_unix: at,
            served,
            ok: served,
            errors: 1,
            rejected: 2,
            worker_ms: 10,
            p50_us: p99 / 2,
            p99_us: p99,
        }
    }

    const H: i64 = 1_000_000 / 3600 * 3600;

    #[test]
    fn writes_and_reads_and_replaces_the_same_minute() {
        let s = Series::in_memory().unwrap();
        let a = sample("a", H, 5, 100);
        s.record(std::slice::from_ref(&a)).unwrap();
        assert_eq!(s.minutes(None, 0).unwrap(), vec![a.clone()]);
        let b = sample("a", H, 7, 200);
        s.record(std::slice::from_ref(&b)).unwrap();
        assert_eq!(s.minutes(None, 0).unwrap(), vec![b]);
    }

    #[test]
    fn minutes_filter_by_app_and_since_and_come_oldest_first() {
        let s = Series::in_memory().unwrap();
        s.record(&[
            sample("b", H + 120, 1, 1),
            sample("a", H + 60, 2, 1),
            sample("a", H, 3, 1),
            sample("b", H + 60, 4, 1),
        ])
        .unwrap();
        let all = s.minutes(None, 0).unwrap();
        assert_eq!(all.len(), 4);
        assert!(all.windows(2).all(|w| w[0].at_unix <= w[1].at_unix));
        let a = s.minutes(Some("a"), 0).unwrap();
        assert_eq!(a.len(), 2);
        assert!(a.iter().all(|x| x.app == "a"));
        let late = s.minutes(None, H + 60).unwrap();
        assert_eq!(late.len(), 3);
        assert!(late.iter().all(|x| x.at_unix >= H + 60));
    }

    #[test]
    fn sweep_rolls_ended_hours_with_sum_and_max() {
        let s = Series::in_memory().unwrap();
        s.record(&[
            sample("a", H, 5, 100),
            sample("a", H + 60, 7, 300),
            sample("a", H + 120, 1, 200),
            sample("a", H + 3600, 9, 50),
        ])
        .unwrap();
        // Inside the second hour: only the first has ended.
        s.sweep(H + 3600 + 30, KEEP_MINUTES_SECS, KEEP_HOURS_SECS)
            .unwrap();
        let hours = s.hours(None, 0).unwrap();
        assert_eq!(hours.len(), 1);
        let h = &hours[0];
        assert_eq!(h.at_unix, H);
        assert_eq!(h.served, 13);
        assert_eq!(h.ok, 13);
        assert_eq!(h.errors, 3);
        assert_eq!(h.rejected, 6);
        assert_eq!(h.worker_ms, 30);
        assert_eq!(h.p99_us, 300);
        assert_eq!(h.p50_us, 150);
    }

    #[test]
    fn sweep_twice_does_not_double_the_hour() {
        let s = Series::in_memory().unwrap();
        s.record(&[sample("a", H, 5, 100), sample("a", H + 60, 7, 100)])
            .unwrap();
        let now = H + 3600 + 5;
        s.sweep(now, KEEP_MINUTES_SECS, KEEP_HOURS_SECS).unwrap();
        s.sweep(now, KEEP_MINUTES_SECS, KEEP_HOURS_SECS).unwrap();
        let hours = s.hours(None, 0).unwrap();
        assert_eq!(hours.len(), 1);
        assert_eq!(hours[0].served, 12);
    }

    #[test]
    fn sweep_drops_old_minutes_and_hours_and_counts_them() {
        let s = Series::in_memory().unwrap();
        s.record(&[sample("a", H, 1, 1), sample("a", H + 60, 1, 1)])
            .unwrap();
        // Far enough on that the minutes are past retention; they roll up
        // first, then go.
        let now = H + 10 * 3600;
        let dropped = s.sweep(now, 3600, KEEP_HOURS_SECS).unwrap();
        assert_eq!(dropped, 2);
        assert_eq!(s.rows().unwrap(), (0, 1));
        // Now the hour itself is past retention.
        let dropped = s.sweep(now, 3600, 3600).unwrap();
        assert_eq!(dropped, 1);
        assert_eq!(s.rows().unwrap(), (0, 0));
    }

    #[test]
    fn unaligned_times_are_rounded_down_to_the_minute() {
        let s = Series::in_memory().unwrap();
        s.record(&[sample("a", H + 61, 1, 1), sample("a", H + 119, 2, 1)])
            .unwrap();
        let got = s.minutes(None, 0).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].at_unix, H + 60);
        assert_eq!(got[0].served, 2);
    }

    #[test]
    fn rows_counts_both_tables() {
        let s = Series::in_memory().unwrap();
        assert_eq!(s.rows().unwrap(), (0, 0));
        s.record(&[sample("a", H, 1, 1), sample("b", H, 1, 1)])
            .unwrap();
        assert_eq!(s.rows().unwrap(), (2, 0));
        s.sweep(H + 3600, KEEP_MINUTES_SECS, KEEP_HOURS_SECS)
            .unwrap();
        assert_eq!(s.rows().unwrap(), (2, 2));
    }

    fn host(at: i64, memory: u64, cpu: u64, load: Option<u64>) -> HostRow {
        HostRow {
            at_unix: at,
            memory_bytes: Some(memory),
            memory_max_bytes: Some(1000),
            cpu_usec: Some(cpu),
            cpu_throttled_usec: Some(cpu / 10),
            cpu_throttled_periods: Some(1),
            cpu_quota_millicores: Some(2000),
            disk_total_bytes: Some(500),
            disk_available_bytes: Some(400 - memory / 10),
            load1_milli: load,
            in_flight: memory / 100,
            queued: 1,
            busy: 1,
            workers: 2,
            ..HostRow::default()
        }
    }

    #[test]
    fn host_rows_round_trip_with_their_unknowns() {
        let s = Series::in_memory().unwrap();
        let a = host(H + 61, 300, 50, None);
        s.record_host(&a).unwrap();
        let got = s.host_minutes(0).unwrap();
        assert_eq!(
            got,
            vec![HostRow {
                at_unix: H + 60,
                ..a
            }]
        );
        assert_eq!(got[0].rss_bytes, None, "a NULL stays unknown");
        // The same minute again replaces it.
        s.record_host(&host(H + 60, 700, 9, Some(5))).unwrap();
        let got = s.host_minutes(0).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].memory_bytes, Some(700));
        assert_eq!(got[0].load1_milli, Some(5));
        assert_eq!(s.host_minutes(H + 120).unwrap(), vec![]);
        assert_eq!(s.host_rows().unwrap(), (1, 0));
        assert_eq!(s.rows().unwrap(), (0, 0), "the apps' tables are apart");
    }

    #[test]
    fn host_hours_sum_the_cpu_and_keep_the_worst_gauge() {
        let s = Series::in_memory().unwrap();
        for row in [
            host(H, 300, 1000, Some(1000)),
            host(H + 60, 900, 2000, Some(2000)),
            host(H + 120, 600, 3000, None),
            host(H + 3600, 100, 1, Some(9)),
        ] {
            s.record_host(&row).unwrap();
        }
        s.sweep(H + 3600 + 30, KEEP_MINUTES_SECS, KEEP_HOURS_SECS)
            .unwrap();
        // Twice, which must not double it.
        s.sweep(H + 3600 + 30, KEEP_MINUTES_SECS, KEEP_HOURS_SECS)
            .unwrap();
        let hours = s.host_hours(0).unwrap();
        assert_eq!(hours.len(), 1);
        let h = &hours[0];
        assert_eq!(h.at_unix, H);
        assert_eq!(h.cpu_usec, Some(6000));
        assert_eq!(h.cpu_throttled_usec, Some(600));
        assert_eq!(h.cpu_throttled_periods, Some(3));
        assert_eq!(h.memory_bytes, Some(900));
        assert_eq!(h.memory_max_bytes, Some(1000));
        assert_eq!(h.disk_available_bytes, Some(310), "the least free");
        assert_eq!(h.load1_milli, Some(1500), "the mean of the known");
        assert_eq!(h.rss_bytes, None, "unknown all hour");
        assert_eq!(h.in_flight, 9);
        assert_eq!(h.workers, 2);
        // And the host's rows go with the apps' when they are old.
        let dropped = s.sweep(H + 10 * 3600, 3600, 3600).unwrap();
        assert_eq!(dropped, 6, "four minutes and the two hours they made");
        assert_eq!(s.host_rows().unwrap(), (0, 0));
    }
}
