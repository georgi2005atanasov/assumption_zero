//! Local controlled exchange client. Trading state has one owner; socket tasks only queue data.
use crate::{
    domain::*,
    feed::Feed,
    metrics::{Latency, ticks},
    runtime::{Ledger, Runtime},
};
use anyhow::{Context, Result, bail, ensure};
use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, str::FromStr};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::{connect_async, tungstenite::Message};
const CAPACITY: usize = 8192;
#[derive(Clone, Debug, Deserialize)]
struct Spec {
    id: i64,
    symbol: String,
    tick: String,
    step: String,
}
#[derive(Clone, Debug, Deserialize)]
struct Manifest {
    version: i32,
    mode: String,
    frames: usize,
    warmup: usize,
    fee: String,
    instruments: Vec<Spec>,
    assets: Vec<String>,
    initial_free: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Command {
    id: i64,
    intent: i64,
    instrument: usize,
    buy: bool,
    action: String,
    order_id: i64,
    price: i64,
    quantity: i64,
    event_time: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Balance {
    asset: usize,
    free: String,
    locked: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Event {
    sequence: i64,
    command_id: i64,
    intent: i64,
    instrument: usize,
    buy: bool,
    order_id: i64,
    kind: String,
    status: u8,
    filled: i64,
    price: i64,
    reason: String,
    balances: Vec<Balance>,
    server_ticks: i64,
}
#[derive(Default, Deserialize)]
struct Packet {
    kind: String,
    #[serde(default)]
    events: Option<Vec<Event>>,
    manifest: Option<Manifest>,
}
#[derive(Clone, Copy)]
struct Pending {
    intent: i64,
    action: Action,
    price: i64,
    quantity: i64,
}
struct Trader {
    measure: bool,
    command_times: HashMap<i64, i64>,
    response_times: Vec<i64>,
    cancels: HashMap<i64, Command>,
    pending: HashMap<(usize, bool), Pending>,
    next: i64,
    sequence: i64,
    previous: Option<String>,
    outbox: Vec<Command>,
    time: Vec<i64>,
    trace: Sha256,
    decisions: usize,
    ready: usize,
}
impl Trader {
    fn new(n: usize) -> Self {
        Self {
            measure: false,
            command_times: HashMap::new(),
            response_times: vec![],
            cancels: HashMap::new(),
            pending: HashMap::new(),
            next: 0,
            sequence: 0,
            previous: None,
            outbox: vec![],
            time: vec![0; n],
            trace: Sha256::new(),
            decisions: 0,
            ready: 0,
        }
    }
    fn trace(&mut self, s: String) {
        self.trace.update(s.as_bytes());
        self.trace.update(b"\n");
    }
    #[allow(clippy::too_many_arguments)]
    fn submit(
        &mut self,
        i: usize,
        buy: bool,
        action: &str,
        order: i64,
        price: i64,
        quantity: i64,
        intent: i64,
    ) {
        self.next += 1;
        let id = self.next;
        self.trace(format!(
            "C|{id}|{intent}|{i}|{}|{action}|{order}|{price}|{quantity}",
            u8::from(buy)
        ));
        let c = Command {
            id,
            intent,
            instrument: i,
            buy,
            action: action.into(),
            order_id: order,
            price,
            quantity,
            event_time: self.time[i],
        };
        if action == "cancel" {
            self.cancels.insert(id, c.clone());
        }
        if self.measure {
            self.command_times.insert(id, ticks());
        }
        self.outbox.push(c);
    }
    fn decide(&mut self, i: usize, f: &mut Feed) {
        self.decisions += 1;
        if f.runtimes[i].ready(&f.ledger) {
            self.ready += 1;
        }
        for buy in [true, false] {
            let r = &mut f.runtimes[i];
            let d = r.evaluate(&f.ledger);
            let action = if buy { d.bid_action } else { d.ask_action };
            if self.pending.contains_key(&(i, buy))
                || !matches!(action, Action::Place | Action::Replace | Action::Cancel)
            {
                continue;
            }
            let Some(intent) = r.accept_intent(buy, d, &mut f.ledger) else {
                continue;
            };
            let order = r.orders[usize::from(!buy)];
            let (price, quantity) = if buy {
                (d.bid, d.bid_qty)
            } else {
                (d.ask, d.ask_qty)
            };
            self.pending.insert(
                (i, buy),
                Pending {
                    intent,
                    action,
                    price,
                    quantity,
                },
            );
            self.submit(
                i,
                buy,
                if action == Action::Place {
                    "place"
                } else {
                    "cancel"
                },
                order.id,
                price,
                quantity,
                intent,
            );
        }
    }
    fn apply(&mut self, e: Event, f: &mut Feed, finishing: bool) -> Result<()> {
        let mut canonical = e.clone();
        canonical.server_ticks = 0;
        let fingerprint = serde_json::to_string(&canonical)?;
        if e.sequence == self.sequence && self.previous.as_ref() == Some(&fingerprint) {
            return Ok(());
        }
        ensure!(
            e.sequence == self.sequence + 1 && e.instrument < f.runtimes.len(),
            "execution sequence/instrument mismatch"
        );
        let key = (e.instrument, e.buy);
        let p = self.pending.get(&key).copied();
        let r = &mut f.runtimes[e.instrument];
        if e.kind == "reject" {
            let current = r.orders[usize::from(!e.buy)];
            let terminal_cancel = self.cancels.remove(&e.command_id).is_some_and(|c| {
                c.intent == e.intent
                    && c.order_id == e.order_id
                    && (!current.active || current.id != e.order_id)
            });
            if !terminal_cancel {
                ensure!(
                    p.is_some_and(|p| p.intent == e.intent),
                    "unmatched rejection"
                );
                r.reject_intent(e.buy, e.intent, &mut f.ledger);
                self.pending.remove(&key);
            }
        } else {
            if e.kind == "ack" {
                ensure!(
                    p.is_some_and(|p| p.intent == e.intent)
                        && r.acknowledge(e.buy, e.intent, e.order_id),
                    "unmatched acknowledgement"
                );
                r.confirm_intent(e.buy, e.intent, &mut f.ledger)?;
                self.pending.remove(&key);
            }
            ensure!(
                e.kind == "ack" || e.kind == "execution",
                "unknown execution kind"
            );
            if e.status == 5 {
                self.cancels.remove(&e.command_id);
            }
            let before = r.orders[usize::from(!e.buy)];
            ensure!(
                before.id == e.order_id && e.filled >= before.filled && e.filled <= before.quantity,
                "invalid execution quantities/order ID"
            );
            let status = match e.status {
                0 => Status::PendingNew,
                1 => Status::PendingCancel,
                2 => Status::New,
                3 => Status::PartiallyFilled,
                4 => Status::Filled,
                5 => Status::Cancelled,
                6 => Status::Rejected,
                7 => Status::Expired,
                _ => bail!("invalid status"),
            };
            r.execution(
                e.buy,
                e.order_id,
                e.filled,
                e.price,
                self.time[e.instrument],
                status,
                &mut f.ledger,
            );
            if status == Status::Cancelled && p.is_some_and(|p| p.action == Action::Replace) {
                let p = p.unwrap();
                if finishing {
                    r.reject_intent(e.buy, p.intent, &mut f.ledger);
                    self.pending.remove(&key);
                } else {
                    self.pending.insert(
                        key,
                        Pending {
                            action: Action::Place,
                            ..p
                        },
                    );
                    self.submit(
                        e.instrument,
                        e.buy,
                        "place",
                        0,
                        p.price,
                        p.quantity,
                        p.intent,
                    );
                }
            } else if matches!(status, Status::Cancelled | Status::Filled | Status::Expired) {
                self.pending.remove(&key);
            }
        }
        for b in &e.balances {
            ensure!(b.asset < f.ledger.assets.len(), "unknown balance asset");
            f.ledger
                .set_balance(b.asset, decimal(&b.free)?, decimal(&b.locked)?);
            // None updates inventory without acknowledging unrelated pending submissions.
            for r in &mut f.runtimes {
                r.balance_changed(&mut f.ledger, None);
            }
        }
        ensure!(f.ledger.ready, "account reconciliation failed");
        if let Some(created) = self.command_times.remove(&e.command_id)
            && self.measure
        {
            self.response_times.push(ticks() - created);
        }
        self.sequence = e.sequence;
        self.previous = Some(fingerprint);
        self.trace(format!(
            "E|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
            e.sequence,
            e.command_id,
            e.intent,
            e.instrument,
            u8::from(e.buy),
            e.order_id,
            e.kind,
            e.status,
            e.filled,
            e.price,
            e.reason
        ));
        for b in e.balances {
            self.trace(format!("B|{}|{}|{}", b.asset, b.free, b.locked));
        }
        Ok(())
    }
    fn settled(&self, f: &Feed) -> Result<()> {
        ensure!(
            f.ledger.pending_count() == 0
                && self.pending.is_empty()
                && self.cancels.is_empty()
                && f.runtimes
                    .iter()
                    .all(|r| r.orders.iter().all(|o| !o.active)),
            "unsettled client state"
        );
        Ok(())
    }
}
fn decimal(s: &str) -> Result<Decimal> {
    Ok(Decimal::from_str(s)?)
}
fn money(d: Decimal) -> String {
    d.normalize().to_string()
}
fn state(f: &Feed) -> Value {
    json!({"balances":f.ledger.assets.iter().enumerate().map(|(i,a)|json!({"asset":a,"free":money(f.ledger.raw_free(i)),"locked":money(f.ledger.locked(i)),"reserved":money(f.ledger.reserved(i))})).collect::<Vec<_>>(),"pending":f.ledger.pending_count(),"instruments":f.runtimes.iter().map(|r|{let order=|o:Order|[o.id,o.price,o.quantity,o.filled,o.status as i64,i64::from(o.active)];json!({"symbol":r.instrument.symbol,"inventory":r.inventory.current,"bid":order(r.orders[0]),"ask":order(r.orders[1]),"ready":r.ready(&f.ledger),"book":r.book.synchronized,"warm":r.memory.warm,"faulted":r.memory.faulted})}).collect::<Vec<_>>()})
}
fn cpu() -> i64 {
    let mut t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut t) },
        0
    );
    t.tv_sec * 1_000_000_000 + t.tv_nsec
}
fn rss() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap_or_default()
        .lines()
        .find_map(|l| {
            l.strip_prefix("VmRSS:")
                .and_then(|s| s.split_whitespace().next()?.parse::<u64>().ok())
        })
        .unwrap_or(0)
        * 1024
}
fn summary(v: Vec<i64>) -> Value {
    let l = Latency::from(v);
    json!({"count":l.count,"p50":l.p50,"p99":l.p99,"p999":l.p999,"maximum":l.maximum})
}
type Out = (Value, Option<oneshot::Sender<()>>);
fn queue(tx: &mpsc::Sender<Out>, p: Value) -> Result<()> {
    tx.try_send((p, None))
        .map_err(|_| anyhow::anyhow!("outbound queue closed or full"))
}
fn commands(t: &mut Trader, step: i64) -> Value {
    json!({"kind":"commands","step":step,"commands":std::mem::take(&mut t.outbox)})
}
#[derive(Clone, Copy)]
enum ExchangeChannel {
    Orders,
    Market,
}
impl ExchangeChannel {
    fn name(self) -> &'static str {
        match self {
            Self::Orders => "orders",
            Self::Market => "market",
        }
    }
}

