//! `osctl bus bench`: round-trip latency, request throughput and pub/sub
//! throughput through the running broker.
//!
//! It opens a second connection as its own echo service, so it measures the whole
//! path (client, socket, broker, socket, service and back) with nothing else
//! installed. The last line is `BENCH key=value ...` for scripts.

use std::fmt;
use std::time::{Duration, Instant};

use bus_client::Client;
use serde_json::json;

/// Requests kept in flight for the throughput test.
const IN_FLIGHT: usize = 64;
const LATENCY_SAMPLES: usize = 2000;
const WARMUP: usize = 200;
/// Publishes sent before waiting for the subscriber to catch up.
const PUBLISH_BATCH: usize = 1000;

pub struct Report {
    pub size: usize,
    pub p50: Duration,
    pub p99: Duration,
    pub requests_per_s: f64,
    pub publishes_per_s: f64,
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let us = |d: Duration| d.as_secs_f64() * 1e6;
        writeln!(f, "bus benchmark, {}-byte payloads", self.size)?;
        writeln!(f, "  round trip p50     {:>9.1} µs", us(self.p50))?;
        writeln!(f, "  round trip p99     {:>9.1} µs", us(self.p99))?;
        writeln!(f, "  requests/s         {:>9.0}  ({IN_FLIGHT} in flight)", self.requests_per_s)?;
        writeln!(
            f,
            "  publishes/s        {:>9.0}  (one subscriber, {PUBLISH_BATCH} at a time)",
            self.publishes_per_s
        )?;
        writeln!(
            f,
            "BENCH size={} rtt_p50_us={:.1} rtt_p99_us={:.1} requests_per_s={:.0} publishes_per_s={:.0}",
            self.size,
            us(self.p50),
            us(self.p99),
            self.requests_per_s,
            self.publishes_per_s
        )
    }
}

/// `service` is a second connection to the same broker as `bus`.
pub async fn run(bus: &Client, service: Client, duration: Duration, size: usize) -> Result<Report, String> {
    let e = |e: bus_client::CallError| e.to_string();
    let conn = bus.hello().await.map_err(e)?.0;
    let name = format!("bench-{}-{conn}", std::process::id());
    service.claim(&name).await.map_err(e)?;
    let echo = tokio::spawn({
        let service = service.clone();
        async move { crate::commands::echo(&service).await }
    });
    let payload = json!("x".repeat(size));

    // Latency: one request at a time.
    let mut samples = Vec::with_capacity(LATENCY_SAMPLES);
    for i in 0..WARMUP + LATENCY_SAMPLES {
        let started = Instant::now();
        bus.request(&name, "echo", payload.clone()).await.map_err(e)?;
        if i >= WARMUP {
            samples.push(started.elapsed());
        }
    }
    samples.sort_unstable();
    let p50 = samples[samples.len() / 2];
    let p99 = samples[samples.len() * 99 / 100];

    // Throughput: keep IN_FLIGHT requests going for `duration`.
    let deadline = Instant::now() + duration;
    let started = Instant::now();
    let mut workers = Vec::new();
    for _ in 0..IN_FLIGHT {
        let (bus, name, payload) = (bus.clone(), name.clone(), payload.clone());
        workers.push(tokio::spawn(async move {
            let mut done = 0u64;
            while Instant::now() < deadline {
                bus.request(&name, "echo", payload.clone()).await?;
                done += 1;
            }
            Ok::<u64, bus_client::CallError>(done)
        }));
    }
    let mut done = 0;
    for w in workers {
        done += w.await.map_err(|e| e.to_string())?.map_err(e)?;
    }
    let requests_per_s = done as f64 / started.elapsed().as_secs_f64();

    // Pub/sub: publish in batches and wait for each to arrive. An unpaced flood
    // would only measure how fast the broker drops the subscriber for falling
    // behind (its queue is bounded on purpose).
    let topic = format!("{name}.pub");
    let mut sub = service.subscribe(&topic).await.map_err(e)?;
    let batches = ((requests_per_s * duration.as_secs_f64()) as usize / PUBLISH_BATCH).clamp(1, 200);
    let started = Instant::now();
    for _ in 0..batches {
        for _ in 0..PUBLISH_BATCH {
            bus.publish(&topic, payload.clone()).await.map_err(e)?;
        }
        for _ in 0..PUBLISH_BATCH {
            sub.next().await.ok_or("the bus closed the subscriber's connection")?;
        }
    }
    let publishes_per_s = (batches * PUBLISH_BATCH) as f64 / started.elapsed().as_secs_f64();

    echo.abort();
    Ok(Report { size, p50, p99, requests_per_s, publishes_per_s })
}
