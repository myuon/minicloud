//! `minicloud-load`: a small load generator for the performance check.
//!
//! The method is the Cove repository's `examples/edge/compare` one, cut down:
//!
//! - **open loop** (`--rate N`): request *i* is due at *i* / N seconds into
//!   the run, and its latency is measured **from when it was due**, not
//!   from when a connection was free to send it (`cove-edge-load
//!   --from-intended`), so a server that falls behind is charged for the
//!   queue it makes (no coordinated omission). The send lag — how late a
//!   request went out — is reported beside it.
//! - **closed loop** (`--rate 0`): every connection sends its next request as
//!   soon as its last is answered; the throughput is the capacity at that
//!   concurrency.
//!
//! Connections are kept alive, `--connections` of them, each a task on an
//! eight-thread tokio runtime (`cove-edge-load` had eight client threads).
//! A request waits for its due time on tokio's timer, which waits in
//! kqueue/epoll: a thread `sleep` on macOS overshoots by up to 150 ms (the
//! edge README's note), which would be the generator measuring itself.
//! `--mix` names what to ask, by app:
//!
//! | name | request |
//! | --- | --- |
//! | `hello` | `/hello/?name=load` |
//! | `crunch` | `/crunch/?n=` 20000, 50000, 100000, 150000 in turn (`crunch20k`: always 20000) |
//! | `slow` | `/slow/?ms=60&times=3`: three parks, 180 ms of waiting, `aggregate`'s shape |
//! | `proxy` | `/proxy/?url=http://127.0.0.1:<port>/hello/`: a fetch of `hello` from the same host |
//! | `algo-sat` | `/algo/sat?example=hard&part=result`: the playground's heavy SAT run, DPLL refuting 8 pigeons in 7 holes |
//! | `algo-anneal` | `/algo/anneal?example=cooling&part=result`: the playground's annealing comparison, two runs of 200,000 iterations |
//! | `algo` | `/algo/matching?example=large&algorithm=augmenting&part=result`: the playground's heavy run, a maximum matching of a 2000 × 2000 graph by the simple algorithm |
//!
//! `--mix cpu-io` is `crunch=35,slow=30,proxy=10,hello=25`, `cove-edge-load`'s
//! `cpu-io` with `slow` for `aggregate` and `impatient`'s share given to
//! `hello`. Which app request *i* asks is chosen from *i*, so the same
//! command asks the same sequence. The mixes and the connection are
//! [`minicloud::load`]'s, which the host's own load runs share.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clap::Parser;
use minicloud::load::{mix, percentile, request, Conn};

#[derive(Parser)]
#[command(
    name = "minicloud-load",
    about = "Load for minicloud: open-loop latency and capacity"
)]
struct Args {
    /// The host.
    #[arg(long, default_value = "127.0.0.1:8080")]
    addr: String,
    /// Requests a second, open loop; 0 is closed loop (capacity).
    #[arg(long, default_value_t = 0.0)]
    rate: f64,
    /// How many requests.
    #[arg(long, default_value_t = 2000)]
    requests: u64,
    /// Kept-alive connections, one thread each.
    #[arg(long, default_value_t = 64)]
    connections: usize,
    /// What to ask: `hello`, `crunch`, `crunch20k`, `slow`, `proxy`, `algo`, `algo-sat`, `algo-anneal`,
    /// `cpu-io`, or weights like `hello=3,crunch=1`.
    #[arg(long, default_value = "hello")]
    mix: String,
    /// Requests sent first and not measured.
    #[arg(long, default_value_t = 200)]
    warmup: u64,
    /// Write the summary as JSON here too.
    #[arg(long)]
    json: Option<std::path::PathBuf>,
}

/// One answered request.
struct Sample {
    app: &'static str,
    status: u16,
    /// From when it was due (open loop) or sent (closed loop).
    latency: Duration,
    /// From when it was sent.
    sent: Duration,
    /// How late it went out.
    lag: Duration,
}