/// Only application messages reach the JSON/SBE decoders. Tungstenite handles
/// ping replies while the stream continues to be polled; keep-alives carry no
/// trading events and must not advance any application counters or watermarks.
async fn next_application_message<S>(
    stream: &mut S,
    channel: ExchangeChannel,
) -> Result<tokio_tungstenite::tungstenite::Bytes>
where
    S: futures_util::Stream<
            Item = std::result::Result<Message, tokio_tungstenite::tungstenite::Error>,
        > + Unpin,
{
    loop {
        let message = stream
            .next()
            .await
            .with_context(|| {
                format!(
                    "{} WebSocket ended before session completion",
                    channel.name()
                )
            })?
            .with_context(|| format!("{} WebSocket receive failed", channel.name()))?;
        match (channel, message) {
            (_, Message::Ping(_) | Message::Pong(_)) => continue,
            (_, Message::Close(frame)) => bail!(
                "{} WebSocket closed before session completion: {frame:?}",
                channel.name()
            ),
            (ExchangeChannel::Orders, Message::Text(text)) => return Ok(text.into()),
            (ExchangeChannel::Market, Message::Binary(bytes)) => return Ok(bytes),
            _ => bail!(
                "unexpected application message type on {} WebSocket",
                channel.name()
            ),
        }
    }
}

