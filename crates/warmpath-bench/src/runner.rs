//! The dispatch loop.
//!
//! Open loop is the default and the only mode whose numbers should be
//! published. Closed loop exists so the coordinated-omission gap can be shown
//! against a generator that actually has the defect.
//!
//! One limit worth knowing: tokio's timer has millisecond resolution, so at
//! arrival rates near or above 1000/s the scheduler cannot place arrivals
//! precisely. That shows up as dispatch lag, which the run's validity check
//! already reports. The harness detects its own ceiling rather than quietly
//! measuring through it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::StreamExt;

use crate::record::{Mode, RequestRecord, RunConfig};
use crate::schedule::{poisson_schedule, Rng};
use crate::workload::build_bodies;

/// Requests kept in the body pool for a closed-loop run, cycled as needed.
const CLOSED_LOOP_BODY_POOL: usize = 4096;

pub struct RunOutcome {
    pub records: Vec<RequestRecord>,
    pub wall_clock: Duration,
}

pub async fn run(config: &RunConfig) -> anyhow::Result<RunOutcome> {
    match config.mode {
        Mode::OpenLoop => run_open_loop(config).await,
        Mode::ClosedLoop => run_closed_loop(config).await,
    }
}

/// Dispatch on a schedule fixed before the first request goes out.
async fn run_open_loop(config: &RunConfig) -> anyhow::Result<RunOutcome> {
    let mut rng = Rng::new(config.seed);
    let schedule = poisson_schedule(config.rate_per_second, config.duration(), &mut rng);
    anyhow::ensure!(
        !schedule.is_empty(),
        "the arrival schedule is empty; raise the rate or the duration"
    );

    let bodies = build_bodies(config, schedule.len());
    let client = build_client()?;
    let url = config.url();
    let warmup = config.warmup();

    tracing::info!(
        arrivals = schedule.len(),
        rate = config.rate_per_second,
        duration_secs = config.duration_secs,
        "open-loop schedule built"
    );

    let run_start = Instant::now();
    let mut tasks = Vec::with_capacity(schedule.len());

    for (index, &intended) in schedule.iter().enumerate() {
        // Sleeping to an absolute deadline, not for a relative interval, so
        // drift cannot accumulate. A deadline already in the past returns
        // immediately, which is what turns generator slowness into measured lag
        // instead of silently stretching the schedule.
        tokio::time::sleep_until(tokio::time::Instant::from_std(run_start + intended)).await;

        tasks.push(tokio::spawn(issue(
            client.clone(),
            url.clone(),
            bodies[index].clone(),
            Attempt {
                index: index as u64,
                run_start,
                intended,
                // Measured when the task actually starts, not when it was
                // spawned. If the executor is saturated the request really was
                // sent late, and that delay belongs in the lag.
                dispatch_at: None,
                warmup,
            },
        )));
    }

    let mut records = Vec::with_capacity(tasks.len());
    for task in tasks {
        records.push(task.await?);
    }

    Ok(RunOutcome {
        records,
        wall_clock: run_start.elapsed(),
    })
}

