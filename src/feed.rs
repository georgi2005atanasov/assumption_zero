use crate::{
    domain::*,
    metrics::{Benchmark, ticks},
    runtime::*,
};
use anyhow::Result;
use rust_decimal::{Decimal, prelude::ToPrimitive};
use std::io::Write;
fn u16_at(b: &[u8], o: usize) -> usize {
    u16::from_le_bytes(b[o..o + 2].try_into().unwrap()) as usize
}
fn i64_at(b: &[u8], o: usize) -> i64 {
    i64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}
#[derive(Clone, Copy)]
pub struct Layout {
    pub template: u16,
    pub block: usize,
    pub symbol: usize,
    pub group1: usize,
    pub count1: usize,
    pub stride1: usize,
    pub group2: usize,
    pub count2: usize,
    pub stride2: usize,
}
pub fn validate(b: &[u8]) -> Option<Layout> {
    if b.len() < 8 {
        return None;
    }
    let block = u16_at(b, 0);
    let template = u16_at(b, 2) as u16;
    if u16_at(b, 4) != 1 || u16_at(b, 6) != 0 || b.len() < 8 + block + 1 {
        return None;
    }
    let mut l = Layout {
        template,
        block,
        symbol: 8 + block,
        group1: 0,
        count1: 0,
        stride1: 0,
        group2: 0,
        count2: 0,
        stride2: 0,
    };
    match template {
        10001 => {
            if block < 50 {
                return None;
            }
        }
        10003 => {
            if block < 26 {
                return None;
            }
            for group in 0..2 {
                let o = l.symbol;
                if b.len() - o < 4 {
                    return None;
                }
                let stride = u16_at(b, o);
                let count = u16_at(b, o + 2);
                if stride < 16 || count > 5000 || stride * count > b.len() - o - 4 {
                    return None;
                }
                if group == 0 {
                    l.group1 = o + 4;
                    l.count1 = count;
                    l.stride1 = stride;
                } else {
                    l.group2 = o + 4;
                    l.count2 = count;
                    l.stride2 = stride;
                }
                l.symbol = o + 4 + stride * count;
            }
        }
        10000 => {
            if block < 18 || b.len() - l.symbol < 6 {
                return None;
            }
            let o = l.symbol;
            let stride = u16_at(b, o);
            let count = u32::from_le_bytes(b[o + 2..o + 6].try_into().unwrap()) as usize;
            if stride < 25 || count > 8192 || stride * count > b.len() - o - 6 {
                return None;
            }
            l.group1 = o + 6;
            l.count1 = count;
            l.stride1 = stride;
            l.symbol = o + 6 + stride * count;
        }
        _ => return None,
    }
    if l.symbol >= b.len() || l.symbol + 1 + b[l.symbol] as usize != b.len() {
        None
    } else {
        Some(l)
    }
}
fn decode(m: i64, e: i8) -> Result<Decimal> {
    let mut value = Decimal::from(m);
    for _ in 0..e.unsigned_abs() {
        value = if e > 0 {
            value.checked_mul(Decimal::TEN)
        } else {
            value.checked_div(Decimal::TEN)
        }
        .ok_or_else(|| anyhow::anyhow!("SBE decimal overflow"))?;
    }
    Ok(value)
}
#[derive(Clone, Copy, Default)]
struct Change {
    price: Decimal,
    quantity: Decimal,
}
pub struct Feed {
    pub ledger: Ledger,
    pub runtimes: Vec<Runtime>,
    pub replay: bool,
    estimated: bool,
    pub log: bool,
    pub benchmark: Option<Benchmark>,
    pub trace: Option<std::io::BufWriter<std::fs::File>>,
    pub simulation_decisions: Option<Vec<usize>>,
    pub decisions: usize,
    pub actionable: usize,
    pub measure_allocations: bool,
    pub ready_decisions: usize,
    pub allocation_bytes: Option<u64>,
    allocation_start: u64,
    bids: Vec<Change>,
    asks: Vec<Change>,
    trades: Vec<(Decimal, bool)>,
}
impl Feed {
    pub fn new(ledger: Ledger, runtimes: Vec<Runtime>, replay: bool, log: bool) -> Self {
        let estimated = runtimes.first().is_some_and(|r| r.estimated);
        Self {
            ledger,
            runtimes,
            estimated,
            replay,
            log,
            benchmark: None,
            trace: None,
            simulation_decisions: None,
            decisions: 0,
            actionable: 0,
            measure_allocations: false,
            ready_decisions: 0,
            allocation_bytes: None,
            allocation_start: 0,
            bids: vec![Change::default(); 5000],
            asks: vec![Change::default(); 5000],
            trades: vec![(Decimal::ZERO, false); 8192],
        }
    }
    fn emit(&mut self, r: usize, d: Decision) -> Result<()> {
        if let Some(v) = &mut self.simulation_decisions {
            v.push(r);
        }
        if let Some(b) = &mut self.benchmark {
            b.decision(self.runtimes[r].instrument.id, d);
        }
        self.decisions += 1;
        if matches!(
            d.bid_action,
            Action::Place | Action::Replace | Action::Cancel
        ) || matches!(
            d.ask_action,
            Action::Place | Action::Replace | Action::Cancel
        ) {
            self.actionable += 1;
        }
        if let Some(w) = &mut self.trace {
            serde_json::to_writer(&mut *w, &d.values(self.runtimes[r].instrument.id))?;
            w.write_all(b"\n")?;
        }
        if self.measure_allocations && self.runtimes[r].ready(&self.ledger) {
            self.ready_decisions += 1;
            if self.ready_decisions == 100 {
                self.allocation_start = crate::metrics::allocated();
            }
            if self.ready_decisions == 200 {
                self.allocation_bytes = Some(crate::metrics::allocated() - self.allocation_start);
            }
        }
        Ok(())
    }
    pub fn binary(&mut self, b: &[u8]) -> Result<()> {
        let Some(l) = validate(b) else {
            anyhow::ensure!(self.replay, "invalid SBE frame; reload snapshots");
            return Ok(());
        };
        if let Some(m) = &mut self.benchmark {
            m.frame().template_id = l.template;
        }
        let start = if self.benchmark.is_some() { ticks() } else { 0 };
        let symbol = &b[l.symbol + 1..];
        if symbol.len() > 64 {
            return Ok(());
        }
        let index = self
            .runtimes
            .iter()
            .position(|r| r.instrument.symbol.as_bytes() == symbol);
        match l.template {
            10001 => {
                if !self.estimated && !self.log {
                    return Ok(());
                }
                let pe = b[24] as i8;
                let qe = b[25] as i8;
                let bp = decode(i64_at(b, 26), pe)?;
                let bq = decode(i64_at(b, 34), qe)?;
                let ap = decode(i64_at(b, 42), pe)?;
                let aq = decode(i64_at(b, 50), qe)?;
                if let Some(index) = index {
                    let r = &mut self.runtimes[index];
                    if r.estimated && !r.book.synchronized {
                        r.set_precision(pe, qe, &mut self.ledger)?;
                        let bid = Level {
                            price_ticks: r.instrument.price_ticks(bp),
                            quantity: r.instrument.quantity_units(bq),
                        };
                        let ask = Level {
                            price_ticks: r.instrument.price_ticks(ap),
                            quantity: r.instrument.quantity_units(aq),
                        };
                        if r.instrument.price(bid.price_ticks) == bp
                            && r.instrument.price(ask.price_ticks) == ap
                            && r.instrument.quantity(bid.quantity) == bq
                            && r.instrument.quantity(ask.quantity) == aq
                        {
                            r.seed(bid, ask, i64_at(b, 16));
                        }
                    }
                }
                if let Some(m) = &mut self.benchmark {
                    m.frame().decode_ticks += ticks() - start;
                }
                if self.log {
                    println!(
                        "QUOTE {} bid={bp} ({bq}) ask={ap} ({aq})",
                        String::from_utf8_lossy(symbol)
                    );
                }
            }
            10003 => {
                let pe = b[32] as i8;
                let qe = b[33] as i8;
                for j in 0..l.count1 {
                    let o = l.group1 + j * l.stride1;
                    self.bids[j] = Change {
                        price: decode(i64_at(b, o), pe)?,
                        quantity: decode(i64_at(b, o + 8), qe)?,
                    };
                }
                for j in 0..l.count2 {
                    let o = l.group2 + j * l.stride2;
                    self.asks[j] = Change {
                        price: decode(i64_at(b, o), pe)?,
                        quantity: decode(i64_at(b, o + 8), qe)?,
                    };
                }
                if let Some(m) = &mut self.benchmark {
                    m.frame().decode_ticks += ticks() - start;
                }
                let first = i64_at(b, 16);
                let last = i64_at(b, 24);
                if let Some(index) = index {
                    let r = &mut self.runtimes[index];
                    if r.estimated && !r.book.synchronized {
                        r.set_precision(pe, qe, &mut self.ledger)?;
                        for (bid, changes) in [
                            (true, &self.bids[..l.count1]),
                            (false, &self.asks[..l.count2]),
                        ] {
                            for ch in changes {
                                if r.book.synchronized {
                                    break;
                                }
                                if ch.price > Decimal::ZERO && ch.quantity > Decimal::ZERO {
                                    let level = Level {
                                        price_ticks: r.instrument.price_ticks(ch.price),
                                        quantity: r.instrument.quantity_units(ch.quantity),
                                    };
                                    r.observe(bid, level, last);
                                }
                            }
                        }
                    }
                    let was = r.book.synchronized;
                    let bs = if self.benchmark.is_some() { ticks() } else { 0 };
                    let mut decision = None;
                    if r.begin_depth(first, last) {
                        let mut valid = true;
                        for (bid, changes) in [
                            (true, &self.bids[..l.count1]),
                            (false, &self.asks[..l.count2]),
                        ] {
                            for ch in changes {
                                if !valid {
                                    break;
                                }
                                let level = Level {
                                    price_ticks: r.instrument.price_ticks(ch.price),
                                    quantity: r.instrument.quantity_units(ch.quantity),
                                };
                                valid = r.instrument.price(level.price_ticks) == ch.price
                                    && r.instrument.quantity(level.quantity) == ch.quantity
                                    && r.apply(level, bid);
                            }
                        }
                        if !valid {
                            r.invalidate();
                        }
                        if let Some(m) = &mut self.benchmark {
                            m.frame().book_ticks += ticks() - bs;
                        }
                        decision = Some(if valid {
                            r.end_depth(
                                i64_at(b, 8).checked_mul(1000).expect("event time overflow"),
                                &self.ledger,
                            )
                        } else {
                            r.evaluate(&self.ledger)
                        });
                        if valid && let Some(m) = &mut self.benchmark {
                            m.frame().signal_ticks += r.signal_ticks;
                            m.frame().quote_ticks += r.quote_ticks;
                        }
                    } else if was && !r.book.synchronized {
                        if let Some(m) = &mut self.benchmark {
                            m.frame().book_ticks += ticks() - bs;
                        }
                        decision = Some(r.evaluate(&self.ledger));
                    }
                    if let Some(d) = decision {
                        self.emit(index, d)?;
                        let r = &self.runtimes[index];
                        anyhow::ensure!(
                            self.replay || (!r.memory.faulted && r.book.synchronized),
                            "depth coverage/history lost; reload snapshots"
                        );
                    }
                }
                if self.log {
                    println!(
                        "DEPTH {} {first}->{last} bids={} asks={}",
                        String::from_utf8_lossy(symbol),
                        l.count1,
                        l.count2
                    );
                }
            }
            10000 => {
                let qe = b[25] as i8;
                for j in 0..l.count1 {
                    let o = l.group1 + j * l.stride1;
                    self.trades[j] = (decode(i64_at(b, o + 16), qe)?, b[o + 24] == 0);
                }
                if let Some(m) = &mut self.benchmark {
                    m.frame().decode_ticks += ticks() - start;
                }
                if let Some(index) = index {
                    let time = i64_at(b, 8).checked_mul(1000).expect("event time overflow");
                    for j in 0..l.count1 {
                        let (q, buy) = self.trades[j];
                        let q = q.to_f64().expect("decimal to double");
                        let r = &mut self.runtimes[index];
                        if q <= 0.0 || !q.is_finite() {
                            r.invalidate();
                            return Ok(());
                        }
                        let d = r.trade(time, q, buy, &self.ledger);
                        if let Some(m) = &mut self.benchmark {
                            m.frame().signal_ticks += r.signal_ticks;
                            m.frame().quote_ticks += r.quote_ticks;
                        }
                        self.emit(index, d)?;
                        anyhow::ensure!(
                            self.replay || !self.runtimes[index].memory.faulted,
                            "trade history exhausted"
                        );
                    }
                }
            }
            _ => unreachable!(),
        }
        Ok(())
    }
}
pub fn text_message(bytes: &[u8], log: bool) -> Result<()> {
    let v: serde_json::Value = serde_json::from_slice(bytes)?;
    if log {
        println!("TEXT: {}", String::from_utf8_lossy(bytes));
    }
    anyhow::ensure!(
        v["e"] != "serverShutdown" && v.get("code").is_none(),
        "feed shutdown/error control message"
    );
    Ok(())
}