pub async fn run() -> Result<()> {
    let measure = std::env::var("SIM_MEASURE").unwrap_or_default() != "false";
    let url = std::env::var("SIM_EXCHANGE_URL")?;
    let output = std::env::var("SIM_OUTPUT")?;
    let (mut os, _) = connect_async(format!("{url}/orders")).await?;
    if let tokio_tungstenite::MaybeTlsStream::Plain(tcp) = os.get_ref() {
        tcp.set_nodelay(true)?;
    }
    let hello: Packet =
        serde_json::from_slice(&next_application_message(&mut os, ExchangeChannel::Orders).await?)
            .context("invalid JSON in exchange manifest")?;
    ensure!(hello.kind == "hello", "expected manifest handshake");
    let m = hello.manifest.context("manifest missing")?;
    ensure!(m.version == 1, "protocol version mismatch");
    let mut ledger = Ledger::new(m.assets.clone());
    ledger.begin(decimal(&m.fee)?);
    for (i, x) in m.initial_free.iter().enumerate() {
        ledger.set_balance(i, decimal(x)?, Decimal::ZERO);
    }
    ledger.ready = true;
    let mut rs = vec![];
    for spec in &m.instruments {
        let i = Instrument {
            id: spec.id,
            symbol: spec.symbol.clone(),
            base: spec.symbol.trim_end_matches("USDT").into(),
            quote: "USDT".into(),
            tick: decimal(&spec.tick)?,
            step: decimal(&spec.step)?,
            min_qty: decimal(&spec.step)?,
            max_qty: Decimal::from(1000000),
            min_notional: Decimal::ZERO,
            min_price: Decimal::ZERO,
            max_price: Decimal::MAX,
        };
        let c = Config::default();
        let target = i.quantity_units(Decimal::from(100));
        let mut r = Runtime::new(
            i,
            c,
            &ledger,
            target,
            target + c.base_quantity * 10,
            0,
            true,
        );
        r.balance_changed(&mut ledger, None);
        rs.push(r);
    }
    let mut feed = Feed::new(ledger, rs, true, false);
    feed.simulation_decisions = Some(vec![]);
    let mut trader = Trader::new(m.instruments.len());
    let (sink, stream) = os.split();
    let (tx, mut outbound) = mpsc::channel::<Out>(CAPACITY);
    let (report_tx, mut reports) = mpsc::channel::<Result<Packet>>(CAPACITY);
    let mut writer = tokio::spawn(async move {
        let mut sink = sink;
        while let Some((p, done)) = outbound.recv().await {
            sink.send(Message::Text(serde_json::to_string(&p)?.into()))
                .await?;
            if let Some(done) = done {
                let _ = done.send(());
            }
        }
        Ok::<_, anyhow::Error>(())
    });
    let reader = tokio::spawn(async move {
        let mut stream = stream;
        loop {
            let packet = next_application_message(&mut stream, ExchangeChannel::Orders)
                .await
                .and_then(|data| {
                    let p = serde_json::from_slice::<Packet>(&data)
                        .context("invalid JSON in execution report")?;
                    ensure!(p.kind == "reports", "unexpected exchange response");
                    Ok(p)
                });
            let failed = packet.is_err();
            if report_tx.try_send(packet).is_err() {
                return Err(anyhow::anyhow!("inbound report queue closed or full"));
            }
            if failed {
                return Ok::<_, anyhow::Error>(());
            }
        }
    });
    let (market, _) = connect_async(format!("{url}/market")).await?;
    if let tokio_tungstenite::MaybeTlsStream::Plain(tcp) = market.get_ref() {
        tcp.set_nodelay(true)?;
    }
    let (_, mut market) = market.split();
    let (market_tx, mut markets) = mpsc::channel::<Result<Vec<u8>>>(CAPACITY);
    let market_reader = tokio::spawn(async move {
        loop {
            let value = next_application_message(&mut market, ExchangeChannel::Market)
                .await
                .map(|bytes| bytes.to_vec());
            let failed = value.is_err();
            if market_tx.try_send(value).is_err() {
                return Err(anyhow::anyhow!("market queue closed or full"));
            }
            if failed {
                return Ok::<_, anyhow::Error>(());
            }
        }
    });
    let started = std::time::Instant::now();
    let mut frames = 0usize;
    let (mut cpu_start, mut measured_start, mut last_market) = (0, 0, 0);
    let (mut measured_end, mut cpu_end, mut allocated_end) = (0, 0, 0);
    let mut allocated_start = 0;
    let mut finishing = false;
    let mut processing = vec![];
    let mut applications = vec![];
    let mut delivery = vec![];
    let mut readiness = vec![[0u64; 4]; m.instruments.len()];
    let mut high = [0usize; 3];
    let mut held = None;
    let result: Result<Value> = async {
        loop {
            trader.measure = measure && !finishing && frames >= m.warmup;
            high[0] = high[0].max(markets.len());
            high[1] = high[1].max(reports.len());
            high[2] = high[2].max(CAPACITY - tx.capacity());
            if m.mode == "async" {
                while let Ok(p) = reports.try_recv() {
                    for e in p?.events.unwrap_or_default() {
                        let t = ticks();
                        let sent = e.server_ticks;
                        trader.apply(e, &mut feed, finishing)?;
                        if measure && !finishing && frames > m.warmup {
                            applications.push(ticks() - t);
                            delivery.push(ticks() - sent);
                        }
                    }
                }
                if !finishing && !trader.outbox.is_empty() {
                    let p = commands(&mut trader, frames as i64);
                    queue(&tx, p)?;
                }
            }
            if held.is_none() {
                tokio::select! {
                    value = markets.recv() => {
                        held = Some(value.context("market connection closed")??);
                    },
                    p = reports.recv(), if m.mode == "async" => {
                        for e in p.context("report connection closed")??.events.unwrap_or_default() {
                            let t = ticks();
                            let sent = e.server_ticks;
                            trader.apply(e, &mut feed, finishing)?;
                            if measure && !finishing && frames > m.warmup {
                                applications.push(ticks() - t);
                                delivery.push(ticks() - sent);
                            }
                        }
                        continue;
                    },
                    r = &mut writer => { r??; bail!("order writer stopped"); },
                    _ = tokio::signal::ctrl_c() => bail!("simulation interrupted"),
                }
            }
            let data = held.as_ref().unwrap();
            ensure!(data.len() >= 24, "short market envelope");
            let step = i64::from_le_bytes(data[0..8].try_into()?);
            let watermark = i64::from_le_bytes(data[8..16].try_into()?);
            if step == 0 {
                finishing = true;
                trader.measure = false;
                measured_end = ticks();
                cpu_end = cpu();
                allocated_end = crate::metrics::allocated();
            }
            if m.mode == "deterministic" || step == -1 {
                while trader.sequence < watermark {
                    let p = tokio::time::timeout(std::time::Duration::from_secs(30), reports.recv())
                        .await.context("execution watermark timed out")?
                        .context("missing report watermark")??;
                    for e in p.events.unwrap_or_default() {
                        trader.apply(e, &mut feed, finishing)?;
                    }
                }
            }
            if step == -1 {
                trader.settled(&feed)?;
                let (done, wait) = oneshot::channel();
                tx.send((json!({"kind":"settled"}), Some(done))).await?;
                wait.await?;
                break;
            }
            if step == 0 {
                trader.outbox.clear();
                queue(&tx, json!({"kind":"finish"}))?;
                held = None;
                continue;
            }
            ensure!(step == frames as i64 + 1, "market sequence mismatch");
            let begin = ticks();
            feed.binary(&data[24..])?;
            let event_time = i64::from_le_bytes(data[32..40].try_into()?)
                .checked_mul(1000).context("time overflow")?;
            let mut touched = std::mem::take(feed.simulation_decisions.as_mut().unwrap());
            for &i in &touched {
                trader.time[i] = trader.time[i].max(event_time);
                trader.decide(i, &mut feed);
                let r = &feed.runtimes[i];
                let reason = if r.ready(&feed.ledger) { 0 }
                    else if !r.book.synchronized { 1 }
                    else if r.memory.faulted { 3 } else { 2 };
                readiness[i][reason] += 1;
            }
            touched.clear();
            *feed.simulation_decisions.as_mut().unwrap() = touched;
            frames += 1;
            last_market = ticks();
            if measure && frames > m.warmup { processing.push(last_market - begin); }
            if frames == m.warmup {
                measured_start = last_market;
                cpu_start = cpu();
                if measure { crate::metrics::track_allocations(); }
                allocated_start = crate::metrics::allocated();
            }
            if m.mode == "deterministic" || !trader.outbox.is_empty() {
                let p = commands(&mut trader, frames as i64);
                queue(&tx, p)?;
            }
            held = None;
        }
        ensure!(frames == m.frames, "incomplete frame count");
        let cpu_ns = cpu_end - cpu_start;
        let drain_cpu = cpu() - cpu_end;
        let memory = rss();
        let drain = ticks() - measured_end;
        let wall_seconds = started.elapsed().as_secs_f64();
        if measure {
            use std::io::Write;
            let mut file = std::io::BufWriter::new(std::fs::File::create(format!("{output}.samples.csv"))?);
            writeln!(file, "metric,index,nanoseconds")?;
            for (name, samples) in [
                ("frame_processing", &processing), ("report_application", &applications),
                ("command_creation_to_response_apply", &trader.response_times),
                ("report_delivery_and_apply", &delivery),
            ] {
                for (i, s) in samples.iter().enumerate() { writeln!(file, "{name},{i},{s}")?; }
            }
        }
        Ok(json!({
            "schema_version":1, "instrumented":measure, "language":"Rust", "mode":m.mode,
            "completed":true, "frames":frames, "commands":trader.next, "reports":trader.sequence,
            "decisions":trader.decisions, "ready_decisions":trader.ready,
            "trace_hash":format!("{:x}",trader.trace.clone().finalize()), "state":state(&feed),
            "cpu_nanoseconds":cpu_ns, "resident_bytes":memory,
            "memory_snapshot_phase":"after_drain_before_report",
            "allocated_bytes":if measure { Some(allocated_end-allocated_start) } else { None },
            "drain_cpu_nanoseconds":drain_cpu, "measured_duration_ns":measured_end-measured_start,
            "drain_duration_ns":drain, "wall_seconds":wall_seconds,
            "processing_ns":summary(processing), "report_application_ns":summary(applications),
            "command_creation_to_response_apply_ns":summary(trader.response_times),
            "report_delivery_and_apply_ns":summary(delivery),
            "queue_high_water":{"market":high[0],"reports":high[1],"outbound":high[2]},
            "readiness":m.instruments.iter().enumerate().map(|(i,s)|json!({
                "symbol":s.symbol,"ready":readiness[i][0],"book_unready":readiness[i][1],
                "warming":readiness[i][2],"signal_fault":readiness[i][3]
            })).collect::<Vec<_>>()
        }))
    }.await;
    reader.abort();
    market_reader.abort();
    writer.abort();
    let report = result?;
    std::fs::write(output, serde_json::to_vec(&report)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn keepalives_are_filtered_but_data_and_close_frames_are_not() {
        for channel in [ExchangeChannel::Orders, ExchangeChannel::Market] {
            let payload = match channel {
                ExchangeChannel::Orders => Message::Text("{\"kind\":\"reports\"}".into()),
                ExchangeChannel::Market => Message::Binary(vec![1, 2, 3].into()),
            };
            let expected = payload.clone().into_data().to_vec();
            let mut stream = futures_util::stream::iter([
                Ok(Message::Pong(vec![].into())),
                Ok(Message::Ping(vec![7].into())),
                Ok(Message::Pong(vec![7].into())),
                Ok(payload),
                Ok(Message::Close(None)),
            ]);
            assert_eq!(
                next_application_message(&mut stream, channel)
                    .await
                    .unwrap(),
                expected
            );
            let error = next_application_message(&mut stream, channel)
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("closed before session completion"));
        }
        // An empty application message is still invalid JSON, not a keep-alive.
        let mut empty = futures_util::stream::iter([Ok(Message::Text("".into()))]);
        let data = next_application_message(&mut empty, ExchangeChannel::Orders)
            .await
            .unwrap();
        assert!(serde_json::from_slice::<Packet>(&data).is_err());
        let mut wrong = futures_util::stream::iter([Ok(Message::Text("{}".into()))]);
        assert!(
            next_application_message(&mut wrong, ExchangeChannel::Market)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn socket_reader_answers_pings_and_skips_unsolicited_pongs() {
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            for channel in [ExchangeChannel::Orders, ExchangeChannel::Market] {
                socket.send(Message::Ping(vec![9, 8].into())).await.unwrap();
                let reply = tokio::time::timeout(std::time::Duration::from_secs(2), socket.next())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                assert_eq!(reply, Message::Pong(vec![9, 8].into()));
                socket.send(Message::Pong(vec![].into())).await.unwrap();
                socket
                    .send(match channel {
                        ExchangeChannel::Orders => Message::Text("{}".into()),
                        ExchangeChannel::Market => Message::Binary(vec![1, 2, 3].into()),
                    })
                    .await
                    .unwrap();
            }
        });
        let (socket, _) = connect_async(format!("ws://{address}")).await.unwrap();
        // Production readers poll split streams while another task owns the sink.
        let (_sink, mut stream) = socket.split();
        assert_eq!(
            next_application_message(&mut stream, ExchangeChannel::Orders)
                .await
                .unwrap(),
            b"{}".as_slice()
        );
        assert_eq!(
            next_application_message(&mut stream, ExchangeChannel::Market)
                .await
                .unwrap(),
            vec![1, 2, 3]
        );
        server.await.unwrap();
    }

    #[test]
    fn fill_then_late_cancel_rejection_is_safe_and_duplicate_is_idempotent() {
        let (ledger, runtimes) = crate::state::synthetic();
        let mut f = Feed::new(ledger, runtimes, true, false);
        f.runtimes[0].orders[0] = Order {
            id: 42,
            price: 100,
            quantity: 10,
            filled: 0,
            status: Status::New,
            active: true,
        };
        let mut t = Trader::new(f.runtimes.len());
        t.decide(0, &mut f);
        assert_eq!(t.outbox.len(), 1);
        let cancel = t.outbox[0].clone();
        assert_eq!(cancel.action, "cancel");
        t.apply(
            Event {
                sequence: 1,
                command_id: 99,
                intent: 99,
                instrument: 0,
                buy: true,
                order_id: 42,
                kind: "execution".into(),
                status: 4,
                filled: 10,
                price: 100,
                reason: String::new(),
                balances: vec![],
                server_ticks: 0,
            },
            &mut f,
            false,
        )
        .unwrap();
        let reject = Event {
            sequence: 2,
            command_id: cancel.id,
            intent: cancel.intent,
            instrument: 0,
            buy: true,
            order_id: 42,
            kind: "reject".into(),
            status: 6,
            filled: 0,
            price: 100,
            reason: "unknown_or_terminal_order".into(),
            balances: vec![],
            server_ticks: 0,
        };
        t.apply(reject.clone(), &mut f, false).unwrap();
        t.apply(reject.clone(), &mut f, false).unwrap();
        t.settled(&f).unwrap();
        assert_eq!(f.ledger.pending_count(), 0);
        assert!(!f.runtimes[0].orders[0].active);
        let mut corrupt = reject;
        corrupt.price += 1;
        assert!(t.apply(corrupt, &mut f, false).is_err());
    }
    #[test]
    fn buy_risk_includes_fee() {
        let (mut ledger, runtimes) = crate::state::synthetic();
        let i = &runtimes[0].instrument;
        let quote = ledger.asset(&i.quote).unwrap();
        ledger.fee = Decimal::new(1, 3);
        let cost = i.price(100) * i.quantity(10);
        ledger.set_balance(quote, cost, Decimal::ZERO);
        assert!(ledger.reserve(i, true, 100, 10, true).is_none());
        ledger.set_balance(quote, cost * (Decimal::ONE + ledger.fee), Decimal::ZERO);
        assert!(ledger.reserve(i, true, 100, 10, true).is_some());
        assert_eq!(ledger.free(quote), Decimal::ZERO);
    }
}
