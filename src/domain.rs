use rust_decimal::{Decimal, prelude::*};
use serde::Serialize;

#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub book_levels: usize,
    pub ofi_window: usize,
    pub trade_flow_window_ns: i64,
    pub volatility_window_ns: i64,
    pub momentum_window_ns: i64,
    pub liquidity_window_ns: i64,
    pub momentum_scale_ticks: f64,
    pub volatility_scale_ticks: f64,
    pub adverse_scale_ticks: f64,
    pub micro_weight: f64,
    pub mli_weight: f64,
    pub ofi_weight: f64,
    pub trade_flow_weight: f64,
    pub momentum_weight: f64,
    pub max_fair_shift_ticks: f64,
    pub max_inventory_skew_ticks: f64,
    pub base_half_spread_ticks: f64,
    pub volatility_spread_weight_ticks: f64,
    pub liquidity_spread_weight_ticks: f64,
    pub adverse_spread_weight_ticks: f64,
    pub inventory_spread_weight_ticks: f64,
    pub base_quantity: i64,
    pub min_risk_size_factor: f64,
    pub min_edge_ticks: i64,
    pub minimum_replace_move_ticks: i64,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            book_levels: 10,
            ofi_window: 100,
            trade_flow_window_ns: 500_000_000,
            volatility_window_ns: 1_000_000_000,
            momentum_window_ns: 5_000_000_000,
            liquidity_window_ns: 1_000_000_000,
            momentum_scale_ticks: 10.0,
            volatility_scale_ticks: 5.0,
            adverse_scale_ticks: 5.0,
            micro_weight: 1.0,
            mli_weight: 1.0,
            ofi_weight: 1.0,
            trade_flow_weight: 1.0,
            momentum_weight: 1.0,
            max_fair_shift_ticks: 2.0,
            max_inventory_skew_ticks: 2.0,
            base_half_spread_ticks: 2.0,
            volatility_spread_weight_ticks: 1.0,
            liquidity_spread_weight_ticks: 1.0,
            adverse_spread_weight_ticks: 1.0,
            inventory_spread_weight_ticks: 1.0,
            base_quantity: 1_000,
            min_risk_size_factor: 0.25,
            min_edge_ticks: 1,
            minimum_replace_move_ticks: 2,
        }
    }
}