async fn run(
    args: &Args,
    mix: &[(&'static str, u32)],
    requests: u64,
    record: bool,
) -> (Vec<Sample>, u64, Duration) {
    let port: u16 = args
        .addr
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);
    let next = Arc::new(AtomicU64::new(0));
    let failed = Arc::new(AtomicU64::new(0));
    let samples = Arc::new(Mutex::new(Vec::with_capacity(requests as usize)));
    let started = tokio::time::Instant::now() + Duration::from_millis(50);
    let tasks: Vec<_> = (0..args.connections)
        .map(|_| {
            let (next, failed, samples) =
                (Arc::clone(&next), Arc::clone(&failed), Arc::clone(&samples));
            let addr = args.addr.clone();
            let rate = args.rate;
            let mix = mix.to_vec();
            tokio::spawn(async move {
                let mut conn = Conn::new(addr);
                let mut mine = Vec::new();
                tokio::time::sleep_until(started).await;
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= requests {
                        break;
                    }
                    let due = if rate > 0.0 {
                        let due = started + Duration::from_secs_f64(i as f64 / rate);
                        tokio::time::sleep_until(due).await;
                        due.into_std()
                    } else {
                        Instant::now()
                    };
                    let (app, path) = request(i, &mix, port);
                    let sent = Instant::now();
                    match conn.ask(&path).await {
                        Some(status) if record => {
                            let done = Instant::now();
                            mine.push(Sample {
                                app,
                                status,
                                latency: done - due,
                                sent: done - sent,
                                lag: sent.saturating_duration_since(due),
                            });
                        }
                        Some(_) => {}
                        None => {
                            failed.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                samples.lock().unwrap().extend(mine);
            })
        })
        .collect();
    for task in tasks {
        task.await.unwrap();
    }
    let elapsed = started.elapsed();
    let samples = std::mem::take(&mut *samples.lock().unwrap());
    (samples, failed.load(Ordering::Relaxed), elapsed)
}

fn main() {
    let args = Args::parse();
    let mix = mix(&args.mix).unwrap_or_else(|why| panic!("{why}"));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(8)
        .enable_all()
        .build()
        .expect("a runtime");
    if args.warmup > 0 {
        let warm = Args {
            rate: 0.0,
            ..Args::parse()
        };
        runtime.block_on(run(&warm, &mix, args.warmup, false));
    }
    let (samples, failed, elapsed) = runtime.block_on(run(&args, &mix, args.requests, true));
    let mode = if args.rate > 0.0 {
        format!(
            "open loop at {} req/s, latency from the intended start",
            args.rate
        )
    } else {
        "closed loop, latency from the send".to_string()
    };
    println!(
        "{} requests to {} ({}), {} connections kept alive, {mode}",
        args.requests, args.addr, args.mix, args.connections
    );
    let throughput = samples.len() as f64 / elapsed.as_secs_f64();
    let mut latencies: Vec<Duration> = samples.iter().map(|s| s.latency).collect();
    latencies.sort();
    let mut sent: Vec<Duration> = samples.iter().map(|s| s.sent).collect();
    sent.sort();
    let mut lags: Vec<Duration> = samples.iter().map(|s| s.lag).collect();
    lags.sort();
    let late = lags
        .iter()
        .filter(|lag| **lag > Duration::from_millis(1))
        .count();
    println!(
        "  answered {} ({} failed to connect or read) in {:.2} s: {throughput:.0} req/s",
        samples.len(),
        failed,
        elapsed.as_secs_f64()
    );
    println!(
        "  latency ms: p50 {:.2}  p90 {:.2}  p99 {:.2}  max {:.2}   (p99 from the send {:.2})",
        percentile(&latencies, 0.5),
        percentile(&latencies, 0.9),
        percentile(&latencies, 0.99),
        percentile(&latencies, 1.0),
        percentile(&sent, 0.99),
    );
    if args.rate > 0.0 {
        println!(
            "  send lag ms: p50 {:.2}  p99 {:.2}  max {:.2}; {late} sent more than 1 ms late",
            percentile(&lags, 0.5),
            percentile(&lags, 0.99),
            percentile(&lags, 1.0),
        );
    }
    let mut per_app: BTreeMap<&str, (Vec<Duration>, BTreeMap<u16, u64>)> = BTreeMap::new();
    for sample in &samples {
        let entry = per_app.entry(sample.app).or_default();
        entry.0.push(sample.latency);
        *entry.1.entry(sample.status).or_default() += 1;
    }
    let mut apps_json = serde_json::Map::new();
    for (app, (mut latencies, statuses)) in per_app {
        latencies.sort();
        let statuses_text: Vec<String> =
            statuses.iter().map(|(s, n)| format!("{n} x {s}")).collect();
        println!(
            "  {app:<8} {:<22} {:>8.1} req/s  p50 {:>7.2} ms  p99 {:>7.2} ms",
            statuses_text.join(", "),
            latencies.len() as f64 / elapsed.as_secs_f64(),
            percentile(&latencies, 0.5),
            percentile(&latencies, 0.99),
        );
        apps_json.insert(
            app.to_string(),
            serde_json::json!({
                "answered": latencies.len(),
                "statuses": statuses.iter().map(|(s, n)| (s.to_string(), *n)).collect::<BTreeMap<_, _>>(),
                "p50_ms": percentile(&latencies, 0.5),
                "p99_ms": percentile(&latencies, 0.99),
            }),
        );
    }
    if let Some(path) = &args.json {
        let summary = serde_json::json!({
            "addr": args.addr,
            "mix": args.mix,
            "rate": args.rate,
            "requests": args.requests,
            "connections": args.connections,
            "answered": samples.len(),
            "failed": failed,
            "elapsed_s": elapsed.as_secs_f64(),
            "throughput": throughput,
            "p50_ms": percentile(&latencies, 0.5),
            "p99_ms": percentile(&latencies, 0.99),
            "p99_sent_ms": percentile(&sent, 0.99),
            "lag_p99_ms": percentile(&lags, 0.99),
            "apps": apps_json,
        });
        std::fs::write(path, serde_json::to_string_pretty(&summary).unwrap())
            .expect("writes the summary");
    }
}
