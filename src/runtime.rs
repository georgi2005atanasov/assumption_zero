use crate::{domain::*, metrics::ticks};
use rust_decimal::Decimal;

#[derive(Clone, Copy, Default)]
struct Intent {
    id: i64,
    asset: usize,
    amount: Decimal,
}
pub struct Ledger {
    pub assets: Vec<String>,
    pub ready: bool,
    pub fee: Decimal,
    free: Vec<Decimal>,
    locked: Vec<Decimal>,
    reserved: Vec<Decimal>,
    intents: Vec<Intent>,
    next: i64,
}
impl Ledger {
    pub fn new(assets: Vec<String>) -> Self {
        let n = assets.len();
        Self {
            assets,
            ready: false,
            fee: Decimal::ZERO,
            free: vec![Decimal::ZERO; n],
            locked: vec![Decimal::ZERO; n],
            reserved: vec![Decimal::ZERO; n],
            intents: vec![Intent::default(); 4096],
            next: 0,
        }
    }
    pub fn asset(&self, a: &str) -> Option<usize> {
        self.assets.iter().position(|s| s == a)
    }
    pub fn begin(&mut self, fee: Decimal) {
        assert!(fee >= Decimal::ZERO);
        self.ready = false;
        self.fee = fee;
        self.free.fill(Decimal::ZERO);
        self.locked.fill(Decimal::ZERO);
        self.reserved.fill(Decimal::ZERO);
        self.intents.fill(Intent::default());
    }
    pub fn set_balance(&mut self, a: usize, free: Decimal, locked: Decimal) {
        if a >= self.assets.len() || free < Decimal::ZERO || locked < Decimal::ZERO {
            self.ready = false;
            return;
        }
        self.free[a] = free;
        self.locked[a] = locked;
    }
    pub fn free(&self, a: usize) -> Decimal {
        (self.free[a] - self.reserved[a]).max(Decimal::ZERO)
    }
    pub fn total(&self, a: usize) -> Decimal {
        self.free[a] + self.locked[a]
    }
    pub fn reserve(
        &mut self,
        i: &Instrument,
        buy: bool,
        p: i64,
        q: i64,
        require_available: bool,
    ) -> Option<i64> {
        if !self.ready || q <= 0 || p <= 0 {
            return None;
        }
        let a = self.asset(if buy { &i.quote } else { &i.base })?;
        let amount = i.quantity(q) * if buy { i.price(p) } else { Decimal::ONE };
        if require_available && amount > self.free(a) {
            return None;
        }
        if let Some(slot) = self.intents.iter_mut().find(|x| x.id == 0) {
            self.next += 1;
            *slot = Intent {
                id: self.next,
                asset: a,
                amount,
            };
            self.reserved[a] += amount;
            Some(self.next)
        } else {
            self.ready = false;
            None
        }
    }
    pub fn release(&mut self, id: i64) -> bool {
        if let Some(slot) = self.intents.iter_mut().find(|x| x.id == id) {
            self.reserved[slot.asset] -= slot.amount;
            *slot = Intent::default();
            true
        } else {
            false
        }
    }
}
#[derive(Clone, Copy, Default)]
struct Pending {
    intent: i64,
    hold: i64,
    action: Action,
    prior: Status,
    replacement_price: i64,
    replacement_quantity: i64,
}
pub struct Runtime {
    pub instrument: Instrument,
    pub config: Config,
    pub book: Book,
    pub signals: Signals,
    pub memory: Memory,
    pub pricing: Pricing,
    pub inventory: Inventory,
    pub orders: [Order; 2],
    pub estimated: bool,
    pub stages: bool,
    pub signal_ticks: i64,
    pub quote_ticks: i64,
    base: usize,
    quote: usize,
    precision_initialized: bool,
    estimated_bids: Vec<Level>,
    estimated_asks: Vec<Level>,
    observed: [Level; 2],
    pending_last: i64,
    prior: [Level; 2],
    last_event: i64,
    diff_open: bool,
    adverse: f64,
    pending: [Pending; 2],
}
impl Runtime {
    pub fn new(
        i: Instrument,
        c: Config,
        a: &Ledger,
        target: i64,
        max_long: i64,
        max_short: i64,
        estimated: bool,
    ) -> Self {
        c.validate().expect("invalid config");
        i.validate().expect("invalid instrument");
        let base = a.asset(&i.base).expect("missing base asset");
        let quote = a.asset(&i.quote).expect("missing quote asset");
        let levels = if estimated { c.book_levels.max(32) } else { 0 };
        Self {
            instrument: i,
            config: c,
            book: Book::new(5000),
            signals: Signals::default(),
            memory: Memory::new(&c),
            pricing: Pricing::default(),
            inventory: Inventory {
                target,
                max_long,
                max_short,
                ..Inventory::default()
            },
            orders: [Order::default(); 2],
            estimated,
            stages: false,
            signal_ticks: 0,
            quote_ticks: 0,
            base,
            quote,
            precision_initialized: false,
            estimated_bids: vec![Level::default(); levels],
            estimated_asks: vec![Level::default(); levels],
            observed: [Level::default(); 2],
            pending_last: 0,
            prior: [Level::default(); 2],
            last_event: 0,
            diff_open: false,
            adverse: 0.0,
            pending: [Pending::default(); 2],
        }
    }
    pub fn ready(&self, a: &Ledger) -> bool {
        a.ready && self.book.synchronized && self.memory.warm && !self.memory.faulted
    }
    pub fn load(&mut self, bids: &[Level], asks: &[Level], id: i64) -> bool {
        self.memory.reset();
        self.last_event = 0;
        self.book.load(bids, asks, id)
            && self.book.bids.len() >= self.config.book_levels
            && self.book.asks.len() >= self.config.book_levels
    }
    pub fn set_precision(&mut self, p: i8, q: i8, a: &mut Ledger) -> anyhow::Result<()> {
        if !self.estimated || self.precision_initialized {
            return Ok(());
        }
        self.instrument.tick = power10(p)?;
        self.instrument.step = power10(q)?;
        self.instrument.min_qty = self.instrument.step;
        self.instrument.validate()?;
        self.inventory.target = self.instrument.quantity_units(Decimal::from(100));
        self.inventory.max_long = self
            .inventory
            .target
            .checked_add(self.config.base_quantity * 10)
            .expect("inventory overflow");
        self.inventory.max_short = 0;
        self.balance_changed(a, None);
        self.precision_initialized = true;
        Ok(())
    }
    pub fn seed(&mut self, bid: Level, ask: Level, id: i64) -> bool {
        if !self.estimated
            || self.book.synchronized
            || id <= 0
            || bid.price_ticks <= self.estimated_bids.len() as i64
            || ask.price_ticks <= bid.price_ticks
            || bid.quantity <= 0
            || ask.quantity <= 0
        {
            return false;
        }
        for j in 0..self.estimated_bids.len() {
            self.estimated_bids[j] = Level {
                price_ticks: bid.price_ticks - j as i64,
                quantity: bid.quantity,
            };
            self.estimated_asks[j] = Level {
                price_ticks: ask.price_ticks + j as i64,
                quantity: ask.quantity,
            };
        }
        self.observed = [bid, ask];
        self.memory.reset();
        self.last_event = 0;
        self.book
            .load(&self.estimated_bids, &self.estimated_asks, id)
            && self.book.bids.len() >= self.config.book_levels
            && self.book.asks.len() >= self.config.book_levels
    }
    pub fn observe(&mut self, bid: bool, l: Level, id: i64) -> bool {
        if !self.estimated || self.book.synchronized || l.price_ticks <= 0 || l.quantity <= 0 {
            return false;
        }
        if bid {
            if l.price_ticks >= self.observed[0].price_ticks {
                self.observed[0] = l;
            }
        } else if self.observed[1].price_ticks == 0 || l.price_ticks <= self.observed[1].price_ticks
        {
            self.observed[1] = l;
        }
        self.seed(self.observed[0], self.observed[1], id)
    }
    pub fn begin_depth(&mut self, first: i64, last: i64) -> bool {
        self.diff_open = false;
        let valid = if self.estimated {
            self.book.synchronized && first > 0 && last >= first
        } else {
            self.book.can_apply(first, last)
        };
        if !valid {
            self.book.invalidate();
            return false;
        }
        if last <= self.book.last_id {
            return false;
        }
        self.prior = [self.book.bids[0], self.book.asks[0]];
        self.pending_last = last;
        self.diff_open = true;
        true
    }
    pub fn apply(&mut self, l: Level, bid: bool) -> bool {
        if !self.diff_open || !self.book.apply(l, bid) {
            self.invalidate();
            false
        } else {
            true
        }
    }
    pub fn end_depth(&mut self, event: i64, a: &Ledger) -> Decision {
        let start = if self.stages { ticks() } else { 0 };
        if !self.diff_open
            || !self
                .book
                .complete(self.pending_last, self.config.book_levels)
        {
            self.signal_ticks = 0;
            self.quote_ticks = 0;
            self.invalidate();
            return invalid(&self.orders);
        }
        self.diff_open = false;
        let time = self.last_event.max(event);
        self.last_event = time;
        let b = self.book.bids[0];
        let s = self.book.asks[0];
        let pb = self.prior[0];
        let ps = self.prior[1];
        let bo = if b.price_ticks > pb.price_ticks {
            b.quantity
        } else if b.price_ticks < pb.price_ticks {
            -pb.quantity
        } else {
            b.quantity - pb.quantity
        };
        let ao = if s.price_ticks < ps.price_ticks {
            -s.quantity
        } else if s.price_ticks > ps.price_ticks {
            ps.quantity
        } else {
            ps.quantity - s.quantity
        };
        self.memory.add_ofi(bo as f64 + ao as f64);
        self.adverse = self
            .memory
            .update_book(time, &self.book, &self.config, &mut self.signals);
        let qstart = if self.stages { ticks() } else { 0 };
        let d = self.evaluate(a);
        if self.stages {
            self.signal_ticks = qstart - start;
            self.quote_ticks = ticks() - qstart;
        }
        d
    }
    pub fn trade(&mut self, event: i64, q: f64, buy: bool, a: &Ledger) -> Decision {
        let start = if self.stages { ticks() } else { 0 };
        let t = self.last_event.max(event);
        self.last_event = t;
        self.memory.add_trade(t, q, buy, &self.config);
        self.memory.trade_flow(t, &self.config, &mut self.signals);
        let qs = if self.stages { ticks() } else { 0 };
        let d = self.evaluate(a);
        if self.stages {
            self.signal_ticks = qs - start;
            self.quote_ticks = ticks() - qs;
        }
        d
    }
    pub fn balance_changed(&mut self, a: &mut Ledger, changed: Option<usize>) {
        self.inventory.current = self.instrument.quantity_units(a.total(self.base));
        for side in 0..2 {
            let asset = if side == 0 { self.quote } else { self.base };
            if changed == Some(asset)
                && self.pending[side].intent > 0
                && self.orders[side].id > 0
                && self.pending[side].action == Action::Place
            {
                self.clear_intent(side, a);
            }
        }
    }
    pub fn configure_inventory(&mut self, target: i64, long: i64, short: i64) {
        assert!(long >= target && short >= 0 && target >= -short);
        self.inventory.target = target;
        self.inventory.max_long = long;
        self.inventory.max_short = short;
    }
    pub fn set_orders(&mut self, o: [Order; 2]) {
        self.orders = o;
        self.recalculate();
    }
    pub fn begin_reconciliation(&mut self) {
        self.pending = [Pending::default(); 2];
        self.orders = [Order::default(); 2];
        self.recalculate();
    }
    #[allow(clippy::too_many_arguments)] // Keep the execution-event contract aligned with C#.
    pub fn execution(
        &mut self,
        buy: bool,
        id: i64,
        filled: i64,
        price: i64,
        time: i64,
        status: Status,
        a: &mut Ledger,
    ) {
        let s = usize::from(!buy);
        let order = self.orders[s];
        if order.id != id || filled < order.filled || filled > order.quantity {
            self.invalidate();
            return;
        }
        let delta = filled - order.filled;
        self.orders[s].filled = filled;
        self.orders[s].status = status;
        self.orders[s].active = status.active();
        if !self.orders[s].active {
            if self.pending[s].hold > 0 {
                a.release(self.pending[s].hold);
            }
            self.pending[s].hold = 0;
            let p = self.pending[s].action;
            if p == Action::Cancel || p == Action::Replace && status != Status::Cancelled {
                self.clear_intent(s, a);
            }
        }
        if !self.orders[s].active
            && status == Status::Cancelled
            && self.pending[s].action == Action::Replace
        {
            self.orders[s] = Order {
                price: self.pending[s].replacement_price,
                quantity: self.pending[s].replacement_quantity,
                status: Status::PendingNew,
                active: true,
                ..Order::default()
            };
            self.pending[s].action = Action::Place;
        }
        if delta > 0 {
            self.inventory.current += if buy { delta } else { -delta };
            self.memory.add_fill(time, price, buy);
        }
        self.recalculate();
    }
    pub fn evaluate(&mut self, a: &Ledger) -> Decision {
        if !self.ready(a) {
            return invalid(&self.orders);
        }
        evaluate(
            &self.instrument,
            &self.config,
            &self.book,
            &self.signals,
            &self.inventory,
            &self.orders,
            &mut self.pricing,
            self.adverse,
            a.fee,
            a.free(self.base),
            a.free(self.quote),
        )
    }
    pub fn accept_intent(&mut self, buy: bool, d: Decision, a: &mut Ledger) -> Option<i64> {
        let s = usize::from(!buy);
        if self.pending[s].action != Action::None || self.evaluate(a) != d {
            return None;
        }
        let action = if buy { d.bid_action } else { d.ask_action };
        if !matches!(action, Action::Place | Action::Replace | Action::Cancel)
            || action != Action::Cancel && !self.ready(a)
        {
            return None;
        }
        self.pending[s].prior = self.orders[s].status;
        self.pending[s].action = action;
        if action == Action::Cancel {
            let Some(hold) = a.reserve(
                &self.instrument,
                buy,
                self.orders[s].price,
                self.orders[s].remaining(),
                false,
            ) else {
                self.pending[s].action = Action::None;
                return None;
            };
            self.pending[s].hold = hold;
            self.orders[s].status = Status::PendingCancel;
            return Some(hold);
        }
        let p = if buy { d.bid } else { d.ask };
        let q = if buy { d.bid_qty } else { d.ask_qty };
        let Some(id) = a.reserve(&self.instrument, buy, p, q, true) else {
            self.pending[s].action = Action::None;
            return None;
        };
        self.pending[s].intent = id;
        if action == Action::Replace {
            let Some(hold) = a.reserve(
                &self.instrument,
                buy,
                self.orders[s].price,
                self.orders[s].remaining(),
                false,
            ) else {
                self.clear_intent(s, a);
                return None;
            };
            self.pending[s].hold = hold;
        }
        if action == Action::Place {
            self.orders[s] = Order {
                price: p,
                quantity: q,
                status: Status::PendingNew,
                active: true,
                ..Order::default()
            };
        } else {
            self.orders[s].status = Status::PendingCancel;
            self.pending[s].replacement_price = p;
            self.pending[s].replacement_quantity = q;
        }
        self.recalculate();
        Some(id)
    }
    pub fn reject_intent(&mut self, buy: bool, id: i64, a: &mut Ledger) {
        let s = usize::from(!buy);
        let p = self.pending[s];
        let expected = if p.action == Action::Cancel {
            p.hold
        } else {
            p.intent
        };
        if p.action == Action::None || expected != id {
            return;
        }
        if p.action == Action::Place {
            self.orders[s] = Order::default();
        } else {
            self.orders[s].status = p.prior;
        }
        self.clear_intent(s, a);
        if p.hold > 0 {
            a.release(p.hold);
        }
        self.pending[s].hold = 0;
        self.recalculate();
    }
    pub fn acknowledge(&mut self, buy: bool, intent: i64, id: i64) -> bool {
        let s = usize::from(!buy);
        let p = self.pending[s];
        let o = &mut self.orders[s];
        if id <= 0
            || p.intent != intent
            || p.action != Action::Place
            || !o.active
            || o.status != Status::PendingNew
            || o.id != 0
        {
            return false;
        }
        o.id = id;
        true
    }
    fn clear_intent(&mut self, s: usize, a: &mut Ledger) {
        if self.pending[s].intent > 0 {
            a.release(self.pending[s].intent);
        }
        self.pending[s].intent = 0;
        self.pending[s].action = Action::None;
    }
    pub fn invalidate(&mut self) {
        self.diff_open = false;
        self.book.invalidate();
    }
    fn recalculate(&mut self) {
        self.inventory.reserved_buy = self.orders[0].remaining();
        self.inventory.reserved_sell = self.orders[1].remaining();
    }
}