impl Config {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            (1..=5000).contains(&self.book_levels)
                && (1..=8192).contains(&self.ofi_window)
                && self.trade_flow_window_ns > 0
                && self.volatility_window_ns > 0
                && self.momentum_window_ns > 0
                && self.liquidity_window_ns > 0
                && self.momentum_scale_ticks > 0.0
                && self.volatility_scale_ticks > 0.0
                && self.adverse_scale_ticks > 0.0
                && self.base_half_spread_ticks > 0.0
                && self.base_quantity > 0
                && self.min_risk_size_factor > 0.0
                && self.min_risk_size_factor <= 1.0
                && self.min_edge_ticks >= 0
                && self.minimum_replace_move_ticks >= 1
                && self.micro_weight >= 0.0
                && self.mli_weight >= 0.0
                && self.ofi_weight >= 0.0
                && self.trade_flow_weight >= 0.0
                && self.momentum_weight >= 0.0
                && self.micro_weight
                    + self.mli_weight
                    + self.ofi_weight
                    + self.trade_flow_weight
                    + self.momentum_weight
                    > 0.0,
            "invalid hot configuration"
        );
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct Instrument {
    pub id: i64,
    pub symbol: String,
    pub base: String,
    pub quote: String,
    pub tick: Decimal,
    pub step: Decimal,
    pub min_qty: Decimal,
    pub max_qty: Decimal,
    pub min_notional: Decimal,
    pub min_price: Decimal,
    pub max_price: Decimal,
}
impl Instrument {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.tick > Decimal::ZERO
                && self.step > Decimal::ZERO
                && self.min_qty >= Decimal::ZERO
                && self.max_qty >= self.min_qty
                && self.min_notional >= Decimal::ZERO
                && self.min_price >= Decimal::ZERO
                && self.max_price >= self.min_price,
            "invalid instrument filters"
        );
        Ok(())
    }
    // .NET decimal-to-long truncates toward zero and throws on overflow.
    pub fn price_ticks(&self, p: Decimal) -> i64 {
        (p / self.tick)
            .trunc()
            .to_i64()
            .expect("price conversion overflow")
    }
    pub fn quantity_units(&self, q: Decimal) -> i64 {
        (q / self.step)
            .trunc()
            .to_i64()
            .expect("quantity conversion overflow")
    }
    pub fn price(&self, p: i64) -> Decimal {
        Decimal::from(p) * self.tick
    }
    pub fn quantity(&self, q: i64) -> Decimal {
        Decimal::from(q) * self.step
    }
}
pub fn power10(exponent: i8) -> anyhow::Result<Decimal> {
    anyhow::ensure!(
        (-28..=28).contains(&exponent),
        "unsupported SBE exponent {exponent}"
    );
    let mut x = Decimal::ONE;
    for _ in 0..exponent.unsigned_abs() {
        if exponent < 0 {
            x /= Decimal::TEN;
        } else {
            x *= Decimal::TEN;
        }
    }
    Ok(x)
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct Level {
    pub price_ticks: i64,
    pub quantity: i64,
}
#[derive(Debug)]
pub struct Book {
    pub bids: Vec<Level>,
    pub asks: Vec<Level>,
    pub last_id: i64,
    pub synchronized: bool,
    capacity: usize,
}
impl Book {
    pub fn new(capacity: usize) -> Self {
        Self {
            bids: Vec::with_capacity(capacity),
            asks: Vec::with_capacity(capacity),
            last_id: 0,
            synchronized: false,
            capacity,
        }
    }
    pub fn invalidate(&mut self) {
        self.synchronized = false;
    }
    pub fn load(&mut self, bids: &[Level], asks: &[Level], id: i64) -> bool {
        self.bids.clear();
        self.asks.clear();
        self.synchronized = false;
        if id <= 0 || bids.len() > self.capacity || asks.len() > self.capacity {
            return false;
        }
        for l in bids {
            if !self.apply(*l, true) {
                return false;
            }
        }
        for l in asks {
            if !self.apply(*l, false) {
                return false;
            }
        }
        self.last_id = id;
        self.synchronized = !self.bids.is_empty()
            && !self.asks.is_empty()
            && self.bids[0].price_ticks < self.asks[0].price_ticks;
        self.synchronized
    }
    pub fn can_apply(&self, first: i64, last: i64) -> bool {
        self.synchronized
            && first > 0
            && last >= first
            && (last <= self.last_id
                || first <= self.last_id.wrapping_add(1) && last >= self.last_id.wrapping_add(1))
    }
    pub fn complete(&mut self, last: i64, levels: usize) -> bool {
        self.last_id = last;
        self.synchronized = self.bids.len() >= levels
            && self.asks.len() >= levels
            && self.bids[0].price_ticks < self.asks[0].price_ticks;
        self.synchronized
    }
    pub fn apply(&mut self, l: Level, bid: bool) -> bool {
        if l.price_ticks <= 0 || l.quantity < 0 {
            return false;
        }
        let side = if bid { &mut self.bids } else { &mut self.asks };
        let i = side.partition_point(|old| {
            if bid {
                old.price_ticks > l.price_ticks
            } else {
                old.price_ticks < l.price_ticks
            }
        });
        if i < side.len() && side[i].price_ticks == l.price_ticks {
            if l.quantity == 0 {
                if side.len() == self.capacity {
                    return false;
                }
                side.remove(i);
            } else {
                side[i] = l;
            }
        } else if l.quantity != 0 && i < self.capacity {
            if side.len() == self.capacity {
                side.pop();
            }
            side.insert(i, l);
        }
        true
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum Action {
    #[default]
    None,
    Place,
    Keep,
    Replace,
    Cancel,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum Status {
    #[default]
    PendingNew,
    PendingCancel,
    New,
    PartiallyFilled,
    Filled,
    Cancelled,
    Rejected,
    Expired,
}
impl Status {
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        Ok(match s {
            "NEW" => Self::New,
            "PARTIALLY_FILLED" => Self::PartiallyFilled,
            "FILLED" => Self::Filled,
            "CANCELED" => Self::Cancelled,
            "REJECTED" => Self::Rejected,
            "EXPIRED" => Self::Expired,
            "PENDING_CANCEL" => Self::PendingCancel,
            _ => anyhow::bail!("unknown order status {s}"),
        })
    }
    pub fn active(self) -> bool {
        matches!(
            self,
            Self::PendingNew | Self::PendingCancel | Self::New | Self::PartiallyFilled
        )
    }
}
#[derive(Clone, Copy, Debug, Default)]
pub struct Order {
    pub id: i64,
    pub price: i64,
    pub quantity: i64,
    pub filled: i64,
    pub status: Status,
    pub active: bool,
}
impl Order {
    pub fn remaining(self) -> i64 {
        if self.active {
            (self.quantity - self.filled).max(0)
        } else {
            0
        }
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Decision {
    pub bid: i64,
    pub bid_qty: i64,
    pub bid_action: Action,
    pub ask: i64,
    pub ask_qty: i64,
    pub ask_action: Action,
}
impl Decision {
    pub fn values(self, id: i64) -> [i64; 7] {
        [
            id,
            self.bid,
            self.bid_qty,
            self.bid_action as i64,
            self.ask,
            self.ask_qty,
            self.ask_action as i64,
        ]
    }
}
#[derive(Clone, Copy, Debug, Default)]
pub struct Inventory {
    pub current: i64,
    pub target: i64,
    pub reserved_buy: i64,
    pub reserved_sell: i64,
    pub max_long: i64,
    pub max_short: i64,
}
#[derive(Clone, Copy, Debug, Default, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct Signals {
    pub micro_signal: f64,
    pub multi_level_imbalance: f64,
    pub order_flow_imbalance: f64,
    pub trade_flow_imbalance: f64,
    pub momentum_signal: f64,
    pub volatility_ticks: f64,
    pub liquidity_units: i64,
    pub liquidity_ratio: f64,
}
#[derive(Clone, Copy, Debug, Default)]
pub struct Pricing {
    pub direction: f64,
    pub fair: f64,
    pub skew: f64,
    pub reservation: f64,
    pub half_spread: f64,
}
#[derive(Clone, Copy, Default)]
struct Timed {
    time: i64,
    value: f64,
}
struct History {
    data: Vec<Timed>,
    head: usize,
    count: usize,
}
impl History {
    fn new(capacity: usize) -> Self {
        Self {
            data: vec![Timed::default(); capacity],
            head: 0,
            count: 0,
        }
    }
    fn reset(&mut self) {
        self.head = 0;
        self.count = 0;
    }
    fn get(&self, i: usize) -> Timed {
        self.data[(self.head + i) % self.data.len()]
    }
    fn expire(&mut self, cutoff: i64, predecessor: bool) {
        while if predecessor {
            self.count > 1 && self.get(1).time <= cutoff
        } else {
            self.count > 0 && self.get(0).time < cutoff
        } {
            self.head = (self.head + 1) % self.data.len();
            self.count -= 1;
        }
    }
    fn append(&mut self, time: i64, value: f64) -> bool {
        if self.count == self.data.len() {
            return false;
        }
        let index = (self.head + self.count) % self.data.len();
        self.data[index] = Timed { time, value };
        self.count += 1;
        true
    }
}
#[derive(Clone, Copy, Default)]
struct Markout {
    due: i64,
    price: i64,
    side: i32,
}
pub struct Memory {
    mids: History,
    trades: History,
    liquidity: History,
    losses: History,
    markouts: Vec<Markout>,
    ofi: Vec<f64>,
    ofi_head: usize,
    ofi_count: usize,
    ofi_sum: f64,
    ofi_abs: f64,
    first_mid: Option<i64>,
    pub faulted: bool,
    pub warm: bool,
}
fn ratio(n: f64, d: f64) -> f64 {
    if d > 0.0 { n / d } else { 0.0 }
}
impl Memory {
    pub fn new(c: &Config) -> Self {
        Self {
            mids: History::new(4096),
            trades: History::new(8192),
            liquidity: History::new(4096),
            losses: History::new(1024),
            markouts: Vec::with_capacity(1024),
            ofi: vec![0.0; c.ofi_window],
            ofi_head: 0,
            ofi_count: 0,
            ofi_sum: 0.0,
            ofi_abs: 0.0,
            first_mid: None,
            faulted: false,
            warm: false,
        }
    }
    pub fn reset(&mut self) {
        self.mids.reset();
        self.trades.reset();
        self.liquidity.reset();
        self.losses.reset();
        self.markouts.clear();
        self.ofi_head = 0;
        self.ofi_count = 0;
        self.ofi_sum = 0.0;
        self.ofi_abs = 0.0;
        self.first_mid = None;
        self.faulted = false;
        self.warm = false;
    }
    pub fn add_trade(&mut self, t: i64, q: f64, buy: bool, c: &Config) {
        if q <= 0.0 || !q.is_finite() {
            self.faulted = true;
            return;
        }
        self.trades.expire(t - c.trade_flow_window_ns, false);
        if !self.trades.append(t, if buy { q } else { -q }) {
            self.faulted = true;
        }
    }
    pub fn trade_flow(&mut self, t: i64, c: &Config, s: &mut Signals) {
        self.trades.expire(t - c.trade_flow_window_ns, false);
        let (mut buy, mut sell) = (0.0, 0.0);
        for i in 0..self.trades.count {
            let v = self.trades.get(i).value;
            if v > 0.0 {
                buy += v;
            } else {
                sell -= v;
            }
        }
        s.trade_flow_imbalance = ratio(buy - sell, buy + sell);
    }
    pub fn add_ofi(&mut self, v: f64) {
        if self.ofi_count == self.ofi.len() {
            let old = self.ofi[self.ofi_head];
            self.ofi_sum -= old;
            self.ofi_abs -= old.abs();
            self.ofi[self.ofi_head] = v;
            self.ofi_head = (self.ofi_head + 1) % self.ofi.len();
        } else {
            let i = (self.ofi_head + self.ofi_count) % self.ofi.len();
            self.ofi[i] = v;
            self.ofi_count += 1;
        }
        self.ofi_sum += v;
        self.ofi_abs += v.abs();
    }
    pub fn add_fill(&mut self, t: i64, p: i64, buy: bool) {
        if self.markouts.len() == 1024 {
            self.faulted = true;
            return;
        }
        self.markouts.push(Markout {
            due: t.checked_add(250_000_000).expect("markout overflow"),
            price: p,
            side: if buy { 1 } else { -1 },
        });
    }
    pub fn update_book(&mut self, t: i64, b: &Book, c: &Config, s: &mut Signals) -> f64 {
        let mid = (b.bids[0].price_ticks as f64 + b.asks[0].price_ticks as f64) * 0.5;
        self.first_mid.get_or_insert(t);
        self.mids
            .expire(t - c.momentum_window_ns.max(c.volatility_window_ns), true);
        if !self.mids.append(t, mid) {
            self.faulted = true;
        }
        let bb = b.bids[0].quantity as f64;
        let ba = b.asks[0].quantity as f64;
        s.micro_signal = ratio(bb - ba, bb + ba);
        let (mut wb, mut wa) = (0.0, 0.0);
        for i in 0..c.book_levels {
            wb += b.bids[i].quantity as f64 / (i + 1) as f64;
            wa += b.asks[i].quantity as f64 / (i + 1) as f64;
        }
        s.multi_level_imbalance = ratio(wb - wa, wb + wa);
        s.order_flow_imbalance = ratio(self.ofi_sum, self.ofi_abs);
        s.liquidity_units = wb.min(wa) as i64;
        self.liquidity.expire(t - c.liquidity_window_ns, false);
        if !self.liquidity.append(t, wb.min(wa)) {
            self.faulted = true;
        }
        let mut sum = 0.0;
        for i in 0..self.liquidity.count {
            sum += self.liquidity.get(i).value;
        }
        let mean = if self.liquidity.count == 0 {
            0.0
        } else {
            sum / self.liquidity.count as f64
        };
        s.liquidity_ratio = if mean > 0.0 {
            (wb.min(wa) / mean).clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.trade_flow(t, c, s);
        let mut older = mid;
        for i in 0..self.mids.count {
            let sample = self.mids.get(i);
            if sample.time > t - c.momentum_window_ns {
                break;
            }
            older = sample.value;
        }
        s.momentum_signal = ((mid - older) / c.momentum_scale_ticks).clamp(-1.0, 1.0);
        let (mut sum, mut sq, mut n) = (0.0, 0.0, 0);
        for i in 0..self.mids.count {
            let sample = self.mids.get(i);
            if sample.time < t - c.volatility_window_ns {
                continue;
            }
            let d = sample.value - mid;
            sum += d;
            sq += d * d;
            n += 1;
        }
        s.volatility_ticks = if n > 0 {
            (sq / n as f64 - (sum / n as f64) * (sum / n as f64))
                .max(0.0)
                .sqrt()
        } else {
            0.0
        };
        self.losses.expire(t - c.trade_flow_window_ns, false);
        let mut i = 0;
        while i < self.markouts.len() {
            let m = self.markouts[i];
            if m.due > t {
                i += 1;
                continue;
            }
            let found = (0..self.mids.count)
                .map(|j| self.mids.get(j))
                .find(|s| s.time >= m.due);
            let Some(sample) = found else {
                self.faulted = true;
                break;
            };
            let mark = m.side as f64 * (sample.value - m.price as f64);
            if !self.losses.append(t, (-mark).max(0.0)) {
                self.faulted = true;
            }
            self.markouts.swap_remove(i);
        }
        let mut loss = 0.0;
        for i in 0..self.losses.count {
            loss += self.losses.get(i).value;
        }
        self.warm = t - self.first_mid.unwrap() >= c.momentum_window_ns
            && self.ofi_count == self.ofi.len()
            && self.liquidity.count > 0;
        if self.losses.count == 0 {
            0.0
        } else {
            (loss / self.losses.count as f64 / c.adverse_scale_ticks).clamp(0.0, 1.0)
        }
    }
}

pub fn invalid(orders: &[Order; 2]) -> Decision {
    let cancel = |o: Order| {
        if o.active && o.status != Status::PendingCancel {
            Action::Cancel
        } else {
            Action::None
        }
    };
    Decision {
        bid_action: cancel(orders[0]),
        ask_action: cancel(orders[1]),
        ..Decision::default()
    }
}
#[allow(clippy::too_many_arguments)]
pub fn evaluate(
    i: &Instrument,
    c: &Config,
    b: &Book,
    s: &Signals,
    v: &Inventory,
    o: &[Order; 2],
    p: &mut Pricing,
    adverse: f64,
    fee: Decimal,
    free_base: Decimal,
    free_quote: Decimal,
) -> Decision {
    if !b.synchronized
        || b.bids.len() < c.book_levels
        || b.asks.len() < c.book_levels
        || v.max_long < v.current
        || fee < Decimal::ZERO
    {
        return invalid(o);
    }
    let weights =
        c.micro_weight + c.mli_weight + c.ofi_weight + c.trade_flow_weight + c.momentum_weight;
    let direction = ((c.micro_weight * s.micro_signal
        + c.mli_weight * s.multi_level_imbalance
        + c.ofi_weight * s.order_flow_imbalance
        + c.trade_flow_weight * s.trade_flow_imbalance
        + c.momentum_weight * s.momentum_signal)
        / weights)
        .clamp(-1.0, 1.0);
    let mid = (b.bids[0].price_ticks as f64 + b.asks[0].price_ticks as f64) * 0.5;
    let deviation = v.current - v.target;
    let lower = -v.max_short;
    let range = if deviation >= 0 {
        v.max_long as f64 - v.target as f64
    } else {
        v.target as f64 - lower as f64
    };
    let z = if range > 0.0 {
        (deviation as f64 / range).clamp(-1.0, 1.0)
    } else {
        0.0
    };
    let volatility = (s.volatility_ticks / c.volatility_scale_ticks).clamp(0.0, 1.0);
    let thin = (1.0 - s.liquidity_ratio).clamp(0.0, 1.0);
    let risk = c.volatility_spread_weight_ticks * volatility
        + c.liquidity_spread_weight_ticks * thin
        + c.adverse_spread_weight_ticks * adverse.clamp(0.0, 1.0);
    let half = (c.base_half_spread_ticks + risk + c.inventory_spread_weight_ticks * z.abs())
        .max(fee.to_f64().unwrap() * mid + c.min_edge_ticks as f64);
    let fair = mid + c.max_fair_shift_ticks * direction;
    let skew = c.max_inventory_skew_ticks * z;
    let reservation = fair - skew;
    let bid = ((reservation - half).floor() as i64).min(b.asks[0].price_ticks - 1);
    let ask = ((reservation + half).ceil() as i64).max(b.bids[0].price_ticks + 1);
    if bid <= 0 || ask <= bid || !half.is_finite() {
        return invalid(o);
    }
    let factor = c
        .min_risk_size_factor
        .max(1.0 / (1.0 + risk / c.base_half_spread_ticks));
    let mut buy = (c.base_quantity as f64 * factor * (1.0 - z)).floor() as i64;
    let mut sell = (c.base_quantity as f64 * factor * (1.0 + z)).floor() as i64;
    let own_buy = o[0].remaining();
    let own_sell = o[1].remaining();
    let bc = (v.max_long - v.current - (v.reserved_buy - own_buy).max(0)).max(0);
    let sc = (v.current - lower - (v.reserved_sell - own_sell).max(0)).max(0);
    let bf = free_quote + i.price(o[0].price) * i.quantity(own_buy);
    let sf = free_base + i.quantity(own_sell);
    let max = i.quantity_units(i.max_qty);
    buy = buy
        .min(bc)
        .min(i.quantity_units(i.max_qty.min(bf / i.price(bid))))
        .min(max)
        .max(0);
    sell = sell
        .min(sc)
        .min(i.quantity_units(i.max_qty.min(sf)))
        .min(max)
        .max(0);
    let valid = |price: i64, qty: i64| {
        qty > 0
            && i.price(price) >= i.min_price
            && i.price(price) <= i.max_price
            && i.quantity(qty) >= i.min_qty
            && i.quantity(qty) <= i.max_qty
            && i.price(price) * i.quantity(qty) >= i.min_notional
    };
    if !valid(bid, buy) {
        buy = 0;
    }
    if !valid(ask, sell) {
        sell = 0;
    }
    *p = Pricing {
        direction,
        fair,
        skew,
        reservation,
        half_spread: half,
    };
    let action = |order: Order, price: i64, qty: i64| {
        if order.active && matches!(order.status, Status::PendingNew | Status::PendingCancel) {
            Action::None
        } else if qty == 0 {
            if order.active {
                Action::Cancel
            } else {
                Action::None
            }
        } else if !order.active {
            Action::Place
        } else if (price - order.price).abs() >= c.minimum_replace_move_ticks
            || qty != order.remaining()
        {
            Action::Replace
        } else {
            Action::Keep
        }
    };
    let bp = if buy > 0 { bid } else { 0 };
    let ap = if sell > 0 { ask } else { 0 };
    Decision {
        bid: bp,
        bid_qty: buy,
        bid_action: action(o[0], bp, buy),
        ask: ap,
        ask_qty: sell,
        ask_action: action(o[1], ap, sell),
    }
}
