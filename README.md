# AssumptionZero — Rust

Rust implementation of the C# AssumptionZero market-processing application.
The existing C# simulator supplies identical SBE messages to both applications;
there is no separate Rust simulator.

The flow is receive/reassemble → SBE validation/decode → bounded book → signals →
pricing and risk → quote intents. Defaults, synthetic initialization, fixture
format, numeric units, account reservations, fills, and order-state transitions
match C#. Replay never submits orders; live mode also produces intents only.

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
# The existing local PostgreSQL connection is built in; no exports needed.
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
starting state. Otherwise, both languages use identical synthetic initialization.

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
