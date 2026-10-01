use crate::{domain::*, runtime::*};
use anyhow::{Context, Result};
use rust_decimal::Decimal;
use serde_json::Value;
use std::str::FromStr;
pub fn decimal(v: &Value) -> Result<Decimal> {
    Decimal::from_str(
        &v.as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| v.to_string()),
    )
    .context("invalid decimal")
}
pub fn symbols() -> Vec<String> {
    serde_json::from_str(include_str!("symbols.json")).unwrap()
}
pub fn synthetic() -> (Ledger, Vec<Runtime>) {
    let symbols = symbols();
    let mut assets = vec!["USDT".to_owned()];
    assets.extend(symbols.iter().map(|s| s[..s.len() - 4].to_owned()));
    let mut a = Ledger::new(assets);
    a.begin(Decimal::new(1, 3));
    a.set_balance(0, Decimal::from(1_000_000_000), Decimal::ZERO);
    for i in 0..symbols.len() {
        a.set_balance(i + 1, Decimal::from(100), Decimal::ZERO);
    }
    a.ready = true;
    let rs = symbols
        .into_iter()
        .enumerate()
        .map(|(id, symbol)| {
            let i = Instrument {
                id: id as i64,
                symbol,
                base: a.assets[id + 1].clone(),
                quote: "USDT".into(),
                tick: Decimal::ONE,
                step: Decimal::ONE,
                min_qty: Decimal::ONE,
                max_qty: Decimal::from(1_000_000),
                min_notional: Decimal::ZERO,
                min_price: Decimal::ZERO,
                max_price: Decimal::MAX,
            };
            let c = Config::default();
            let mut r = Runtime::new(i, c, &a, 100, 100 + c.base_quantity * 10, 0, true);
            r.balance_changed(&mut a, None);
            r
        })
        .collect();
    (a, rs)
}
pub fn levels(i: &Instrument, v: &Value, strict: bool) -> Result<Vec<Level>> {
    v.as_array()
        .context("levels must be an array")?
        .iter()
        .map(|v| {
            let p = decimal(&v[0])?;
            let q = decimal(&v[1])?;
            let l = Level {
                price_ticks: i.price_ticks(p),
                quantity: i.quantity_units(q),
            };
            anyhow::ensure!(
                !strict || (i.price(l.price_ticks) == p && i.quantity(l.quantity) == q),
                "off-grid replay level"
            );
            Ok(l)
        })
        .collect()
}
pub fn fixture(path: &str) -> Result<(Ledger, Vec<Runtime>)> {
    let v: Value = serde_json::from_slice(&std::fs::read(path)?)?;
    let balances = v["balances"].as_array().context("balances required")?;
    let assets = balances
        .iter()
        .map(|v| Ok(v["asset"].as_str().context("asset required")?.to_owned()))
        .collect::<Result<Vec<_>>>()?;
    let mut a = Ledger::new(assets);
    a.begin(decimal(&v["makerFeeRate"])?);
    for (i, b) in balances.iter().enumerate() {
        a.set_balance(i, decimal(&b["free"])?, decimal(&b["locked"])?);
    }
    a.ready = true;
    let mut rs = Vec::new();
    for (id, v) in v["instruments"]
        .as_array()
        .context("instruments required")?
        .iter()
        .enumerate()
    {
        let name = |k: &str| -> Result<String> {
            Ok(v[k]
                .as_str()
                .with_context(|| format!("{k} required"))?
                .to_owned())
        };
        let i = Instrument {
            id: id as i64,
            symbol: name("symbol")?,
            base: name("baseAsset")?,
            quote: name("quoteAsset")?,
            tick: decimal(&v["tickSize"])?,
            step: decimal(&v["stepSize"])?,
            min_qty: decimal(&v["minQuantity"])?,
            max_qty: decimal(&v["maxQuantity"])?,
            min_notional: decimal(&v["minNotional"])?,
            min_price: if v["minPrice"].is_null() {
                Decimal::ZERO
            } else {
                decimal(&v["minPrice"])?
            },
            max_price: if v["maxPrice"].is_null() {
                Decimal::MAX
            } else {
                decimal(&v["maxPrice"])?
            },
        };
        i.validate()?;
        let target = i.quantity_units(decimal(&v["targetQuantity"])?);
        let max = i.quantity_units(decimal(&v["maxLongQuantity"])?);
        let bids = levels(&i, &v["snapshot"]["bids"], true)?;
        let asks = levels(&i, &v["snapshot"]["asks"], true)?;
        let mut r = Runtime::new(i, Config::default(), &a, target, max, 0, false);
        r.balance_changed(&mut a, None);
        anyhow::ensure!(
            r.load(
                &bids,
                &asks,
                v["snapshot"]["lastUpdateId"]
                    .as_i64()
                    .context("snapshot ID required")?
            ),
            "invalid replay snapshot"
        );
        rs.push(r);
    }
    anyhow::ensure!(!rs.is_empty(), "no replay instruments");
    Ok((a, rs))
}