/// Dispatch from a fixed number of callers, each waiting for its response
/// before sending again.
///
/// There is no schedule here, so there is nothing to be late against: a
/// request's intended time is the moment its caller became free. That is
/// exactly the blind spot that makes closed-loop tail numbers optimistic.
async fn run_closed_loop(config: &RunConfig) -> anyhow::Result<RunOutcome> {
    anyhow::ensure!(
        config.concurrency > 0,
        "closed loop needs at least one caller"
    );

    let bodies = Arc::new(build_bodies(config, CLOSED_LOOP_BODY_POOL));
    let client = build_client()?;
    let url = config.url();
    let warmup = config.warmup();
    let duration = config.duration();

    let run_start = Instant::now();
    let deadline = run_start + duration;
    let next_index = Arc::new(AtomicU64::new(0));

    let mut callers = Vec::with_capacity(config.concurrency);
    for _ in 0..config.concurrency {
        let client = client.clone();
        let url = url.clone();
        let bodies = Arc::clone(&bodies);
        let next_index = Arc::clone(&next_index);

        callers.push(tokio::spawn(async move {
            let mut records = Vec::new();
            while Instant::now() < deadline {
                let index = next_index.fetch_add(1, Ordering::Relaxed);
                let body = bodies[index as usize % bodies.len()].clone();

                // One clock read serves as both times. A closed-loop caller's
                // request is due exactly when the caller became free to send
                // it, so there is nothing to be late against. Reading the clock
                // twice would manufacture a few microseconds of lag that does
                // not exist.
                let now = Instant::now();
                records.push(
                    issue(
                        client.clone(),
                        url.clone(),
                        body,
                        Attempt {
                            index,
                            run_start,
                            intended: now.saturating_duration_since(run_start),
                            dispatch_at: Some(now),
                            warmup,
                        },
                    )
                    .await,
                );
            }
            records
        }));
    }

    let mut records = Vec::new();
    for caller in callers {
        records.extend(caller.await?);
    }
    records.sort_by_key(|record| record.index);

    Ok(RunOutcome {
        records,
        wall_clock: run_start.elapsed(),
    })
}

fn build_client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        // A pool large enough that connection setup is not what the run
        // measures, and no idle timeout surprises mid-run.
        .pool_max_idle_per_host(1024)
        .build()
        .map_err(Into::into)
}

/// Everything about one request that is fixed before it is sent.
struct Attempt {
    index: u64,
    run_start: Instant,
    /// When the request was due, as an offset from the start of the run.
    intended: Duration,
    /// When the request was actually sent. `None` means measure it at the
    /// moment this attempt begins.
    dispatch_at: Option<Instant>,
    warmup: Duration,
}

/// Send one request and measure it against both clocks.
async fn issue(
    client: reqwest::Client,
    url: String,
    body: Bytes,
    attempt: Attempt,
) -> RequestRecord {
    let Attempt {
        index,
        run_start,
        intended,
        dispatch_at,
        warmup,
    } = attempt;

    let intended_at = run_start + intended;
    let dispatch_at = dispatch_at.unwrap_or_else(Instant::now);
    let dispatch_offset = dispatch_at.saturating_duration_since(run_start);

    let mut record = RequestRecord {
        index,
        intended_offset_us: micros(intended),
        dispatch_offset_us: micros(dispatch_offset),
        dispatch_lag_us: micros(dispatch_offset.saturating_sub(intended)),
        ttft_us: None,
        ttft_from_dispatch_us: None,
        e2e_us: None,
        e2e_from_dispatch_us: None,
        status: None,
        response_bytes: 0,
        error: None,
        warmup: intended < warmup,
    };

    let response = match client
        .post(&url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
    {
        Ok(response) => response,
        Err(err) => {
            record.error = Some(err.to_string());
            return record;
        }
    };

    record.status = Some(response.status().as_u16());
    let mut stream = response.bytes_stream();
    let mut seen_first_byte = false;

    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => {
                if !seen_first_byte {
                    seen_first_byte = true;
                    let now = Instant::now();
                    record.ttft_us = Some(micros(now.saturating_duration_since(intended_at)));
                    record.ttft_from_dispatch_us =
                        Some(micros(now.saturating_duration_since(dispatch_at)));
                }
                record.response_bytes += bytes.len() as u64;
            }
            Err(err) => {
                record.error = Some(err.to_string());
                return record;
            }
        }
    }

    let now = Instant::now();
    record.e2e_us = Some(micros(now.saturating_duration_since(intended_at)));
    record.e2e_from_dispatch_us = Some(micros(now.saturating_duration_since(dispatch_at)));
    record
}

/// Microseconds, clamped to at least one so a measurement is always
/// recordable in an HdrHistogram whose lowest value is one.
fn micros(duration: Duration) -> u64 {
    duration.as_micros().max(1) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn micros_never_returns_zero() {
        assert_eq!(micros(Duration::ZERO), 1);
        assert_eq!(micros(Duration::from_nanos(1)), 1);
        assert_eq!(micros(Duration::from_micros(250)), 250);
        assert_eq!(micros(Duration::from_millis(3)), 3_000);
    }
}
