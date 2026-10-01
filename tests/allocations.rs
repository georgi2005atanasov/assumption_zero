mod common;
use assumption_zero::{feed::Feed, metrics};
#[test]
fn warmed_decode_book_signal_quote_path_has_zero_allocator_bytes() {
    let (r, a) = common::runtime(None);
    let mut feed = Feed::new(a, vec![r], true, false);
    let frames: Vec<_> = (0..400)
        .map(|i| common::depth(10 + i, i * 10, "BTCUSDT", 0, 0))
        .collect();
    for frame in &frames[..100] {
        feed.binary(frame).unwrap();
    }
    assert!(feed.runtimes[0].ready(&feed.ledger));
    metrics::track_allocations();
    let before = metrics::allocated();
    for frame in &frames[100..] {
        feed.binary(frame).unwrap();
    }
    assert_eq!(metrics::allocated() - before, 0);
    assert_eq!(feed.decisions, 400);
    let trade = common::trades(4010, 2);
    feed.binary(&trade).unwrap();
    let before = metrics::allocated();
    for _ in 0..100 {
        feed.binary(&trade).unwrap();
    }
    assert_eq!(metrics::allocated() - before, 0);
}
