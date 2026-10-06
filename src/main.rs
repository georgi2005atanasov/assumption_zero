use anyhow::{Context, Result};
use assumption_zero::{
    feed::{Feed, text_message},
    live,
    metrics::{self, Benchmark},
    state,
};
use futures_util::{SinkExt, StreamExt};
use std::io::Write;
use tokio_tungstenite::tungstenite::{Message, protocol::frame::coding::CloseCode};
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    if std::env::args().any(|a| a == "--version") {
        println!(
            "assumption-zero {} rustc {}",
            env!("CARGO_PKG_VERSION"),
            env!("RUSTC_VERSION")
        );
        return Ok(());
    }
    let log = match std::env::var("LOG_EVENTS")
        .unwrap_or_else(|_| "false".into())
        .to_lowercase()
        .as_str()
    {
        "true" => true,
        "false" => false,
        _ => anyhow::bail!("LOG_EVENTS must be true or false"),
    };
    match std::env::var("EXECUTION_MODE")
        .unwrap_or_else(|_| "Replay".into())
        .to_lowercase()
        .as_str()
    {
        "simulatedexchange" => assumption_zero::simulation::run().await,
        "replay" => replay(log).await,
        "seeddata" => live::capture(log).await,
        "normalexecution" => live::normal(log).await,
        _ => anyhow::bail!("EXECUTION_MODE must be Replay, SeedData, or NormalExecution"),
    }
}
async fn replay(log: bool) -> Result<()> {
    let fixture = std::env::var("REPLAY_FIXTURE")
        .ok()
        .filter(|s| !s.trim().is_empty());
    let (ledger, runtimes) = if let Some(path) = fixture {
        state::fixture(&path)?
    } else {
        println!(
            "Performance replay: account and starting books are estimated; quote values are not historical results."
        );
        state::synthetic()
    };
    let mut feed = Feed::new(ledger, runtimes, true, log);
    let output = std::env::var("BENCHMARK_OUTPUT").ok();
    if output.is_some() {
        anyhow::ensure!(
            std::env::var("BENCHMARK_DIAGNOSTIC").as_deref() != Ok("1"),
            "GC diagnostics apply only to C#"
        );
        let expected = live::required("BENCHMARK_EXPECTED_FRAMES")?.parse()?;
        let warmup = std::env::var("BENCHMARK_WARMUP_FRAMES")
            .unwrap_or_else(|_| "0".into())
            .parse()?;
        feed.benchmark = Some(Benchmark::new(expected, warmup)?);
        for r in &mut feed.runtimes {
            r.stages = true;
        }
    }
    if let Ok(path) = std::env::var("CORRECTNESS_TRACE") {
        feed.trace = Some(std::io::BufWriter::new(std::fs::File::create(path)?));
    }
    let measure = std::env::var("MEASURE_ALLOCATIONS").as_deref() == Ok("1");
    if measure {
        metrics::track_allocations();
    }
    feed.measure_allocations = measure;
    let url = std::env::var("REPLAY_URL").unwrap_or_else(|_| "ws://127.0.0.1:5080/stream".into());
    let mut completed = false;
    let result: Result<()> = async {
        let mut socket = live::connect(&url, None).await?;
        println!("SBE connection open.");
        loop {
            let next = tokio::select! {
                _ = tokio::signal::ctrl_c() => anyhow::bail!("replay interrupted"),
                next = socket.next() => next,
            };
            let msg = next.context("replay ended without normal closure")??;
            let receive = metrics::ticks();
            match msg {
                Message::Binary(bytes) => {
                    if let Some(m) = &mut feed.benchmark {
                        m.begin(1, bytes.len(), receive)?;
                    }
                    // Decimal/grid overflow is fatal, just as in C#. Still finish the
                    // frame and write an incomplete report when arithmetic unwinds.
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        feed.binary(&bytes)
                    }));
                    if let Some(m) = &mut feed.benchmark {
                        m.end();
                    }
                    result.map_err(|_| {
                        anyhow::anyhow!("arithmetic failure while processing replay")
                    })??;
                }
                Message::Text(text) => {
                    if let Some(m) = &mut feed.benchmark {
                        m.begin(0, text.len(), receive)?;
                    }
                    let result = text_message(text.as_bytes(), log);
                    if let Some(m) = &mut feed.benchmark {
                        m.end();
                    }
                    result?;
                }
                Message::Close(frame) => {
                    anyhow::ensure!(
                        frame.is_some_and(|f| f.code == CloseCode::Normal),
                        "abnormal replay closure"
                    );
                    socket.flush().await?;
                    completed = true;
                    break;
                }
                Message::Ping(_) | Message::Pong(_) => (),
                _ => (),
            }
        }
        Ok(())
    }
    .await;
    if let Some(trace) = &mut feed.trace {
        trace.flush()?;
    }
    let mut report = feed.benchmark.as_ref().map(|b| b.finish(completed, ""));
    let hash = metrics::final_state(&feed.runtimes, &feed.ledger);
    if let Some(report) = &mut report {
        report["FinalStateHash"] = serde_json::Value::String(hash.clone());
    }
    if let (Some(path), Some(report)) = (output, report) {
        serde_json::to_writer(
            std::io::BufWriter::new(std::fs::File::create(path)?),
            &report,
        )?;
    }
    result?;
    println!(
        "Replay complete: {} decisions, {} with place/replace/cancel intents.",
        feed.decisions, feed.actionable
    );
    println!(
        "Products: {} with a book, {} ready of {} configured.",
        feed.runtimes.iter().filter(|r| r.book.synchronized).count(),
        feed.runtimes
            .iter()
            .filter(|r| r.ready(&feed.ledger))
            .count(),
        feed.runtimes.len()
    );
    println!("FINAL_STATE_HASH={hash}");
    if measure {
        println!(
            "MARKET_ALLOCATOR_BYTES={};READY_DECISIONS={}",
            feed.allocation_bytes
                .map(|x| x.to_string())
                .unwrap_or_else(|| "-1".into()),
            feed.ready_decisions
        );
    }
    Ok(())
}
