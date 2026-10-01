#![allow(dead_code)]
use assumption_zero::{domain::*, runtime::*};
use rust_decimal::Decimal;
pub fn instrument() -> Instrument {
    Instrument {
        id: 0,
        symbol: "BTCUSDT".into(),
        base: "BTC".into(),
        quote: "USDT".into(),
        tick: Decimal::ONE,
        step: Decimal::ONE,
        min_qty: Decimal::ONE,
        max_qty: Decimal::from(100_000),
        min_notional: Decimal::ONE,
        min_price: Decimal::ZERO,
        max_price: Decimal::MAX,
    }
}
pub fn runtime(config: Option<Config>) -> (Runtime, Ledger) {
    let c = config.unwrap_or(Config {
        book_levels: 1,
        ofi_window: 1,
        momentum_window_ns: 1000,
        base_quantity: 10,
        ..Config::default()
    });
    let mut a = Ledger::new(vec!["BTC".into(), "USDT".into()]);
    a.begin(Decimal::ZERO);
    a.set_balance(0, Decimal::from(100), Decimal::ZERO);
    a.set_balance(1, Decimal::from(100_000), Decimal::ZERO);
    a.ready = true;
    let mut r = Runtime::new(instrument(), c, &a, 100, 200, 0, false);
    r.balance_changed(&mut a, None);
    assert!(r.load(
        &[
            Level {
                price_ticks: 100,
                quantity: 30
            },
            Level {
                price_ticks: 99,
                quantity: 20
            }
        ],
        &[
            Level {
                price_ticks: 102,
                quantity: 10
            },
            Level {
                price_ticks: 103,
                quantity: 20
            }
        ],
        9
    ));
    (r, a)
}
pub fn warm(r: &mut Runtime, a: &Ledger) {
    assert!(r.begin_depth(10, 10));
    r.end_depth(0, a);
    assert!(r.begin_depth(11, 11));
    r.end_depth(1000, a);
    assert!(r.ready(a));
}
fn push_i64(v: &mut Vec<u8>, x: i64) {
    v.extend_from_slice(&x.to_le_bytes());
}
pub fn depth(id: i64, time_us: i64, symbol: &str, pe: i8, qe: i8) -> Vec<u8> {
    let mut v = Vec::new();
    for x in [26u16, 10003, 1, 0] {
        v.extend_from_slice(&x.to_le_bytes());
    }
    push_i64(&mut v, time_us);
    push_i64(&mut v, id);
    push_i64(&mut v, id);
    v.extend_from_slice(&[pe as u8, qe as u8]);
    for (p, q) in [(100, 30), (102, 10)] {
        v.extend_from_slice(&16u16.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        push_i64(&mut v, p);
        push_i64(&mut v, q);
    }
    v.push(symbol.len() as u8);
    v.extend_from_slice(symbol.as_bytes());
    v
}
pub fn quote(symbol: &str, id: i64, pe: i8, qe: i8) -> Vec<u8> {
    let mut v = Vec::new();
    for x in [50u16, 10001, 1, 0] {
        v.extend_from_slice(&x.to_le_bytes());
    }
    push_i64(&mut v, 0);
    push_i64(&mut v, id);
    v.extend_from_slice(&[pe as u8, qe as u8]);
    for x in [100i64, 30, 102, 10] {
        push_i64(&mut v, x);
    }
    v.push(symbol.len() as u8);
    v.extend_from_slice(symbol.as_bytes());
    v
}
pub fn trades(time: i64, count: usize) -> Vec<u8> {
    let mut v = Vec::new();
    for x in [18u16, 10000, 1, 0] {
        v.extend_from_slice(&x.to_le_bytes());
    }
    push_i64(&mut v, time);
    push_i64(&mut v, time);
    v.extend_from_slice(&[0, 255]);
    v.extend_from_slice(&25u16.to_le_bytes());
    v.extend_from_slice(&(count as u32).to_le_bytes());
    for j in 0..count {
        push_i64(&mut v, j as i64 + 1);
        push_i64(&mut v, 101);
        push_i64(&mut v, 5);
        v.push(0);
    }
    v.push(7);
    v.extend_from_slice(b"BTCUSDT");
    v
}
