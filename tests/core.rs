mod common;
use assumption_zero::{
    domain::*,
    feed::{Feed, text_message, validate},
    live,
    metrics::{Benchmark, Latency, final_state},
    state,
};
use common::*;
use rust_decimal::{Decimal, prelude::*};
use serde_json::Value;
use std::str::FromStr;
fn near(a: f64, b: f64) {
    assert!((a - b).abs() < 1e-12, "{a} != {b}");
}
#[test]
fn hand_calculated_signals_and_markout() {
    let c = Config {
        book_levels: 2,
        ofi_window: 1,
        momentum_window_ns: 1_000_000_000,
        ..Config::default()
    };
    let (mut r, _) = runtime(Some(c));
    let mut s = Signals::default();
    r.memory.add_ofi(5.0);
    r.memory.add_trade(0, 10.0, true, &c);
    r.memory.add_trade(0, 5.0, false, &c);
    near(r.memory.update_book(0, &r.book, &c, &mut s), 0.0);
    near(s.micro_signal, 0.5);
    near(s.multi_level_imbalance, 1.0 / 3.0);
    near(s.order_flow_imbalance, 1.0);
    near(s.trade_flow_imbalance, 1.0 / 3.0);
    near(s.liquidity_ratio, 1.0);
    r.memory.add_fill(0, 101, true);
    for (p, q, bid) in [
        (100, 0, true),
        (99, 0, true),
        (98, 10, true),
        (97, 10, true),
        (102, 0, false),
        (100, 10, false),
    ] {
        assert!(r.book.apply(
            Level {
                price_ticks: p,
                quantity: q
            },
            bid
        ));
    }
    assert!(r.book.complete(10, 2));
    near(r.memory.update_book(250_000_000, &r.book, &c, &mut s), 0.4);
}
#[test]
fn hand_calculated_quotes_and_filters() {
    let c = Config {
        book_levels: 1,
        ofi_window: 1,
        mli_weight: 0.0,
        ofi_weight: 0.0,
        trade_flow_weight: 0.0,
        momentum_weight: 0.0,
        volatility_spread_weight_ticks: 0.0,
        liquidity_spread_weight_ticks: 0.0,
        adverse_spread_weight_ticks: 0.0,
        inventory_spread_weight_ticks: 0.0,
        base_quantity: 10,
        ..Config::default()
    };
    let (mut r, _) = runtime(Some(c));
    let s = Signals {
        micro_signal: 0.5,
        liquidity_ratio: 1.0,
        ..Signals::default()
    };
    let mut p = Pricing::default();
    let eval = |r: &assumption_zero::runtime::Runtime, p: &mut Pricing| {
        evaluate(
            &r.instrument,
            &c,
            &r.book,
            &s,
            &r.inventory,
            &r.orders,
            p,
            0.0,
            Decimal::ZERO,
            Decimal::from(100),
            Decimal::from(100_000),
        )
    };
    let d = eval(&r, &mut p);
    assert_eq!(
        (
            d.bid,
            d.ask,
            d.bid_qty,
            d.ask_qty,
            d.bid_action,
            d.ask_action
        ),
        (100, 104, 10, 10, Action::Place, Action::Place)
    );
    r.orders = [
        Order {
            id: 1,
            price: 100,
            quantity: 10,
            status: Status::New,
            active: true,
            ..Order::default()
        },
        Order {
            id: 2,
            price: 104,
            quantity: 10,
            status: Status::New,
            active: true,
            ..Order::default()
        },
    ];
    assert_eq!(eval(&r, &mut p).bid_action, Action::Keep);
    r.orders[0].quantity = 9;
    assert_eq!(eval(&r, &mut p).bid_action, Action::Replace);
    r.orders = [Order::default(); 2];
    r.instrument.min_qty = Decimal::from(20);
    assert_eq!(eval(&r, &mut p).bid_action, Action::None);
    r.instrument.min_qty = Decimal::ONE;
    r.instrument.max_price = Decimal::from(103);
    let d = eval(&r, &mut p);
    assert_eq!((d.bid_action, d.ask_action), (Action::Place, Action::None));
}
#[test]
fn gaps_overflow_partial_fills_and_cancels() {
    let (mut r, mut a) = runtime(None);
    assert!(!r.book.can_apply(11, 11));
    warm(&mut r, &a);
    r.set_orders([
        Order {
            id: 42,
            price: 100,
            quantity: 10,
            status: Status::New,
            active: true,
            ..Order::default()
        },
        Order::default(),
    ]);
    r.execution(true, 42, 3, 100, 2000, Status::PartiallyFilled, &mut a);
    assert_eq!(r.inventory.current, 103);
    assert_eq!(r.inventory.reserved_buy, 7);
    r.invalidate();
    let d = r.evaluate(&a);
    assert_eq!(d.bid_action, Action::Cancel);
    assert!(r.accept_intent(true, d, &mut a).unwrap() > 0);
    assert_eq!(r.evaluate(&a).bid_action, Action::None);
    r.execution(true, 42, 3, 0, 3000, Status::Cancelled, &mut a);
    assert_eq!(r.inventory.reserved_buy, 0);
    for _ in 0..8193 {
        r.memory.add_trade(0, 1.0, true, &r.config);
    }
    assert!(r.memory.faulted);
    assert!(!r.begin_depth(13, 13));
    assert!(!r.book.synchronized);
    assert!(r.load(
        &[Level {
            price_ticks: 100,
            quantity: 30
        }],
        &[Level {
            price_ticks: 102,
            quantity: 10
        }],
        20
    ));
    assert!(!r.memory.faulted);
    assert!(!r.ready(&a));
}
#[test]
fn accepted_rejected_and_acknowledged_intents() {
    let (mut r, mut a) = runtime(None);
    warm(&mut r, &a);
    let d = r.evaluate(&a);
    let before = a.free(1);
    let id = r.accept_intent(true, d, &mut a).unwrap();
    assert!(a.free(1) < before);
    assert!(!r.acknowledge(true, id, 0));
    r.reject_intent(true, id, &mut a);
    assert_eq!(a.free(1), before);
    let d = r.evaluate(&a);
    let id = r.accept_intent(true, d, &mut a).unwrap();
    assert!(r.acknowledge(true, id, 123));
    r.execution(true, 123, 3, 100, 2000, Status::PartiallyFilled, &mut a);
    assert!(r.book.synchronized);
    assert_eq!(r.orders[0].filled, 3);
    r.execution(true, 999, 3, 100, 2001, Status::New, &mut a);
    assert!(!r.book.synchronized);
}
#[test]
fn replacements_hold_funds_until_cancel_confirmation() {
    let (mut r, mut a) = runtime(None);
    warm(&mut r, &a);
    r.set_orders([
        Order {
            id: 42,
            price: 90,
            quantity: 10,
            status: Status::New,
            active: true,
            ..Order::default()
        },
        Order::default(),
    ]);
    let d = r.evaluate(&a);
    assert_eq!(d.bid_action, Action::Replace);
    let before = a.free(1);
    let id = r.accept_intent(true, d, &mut a).unwrap();
    assert_eq!(r.orders[0].status, Status::PendingCancel);
    assert!(a.free(1) < before - Decimal::from(900));
    r.execution(true, 42, 0, 0, 2000, Status::Cancelled, &mut a);
    assert_eq!(r.orders[0].status, Status::PendingNew);
    assert_eq!(r.orders[0].price, d.bid);
    assert!(r.acknowledge(true, id, 43));
    a.set_balance(1, Decimal::from(100_000), Decimal::ZERO);
    r.balance_changed(&mut a, Some(1));
    assert_eq!(a.free(1), Decimal::from(100_000));
}
#[test]
fn shared_reservations_and_full_book_removal() {
    let (_, mut a) = runtime(None);
    a.begin(Decimal::ZERO);
    a.set_balance(1, Decimal::from(100), Decimal::ZERO);
    a.ready = true;
    let i = instrument();
    let id = a.reserve(&i, true, 60, 1, true).unwrap();
    assert!(a.reserve(&i, true, 60, 1, true).is_none());
    assert_eq!(a.free(1), Decimal::from(40));
    assert!(a.release(id));
    a.set_balance(1, Decimal::from(40), Decimal::from(60));
    let id = a.reserve(&i, true, 60, 1, false).unwrap();
    a.set_balance(1, Decimal::from(100), Decimal::ZERO);
    assert_eq!(a.free(1), Decimal::from(40));
    a.release(id);
    assert_eq!(a.free(1), Decimal::from(100));
    let mut b = Book::new(1);
    b.load(
        &[Level {
            price_ticks: 100,
            quantity: 10,
        }],
        &[Level {
            price_ticks: 102,
            quantity: 10,
        }],
        1,
    );
    assert!(!b.apply(
        Level {
            price_ticks: 100,
            quantity: 0
        },
        true
    ));
}
#[test]
fn decimal_vectors_exported_from_csharp() {
    let rows: Vec<Value> = serde_json::from_str(include_str!("numeric-vectors.json")).unwrap();
    for v in rows {
        let a = Decimal::from_str(v["Dividend"].as_str().unwrap()).unwrap();
        let b = Decimal::from_str(v["Divisor"].as_str().unwrap()).unwrap();
        let q = a / b;
        assert_eq!(
            q,
            Decimal::from_str(v["Quotient"].as_str().unwrap()).unwrap(),
            "{a}/{b}"
        );
        assert_eq!(q.trunc().to_i64(), v["Truncated"].as_i64());
        assert_eq!(
            q.to_f64().unwrap().to_bits() as i64,
            v["DoubleBits"].as_i64().unwrap(),
            "double conversion of {q}"
        );
    }
    let mut i = instrument();
    i.tick = Decimal::new(1, 2);
    assert_eq!(i.price_ticks(Decimal::from_str("-1.999").unwrap()), -199);
    assert!(std::panic::catch_unwind(|| i.quantity_units(Decimal::MAX)).is_err());
    assert!(power10(-29).is_err());
}
#[test]
fn sbe_validation_rejects_truncation_unknown_and_oversized_groups() {
    let d = depth(10, 0, "BTCUSDT", 0, 0);
    assert!(validate(&d).is_some());
    for n in 0..d.len() {
        assert!(validate(&d[..n]).is_none());
    }
    let mut bad = d.clone();
    bad[4] = 2;
    assert!(validate(&bad).is_none());
    bad = d.clone();
    bad[2..4].copy_from_slice(&10002u16.to_le_bytes());
    assert!(validate(&bad).is_none());
    bad = d;
    bad[36..38].copy_from_slice(&5001u16.to_le_bytes());
    assert!(validate(&bad).is_none());
    let (r, a) = runtime(None);
    let mut f = Feed::new(a, vec![r], true, false);
    f.binary(&bad).unwrap();
    f.replay = false;
    assert!(f.binary(&bad).is_err());
    assert!(text_message(br#"{"e":"serverShutdown"}"#, false).is_err());
    assert!(text_message(b"invalid json", false).is_err());
}
#[test]
fn synthetic_precision_and_depth_only_start_match_repeated_runs() {
    fn run() -> String {
        let (a, rs) = state::synthetic();
        let mut f = Feed::new(a, rs, true, false);
        f.binary(&quote("BTCUSDT", 9, -2, -3)).unwrap();
        assert_eq!(f.runtimes[0].instrument.tick, Decimal::new(1, 2));
        assert_eq!(f.runtimes[0].inventory.target, 100_000);
        f.binary(&depth(10, 0, "BTCUSDT", -2, -3)).unwrap();
        f.binary(&depth(10, 0, "ETHUSDT", 0, 0)).unwrap();
        assert!(f.runtimes[1].book.synchronized);
        assert_eq!(f.runtimes[1].book.last_id, 10);
        final_state(&f.runtimes, &f.ledger)
    }
    assert_eq!(run(), run());
}
#[test]
fn benchmark_counts_multi_trade_decisions_and_interrupted_warmup() {
    let (mut r, a) = runtime(None);
    warm(&mut r, &a);
    r.stages = true;
    let mut f = Feed::new(a, vec![r], true, false);
    f.benchmark = Some(Benchmark::new(2, 0).unwrap());
    let frame = trades(1010, 2);
    f.benchmark
        .as_mut()
        .unwrap()
        .begin(1, frame.len(), assumption_zero::metrics::ticks())
        .unwrap();
    f.binary(&frame).unwrap();
    f.benchmark.as_mut().unwrap().end();
    let b = f.benchmark.unwrap();
    assert_eq!(b.frames[0].decisions, 2);
    assert!(b.frames[0].last_decision_ticks > 0);
    let mut b = Benchmark::new(10, 5).unwrap();
    b.begin(1, 16, assumption_zero::metrics::ticks()).unwrap();
    b.end();
    let v = b.finish(false, "");
    assert_eq!(v["MeasuredFrames"], 0);
    assert_eq!(v["Completed"], false);
    assert_eq!(Latency::from(vec![3, 1, 2, 4]).p50, 2);
}
#[test]
fn account_event_mapping_and_ed25519_signing() {
    let (_, a) = runtime(None);
    let v:Value=serde_json::from_str(r#"{"event":{"e":"executionReport","s":"BTCUSDT","S":"BUY","i":42,"z":"3","L":"100","T":2,"X":"PARTIALLY_FILLED"}}"#).unwrap();
    let events = live::account_events(&v, &a.assets, &[instrument()]).unwrap();
    assert!(matches!(
        events[0],
        live::AccountEvent::Execution {
            runtime: 0,
            buy: true,
            id: 42,
            filled: 3,
            price: 100,
            time: 2_000_000,
            status: Status::PartiallyFilled
        }
    ));
    assert!(
        live::account_events(
            &serde_json::json!({"event":{"e":"unknown"}}),
            &a.assets,
            &[instrument()]
        )
        .is_err()
    );
    use base64::Engine;
    use ed25519_dalek::Verifier;
    let key = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
    let signature = base64::engine::general_purpose::STANDARD
        .decode(live::sign(&key, "timestamp=123"))
        .unwrap();
    key.verifying_key()
        .verify(
            b"timestamp=123",
            &ed25519_dalek::Signature::from_slice(&signature).unwrap(),
        )
        .unwrap();
    let config=live::pg_config("Host=localhost;Database=test;Username=postgres;Password='with;semicolon';Trust Server Certificate=true;").unwrap();
    assert_eq!(config.get_password(), Some(b"with;semicolon".as_slice()));
}
