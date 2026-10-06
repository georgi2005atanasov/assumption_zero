# AssumptionZero — Rust

Rust implementation of the [C# AssumptionZero market-processing application](https://github.com/georgi2005atanasov/AssumptionZero.CSharp).
The existing C# simulator supplies identical SBE messages to both applications;
there is no separate Rust simulator.

The flow is receive/reassemble → SBE validation/decode → bounded book → signals →
pricing and risk → quote intents. The port follows C#'s strategy rules, synthetic
initialization scheme, fixture format, numeric units, account reservations, fills,
and order-state transitions. Both applications default to the same 100 symbols
in the same order. Normal replay and live mode produce intents only; the new
`SimulatedExchange` mode sends orders to a local controlled exchange.

The comparison explores the resource and latency tradeoffs of two implementations.
C# is already optimized around bounded arrays, reusable buffers, and a single
market processing loop, with tests for zero managed allocations in warmed core
processing. A smaller Rust process footprint is useful when it addresses a real
resource constraint; it is not, by itself, a reason to replace the C# application.

## Build and test

```bash
export CSHARP_ROOT="$HOME/RiderProjects/AssumptionZero"
export RUST_ROOT="$HOME/CLionProjects/assumption-zero"
export PATH="$HOME/.dotnet:$HOME/.cargo/bin:$PATH"
"$CSHARP_ROOT/scripts/run-csharp.sh" build
"$RUST_ROOT/scripts/run-rust.sh" build
"$RUST_ROOT/scripts/run-rust.sh" test
```

The scripts work from any directory. Build and test explicitly select the stable
Rust toolchain and install it if missing; no global rustup default is required. For the complete quiet-machine setup,
CPU-affinity examples, PostgreSQL configuration, paired/individual benchmark
commands, and report interpretation, see the shared runbook:
`$CSHARP_ROOT/docs/testing.md`.

## Benchmarks

```bash
# Set PG_CONNECTION for your database; the built-in address is machine-specific.
"$RUST_ROOT/scripts/run-rust.sh" smoke                   # First 1,000 messages
"$RUST_ROOT/scripts/run-rust.sh" benchmark               # First 300,000 messages
"$RUST_ROOT/scripts/run-rust.sh" benchmark --max-messages 100000
"$RUST_ROOT/scripts/run-rust.sh" benchmark --all          # Entire recording
"$CSHARP_ROOT/scripts/run-simulator.sh" compare           # Preferred paired test
```

The underlying simulator has no entry limit by default; scripts explicitly pass
300,000 (or 1,000 in smoke mode). Limits never modify the database. Benchmarks
choose one shared playback speed targeting approximately 60 seconds per replay;
`--speed 1` preserves original timing. Build before measuring; benchmark commands
do not compile. The shared coordinator manages the simulator automatically.

Manual replay: start `run-simulator.sh replay` first, then `run-rust.sh replay`.
Use `--replay-url` for another endpoint and `--fixture` for matching historical
starting state. Without a fixture, both languages estimate balances, instrument
rules, and starting books; these estimates do not reconstruct historical strategy
results.

Both applications default to the same **100 symbols in the same order**, defined
in C#'s `AssumptionZero.App/Program.cs` and Rust's
[src/symbols.json](src/symbols.json). Keep the lists and their order aligned when
changing the workload for fixture-free comparisons. With a shared fixture, both
applications use its instruments; the bundled BTC fixture is for the demo only.
Correctness hashes must agree before
comparing performance. Repository paths, database addresses, recording IDs, and
CPU IDs in the shared runbook are examples from the development machine.

## Controlled exchange benchmark

The new `SimulatedExchange` mode sends orders to a local C# exchange and receives
acknowledgements, rejections, partial/full fills, and cancellations. Both clients
update inventory and the shared risk ledger before subsequent decisions. The
normal replay and live modes retain their existing intent-only behavior.

The default comparison now offers **500,000 messages over 600 seconds per run**,
with 50,000 warmup messages, repeated deterministic equivalence checks, a baseline
pair, and five alternating measured pairs. This is about two hours plus preflight.
It uses recorded market data and controlled synthetic execution, with no Binance
calls. Async order counts can differ, so matching frames alone is not evidence of
an equal workload or a rewrite decision.

```bash
/home/sparrow/RiderProjects/AssumptionZero/scripts/run-closed-loop-benchmark.sh prepare
# Close other applications before the measurement run.
/home/sparrow/RiderProjects/AssumptionZero/scripts/run-closed-loop-benchmark.sh run \
  --max-messages 500000 --target-duration-seconds 600 \
  --app-cpus 2 --simulator-cpus 0,4,6,8,10,12,14
```

The CPU IDs above match the development machine. Preparation fingerprints the
build; the run writes reconciled results and an archive to return for analysis.
See [the closed-loop runbook in the C# repository](https://github.com/georgi2005atanasov/AssumptionZero.CSharp/blob/main/docs/closed-loop-benchmark.md) for protocol, measurement boundaries, readiness limits, and options.
The published October 2 figures below still describe the earlier market-only
benchmark; they are not results of this new order-cycle test.

## What the recorded comparison shows

The accepted session on **2 October 2026 at 19:39 UTC** used the same 300,000
recorded messages, five measured runs per language in alternating order, and the
same application CPU. Each replay was paced to 60 seconds; the first 30,000
messages were excluded as warm-up. Both applications completed all runs and
matched the checked decision and final-state hashes. Values below are medians
of five per-run measurements.

| Measurement | C# | Rust |
| --- | ---: | ---: |
| Process CPU time over about 54.35 measured seconds | 26.44 s | 5.28 s |
| Resident memory after replay, before reporting | 121.16 MiB | 59.32 MiB |
| Frame-processing P99 | 20.335 µs | 20.350 µs |
| Send-to-decision P99 | 367.368 µs | 340.990 µs |

Rust used **80% less process CPU and 51% less resident memory in this session**.
In absolute terms, that is about **0.39 of one CPU core on average and 62 MiB of
RAM** at the tested load. Frame-processing P99 was essentially tied; C# had lower
book-update, signal, and quote-stage P99 values. Rust's lower median
send-to-decision P99 had overlapping run ranges with C#, so it does not establish
a consistent latency advantage.

C# is an optimized implementation, and its larger measured process footprint
does not make it a worse choice. Process CPU includes transport, runtime, and
instrumentation work outside the trading calculations. The cause of the CPU gap
has not been established, and there were no measured C# GC collections or pauses.
Memory includes runtime state and benchmark buffers. The older 77% memory-saving
claim was withdrawn because sampling phases differed; the 51% figure uses the
corrected measurement boundary.

These results describe the saved builds and configuration. Both benchmark
applications had 100 configured instruments, matching the current default lists,
with only 5 ready at the end. The workload used synthetic starting state. Maximum
throughput, instrumentation CPU overhead, and production resource savings remain
unestablished.

The Rust result is promising, but this benchmark alone does not justify a rewrite.
If the C# application meets its latency and resource budgets, retaining it is a
reasonable choice. Profile the CPU difference and test representative workloads
before deciding whether the smaller footprint is worth migration and maintenance
costs.

Evidence in the C# repository: `docs/benchmark-accepted-2026-10-02.md` and
`benchmark-results/20261002T193939Z-simulator-compare-9g2o0V/`. The paired JSON,
per-run summaries, and sample CSVs are the source of these figures. Follow
`docs/testing.md` there for a new controlled comparison.

## Application modes

The binary is `target/release/assumption-zero`. It defaults to Replay.

```bash
EXECUTION_MODE=Replay REPLAY_URL=ws://127.0.0.1:5080/stream \
  "$RUST_ROOT/target/release/assumption-zero"

# Live modes require BINANCE_API_KEY. NormalExecution also requires
# BINANCE_ED25519_PRIVATE_KEY_PATH pointing to the Ed25519 PKCS#8 PEM key.
EXECUTION_MODE=SeedData "$RUST_ROOT/target/release/assumption-zero"
EXECUTION_MODE=NormalExecution "$RUST_ROOT/target/release/assumption-zero"
```

SeedData defaults to the existing local PostgreSQL connection and uses the C#
`AssumptionZero.App/init.sql` table. `PG_CONNECTION` is an optional override.
It uses the same recording columns, binary payloads, sequence numbering, and
monotonic nanosecond units. Ctrl+C stops reception and drains queued inserts.
Local PostgreSQL accepts native postgres connection strings or Npgsql-style
semicolon strings. PostgreSQL TLS uses the system certificate store; the explicit
`Trust Server Certificate=true` setting is honored when present. Connection pooling
is unnecessary for the single long-lived recording writer.

NormalExecution loads filters, balances, open orders and depth, and subscribes
to account events. It fails closed on lost reconciliation and resynchronizes the
market feed. Live adapter behavior is tested against local protocol fixtures;
external Binance testnet availability/credentials are not established by those
tests. Optional `BINANCE_HTTP_URL`, `BINANCE_ACCOUNT_URL`, and `BINANCE_SBE_URL`
overrides support local adapter tests.

`LOG_EVENTS` defaults to `false`. `MEASURE_ALLOCATIONS=1` reports requested
allocator bytes across 100 ready decisions after 100 ready warm-up decisions;
this is process-wide and includes transport. Core allocation assertions are
isolated tests. `CORRECTNESS_TRACE=PATH` writes decision tuples for debugging and
must stay off during performance measurement.

## Implementation and validation

- `domain`: checked decimal grid conversion, integer book, event-time signal
  history, quote formulas and instrument filters.
- `runtime`: single market writer, shared account ledger, reservations,
  acknowledgement/replacement/cancellation and fill state.
- `feed`: bounded SBE validation and reusable decode scratch storage.
- `live`: WebSocket transport, testnet bootstrap/signing, account queue and recorder.
- `metrics`: Linux monotonic clocks, frame/stage samples, CPU/RSS, requested
  allocator-byte counter, decision and final discrete-state hashes.

Rust uses decimal arithmetic for financial values and f64 for the same signal
formulas as C#. Checked-in numeric vectors come from the C# DecimalReferenceVectors
test. Intermediate hand-calculated floating values use 1e-12 tolerance; final
integer decisions and discrete state must match exactly. Cross-language integration
tests live in the C# suite; set `RUST_APP_TEST` to this Release binary and
`PG_TEST_CONNECTION` for isolated database coverage.

This baseline uses standard Cargo Release settings and the system allocator.
No fast-math, alternative algorithm, or Rust-specific reduced workload is used.
Dependencies are pinned in Cargo.lock. Benchmark instrumentation targets Linux.
Results determine where this implementation wins or loses; smoke tests establish
correctness, not performance.
