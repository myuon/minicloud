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
        Ok(Series { conn })
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
        let minutes = tx
            .execute("DELETE FROM minute WHERE at < ?1", params![minutes_before])
            .map_err(fail)?;
        let hours = tx
            .execute("DELETE FROM hour WHERE at < ?1", params![hours_before])
            .map_err(fail)?;
        tx.commit().map_err(fail)?;
        Ok((minutes + hours) as u64)
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
        let count = |table: &str| -> Result<u64, String> {
            self.conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| {
                    r.get::<_, i64>(0)
                })
                .map(|n| n as u64)
                .map_err(fail)
        };
        Ok((count("minute")?, count("hour")?))
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
}
