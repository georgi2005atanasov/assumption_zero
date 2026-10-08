# AssumptionZero — Rust

A Rust market-processing application that decodes Binance SBE messages, maintains
instrument books and signals, and produces quote intents. Replay and live modes
produce intents; the local simulated exchange also exercises order submission,
acknowledgements, fills, and cancellations.

The [C# repository](https://github.com/georgi2005atanasov/AssumptionZero.CSharp)
provides the shared simulator, demo fixture, and cross-language integration tests.
Rust builds and runs its own tests independently of that repository.

## Build and test

Install Rust through rustup and a native C compiler/linker. On Linux, also install
your distribution's CA certificates for live HTTPS connections. The repository
selects the stable toolchain in `rust-toolchain.toml`.

```bash
git clone https://github.com/georgi2005atanasov/assumption_zero.git
cd assumption_zero
cargo +stable build --release --locked
cargo +stable test --release --locked
```

The executable is `target/release/assumption-zero`. Tests cover calculations,
book and signal updates, reservations, order transitions, decoding, and warmed
core allocations. They need no database, API keys, or running simulator. Initial
builds download the toolchain and dependencies; `--locked` uses `Cargo.lock`.

## Set up the simulator

For an end-to-end demo, install the .NET 10 SDK and OpenSSL, and clone the C#
repository. Set these paths to your actual checkouts; use absolute paths in every
terminal that needs them.

```bash
export RUST_ROOT="$PWD"
export CSHARP_ROOT="$HOME/projects/AssumptionZero.CSharp"
git clone https://github.com/georgi2005atanasov/AssumptionZero.CSharp.git "$CSHARP_ROOT"
export DOTNET_HOST_PATH="$(command -v dotnet)"
dotnet build "$CSHARP_ROOT/AssumptionZero.sln" -c Release
```

Skip the clone if the C# repository already exists. If .NET is installed under
`$HOME/.dotnet`, add it to `PATH` before these commands. Check `dotnet --list-sdks`
for a 10.x SDK. `DOTNET_HOST_PATH` selects the executable used by tests and launchers.

## Run the replay demo

In terminal 1, start the simulator:

```bash
dotnet "$CSHARP_ROOT/AssumptionZero.Simulator/bin/Release/net10.0/AssumptionZero.Simulator.dll" demo
```

Wait for `Listening on http://127.0.0.1:5080/stream`. In terminal 2, set the same
`CSHARP_ROOT` and `RUST_ROOT` paths and run:

```bash
EXECUTION_MODE=Replay \
REPLAY_URL=ws://127.0.0.1:5080/stream \
REPLAY_FIXTURE="$CSHARP_ROOT/AssumptionZero.App/replay-fixture.json" \
  "$RUST_ROOT/target/release/assumption-zero"
```

No database or Binance credentials are needed. The application prints
`Replay complete`, product readiness, and a final-state hash, then exits. The
simulator stays running; each new connection replays from the beginning. Stop it
with Ctrl+C. The bundled BTCUSDT fixture is intended for this demo only.

## Run integration tests with C#

After building both Release applications, run the C# test suite with the Rust
binary configured:

```bash
export RUST_APP_TEST="$RUST_ROOT/target/release/assumption-zero"
export DOTNET_HOST_PATH="$(command -v dotnet)"
dotnet test "$CSHARP_ROOT/AssumptionZero.Tests/AssumptionZero.Tests.csproj" -c Release --no-build
```

These tests exercise cross-language replay, local live-adapter fixtures, and
simulated order handling. OpenSSL must be on `PATH`. They do not contact Binance.
Rust tests are skipped if `RUST_APP_TEST` is missing; database tests are skipped
unless `PG_TEST_CONNECTION` is set.

To enable database coverage, start the optional PostgreSQL service with Docker
Compose, then rerun the same test command:

```bash
docker compose -f "$CSHARP_ROOT/compose.yaml" up -d --wait database
export PG_TEST_CONNECTION='Host=localhost;Port=5433;Database=assumption_zero;Username=postgres;Password=local_test_password'
dotnet test "$CSHARP_ROOT/AssumptionZero.Tests/AssumptionZero.Tests.csproj" -c Release --no-build
docker compose -f "$CSHARP_ROOT/compose.yaml" --profile database down
```

The tests create and remove their own schemas; an external test database must
allow schema creation and removal. The Compose database volume persists after
shutdown. Set `POSTGRES_PORT` before starting Compose if port 5433 is occupied,
and update the connection string accordingly.

## Run the local order-cycle demo

With both projects built and `DOTNET_HOST_PATH` set:

```bash
dotnet "$CSHARP_ROOT/AssumptionZero.Simulator/bin/Release/net10.0/AssumptionZero.Simulator.dll" closed-loop \
  --recording demo --max-messages 120 --target-duration-seconds 1 \
  --repetitions 1 --scenario mixed --language both --rust-root "$RUST_ROOT"
```

The runner starts the exchange and clients, checks deterministic results, and
exercises asynchronous settlement. It prints reconciliation status and the
results directory, then exits. No database or API key is required. Use
`--scenario rest` for resting orders, or `--language Rust` to run only Rust.

## Live data and recorded replays

| `EXECUTION_MODE` | Behavior | Requirements |
| --- | --- | --- |
| `Replay` (default) | Process messages from the shared simulator | Running simulator; matching fixture optional |
| `SimulatedExchange` | Local order/report cycle | Use the order-cycle command above |
| `SeedData` | Record live SBE market messages into PostgreSQL | Testnet API key and initialized database |
| `NormalExecution` | Load snapshots and process live market/account events | Testnet API key and matching Ed25519 private key |

For live processing, register an Ed25519 public key with Binance Spot Testnet and
set its API key and unencrypted PKCS#8 PEM private-key path:

```bash
export BINANCE_API_KEY='YOUR_TESTNET_API_KEY'
export BINANCE_ED25519_PRIVATE_KEY_PATH='/absolute/path/to/your-private-key.pem'
EXECUTION_MODE=NormalExecution "$RUST_ROOT/target/release/assumption-zero"
```

For capture, keep `BINANCE_API_KEY` set, start the Compose database above, and run:

```bash
export PG_CONNECTION='Host=localhost;Port=5433;Database=assumption_zero;Username=postgres;Password=local_test_password'
EXECUTION_MODE=SeedData "$RUST_ROOT/target/release/assumption-zero"
```

Compose initializes the recording table on its first database start. For your own
new database, apply `AssumptionZero.App/init.sql` from the C# repository once.
Always set `PG_CONNECTION` for your environment; the built-in default is specific
to the development machine. Stop capture with Ctrl+C and wait for queued writes
to finish.

To replay a completed recording, keep `PG_CONNECTION` set in the simulator
terminal, list recordings, then replace `RECORDING_UUID` with the selected ID:

```bash
dotnet "$CSHARP_ROOT/AssumptionZero.Simulator/bin/Release/net10.0/AssumptionZero.Simulator.dll" list
dotnet "$CSHARP_ROOT/AssumptionZero.Simulator/bin/Release/net10.0/AssumptionZero.Simulator.dll" replay RECORDING_UUID
```

Connect the Rust application as in the demo, using a fixture matching that
recording, or omit `REPLAY_FIXTURE` to use estimated starting state. Estimated
balances and books do not reconstruct historical strategy decisions. Set
`LOG_EVENTS=true` to inspect decoded events; it defaults to `false`.

## Further documentation

The shared documentation lives in the C# checkout under `docs/`:

- [Recording and replay](https://github.com/georgi2005atanasov/AssumptionZero.CSharp/blob/main/docs/replay.md)
- [Benchmark setup and execution](https://github.com/georgi2005atanasov/AssumptionZero.CSharp/blob/main/docs/testing.md)
- [Controlled exchange runbook](https://github.com/georgi2005atanasov/AssumptionZero.CSharp/blob/main/docs/closed-loop-benchmark.md)
- [Recorded comparison report](https://github.com/georgi2005atanasov/AssumptionZero.CSharp/blob/main/docs/benchmark-accepted-2026-10-02.md)
