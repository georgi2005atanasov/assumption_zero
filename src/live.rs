use crate::{
    domain::*,
    feed::{Feed, text_message},
    metrics::ticks,
    runtime::*,
    state::{decimal, levels, symbols},
};
use anyhow::{Context, Result};
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey, pkcs8::DecodePrivateKey};
use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{net::TcpStream, sync::mpsc};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async_with_config,
    tungstenite::{Message, client::IntoClientRequest, protocol::WebSocketConfig},
};
mod endpoints {
    include!("endpoints.rs");
}
pub const DEFAULT_PG_CONNECTION: &str = "Host=172.17.0.3;Database=assumption_zero;Username=postgres;Password=mysecretpassword;Trust Server Certificate=true;";

pub type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
pub fn required(key: &str) -> Result<String> {
    std::env::var(key).with_context(|| format!("{key} is required"))
}
fn millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
}
pub async fn connect(url: &str, key: Option<&str>) -> Result<Socket> {
    let mut req = url.into_client_request()?;
    if let Some(key) = key {
        req.headers_mut().insert("X-MBX-APIKEY", key.parse()?);
    }
    let config = WebSocketConfig::default()
        .max_message_size(Some(1024 * 1024))
        .max_frame_size(Some(1024 * 1024));
    Ok(connect_async_with_config(req, Some(config), true).await?.0)
}
pub fn stream_url(symbols: &[String]) -> String {
    format!(
        "{}{}",
        std::env::var("BINANCE_SBE_URL").unwrap_or_else(|_| endpoints::SBE.into()),
        symbols
            .iter()
            .flat_map(|s| [
                format!("{}@depth", s.to_lowercase()),
                format!("{}@bestBidAsk", s.to_lowercase()),
                format!("{}@trade", s.to_lowercase())
            ])
            .collect::<Vec<_>>()
            .join("/")
    )
}
pub fn sign(key: &SigningKey, payload: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(key.sign(payload.as_bytes()).to_bytes())
}
struct Bootstrap {
    key: String,
    signer: SigningKey,
    http: reqwest::Client,
    base: String,
}
impl Bootstrap {
    fn new() -> Result<Self> {
        Ok(Self {
            key: required("BINANCE_API_KEY")?,
            signer: SigningKey::from_pkcs8_pem(&std::fs::read_to_string(required(
                "BINANCE_ED25519_PRIVATE_KEY_PATH",
            )?)?)?,
            http: reqwest::Client::new(),
            base: std::env::var("BINANCE_HTTP_URL")
                .unwrap_or_else(|_| "https://testnet.binance.vision".into()),
        })
    }
    async fn get(&self, path: &str, signed: bool) -> Result<Value> {
        let mut req = self.http.get(format!("{}{path}", self.base));
        if signed {
            let time = millis().to_string();
            let payload = format!("timestamp={time}");
            req = req.header("X-MBX-APIKEY", &self.key).query(&[
                ("timestamp", time),
                ("signature", sign(&self.signer, &payload)),
            ]);
        }
        Ok(req.send().await?.error_for_status()?.json().await?)
    }
    async fn instruments(&self) -> Result<Vec<Instrument>> {
        let root = self.get("/api/v3/exchangeInfo", false).await?;
        let wanted = symbols();
        let mut result = Vec::new();
        for v in root["symbols"].as_array().context("symbols missing")? {
            let symbol = v["symbol"].as_str().context("symbol missing")?;
            if !wanted.iter().any(|s| s == symbol) || v["status"] != "TRADING" {
                continue;
            }
            let mut i = Instrument {
                id: result.len() as i64,
                symbol: symbol.into(),
                base: v["baseAsset"].as_str().context("base missing")?.into(),
                quote: v["quoteAsset"].as_str().context("quote missing")?.into(),
                tick: Decimal::ZERO,
                step: Decimal::ZERO,
                min_qty: Decimal::ZERO,
                max_qty: Decimal::ZERO,
                min_notional: Decimal::ZERO,
                min_price: Decimal::ZERO,
                max_price: Decimal::MAX,
            };
            for f in v["filters"].as_array().context("filters missing")? {
                match f["filterType"].as_str().unwrap_or("") {
                    "PRICE_FILTER" => {
                        i.tick = decimal(&f["tickSize"])?;
                        i.min_price = decimal(&f["minPrice"])?;
                        i.max_price = decimal(&f["maxPrice"])?;
                        if i.max_price == Decimal::ZERO {
                            i.max_price = Decimal::MAX;
                        }
                    }
                    "LOT_SIZE" => {
                        i.step = decimal(&f["stepSize"])?;
                        i.min_qty = decimal(&f["minQty"])?;
                        i.max_qty = decimal(&f["maxQty"])?;
                    }
                    "NOTIONAL" | "MIN_NOTIONAL" => i.min_notional = decimal(&f["minNotional"])?,
                    _ => (),
                }
            }
            if i.tick <= Decimal::ZERO || i.step <= Decimal::ZERO || i.max_qty <= Decimal::ZERO {
                continue;
            }
            i.validate()?;
            result.push(i);
        }
        anyhow::ensure!(!result.is_empty(), "no configured testnet instruments");
        Ok(result)
    }
    async fn account(&self, f: &mut Feed, first: bool) -> Result<()> {
        let v = self.get("/api/v3/account", true).await?;
        f.ledger.begin(decimal(&v["commissionRates"]["maker"])?);
        for r in &mut f.runtimes {
            r.begin_reconciliation();
        }
        for b in v["balances"].as_array().context("balances missing")? {
            if let Some(id) = f
                .ledger
                .asset(b["asset"].as_str().context("asset missing")?)
            {
                f.ledger
                    .set_balance(id, decimal(&b["free"])?, decimal(&b["locked"])?);
            }
        }
        for r in &mut f.runtimes {
            r.balance_changed(&mut f.ledger, None);
            if first {
                let q = r.inventory.current;
                r.configure_inventory(
                    q,
                    q.checked_add(r.config.base_quantity * 10)
                        .context("inventory overflow")?,
                    0,
                );
            }
        }
        let orders = self.get("/api/v3/openOrders", true).await?;
        for v in orders.as_array().context("orders missing")? {
            let Some(r) = f
                .runtimes
                .iter_mut()
                .find(|r| v["symbol"] == r.instrument.symbol)
            else {
                continue;
            };
            let s = match v["side"].as_str() {
                Some("BUY") => 0,
                Some("SELL") => 1,
                _ => anyhow::bail!("unknown order side"),
            };
            anyhow::ensure!(!r.orders[s].active, "multiple open orders on one side");
            let status = Status::parse(v["status"].as_str().context("order status missing")?)?;
            anyhow::ensure!(
                matches!(
                    status,
                    Status::New | Status::PartiallyFilled | Status::PendingCancel
                ),
                "unexpected open order status"
            );
            let o = Order {
                id: v["orderId"].as_i64().context("order ID missing")?,
                price: r.instrument.price_ticks(decimal(&v["price"])?),
                quantity: r.instrument.quantity_units(decimal(&v["origQty"])?),
                filled: r.instrument.quantity_units(decimal(&v["executedQty"])?),
                status,
                active: true,
            };
            anyhow::ensure!(
                o.id > 0
                    && o.price > 0
                    && o.quantity > 0
                    && o.filled >= 0
                    && o.filled <= o.quantity,
                "invalid open order"
            );
            let mut orders = r.orders;
            orders[s] = o;
            r.set_orders(orders);
        }
        f.ledger.ready = true;
        Ok(())
    }
    async fn books(&self, f: &mut Feed) -> Result<()> {
        for r in &mut f.runtimes {
            let v = self
                .get(
                    &format!("/api/v3/depth?symbol={}&limit=100", r.instrument.symbol),
                    false,
                )
                .await?;
            let b = levels(&r.instrument, &v["bids"], false)?;
            let a = levels(&r.instrument, &v["asks"], false)?;
            anyhow::ensure!(
                r.load(
                    &b,
                    &a,
                    v["lastUpdateId"].as_i64().context("book update missing")?
                ),
                "incomplete depth snapshot"
            );
        }
        Ok(())
    }
}
#[derive(Debug)]
pub enum AccountEvent {
    Balance {
        asset: usize,
        free: Decimal,
        locked: Decimal,
    },
    Execution {
        runtime: usize,
        buy: bool,
        id: i64,
        filled: i64,
        price: i64,
        time: i64,
        status: Status,
    },
}
// Parsing and allocation happen on the account socket producer, outside the market writer.
pub fn account_events(
    v: &Value,
    assets: &[String],
    instruments: &[Instrument],
) -> Result<Vec<AccountEvent>> {
    let Some(v) = v.get("event") else {
        return Ok(vec![]);
    };
    let mut result = Vec::new();
    match v["e"].as_str() {
        Some("outboundAccountPosition") => {
            for b in v["B"].as_array().context("balance array missing")? {
                if let Some(asset) = assets.iter().position(|s| b["a"] == *s) {
                    result.push(AccountEvent::Balance {
                        asset,
                        free: decimal(&b["f"])?,
                        locked: decimal(&b["l"])?,
                    });
                }
            }
        }
        Some("executionReport") => {
            let runtime = instruments
                .iter()
                .position(|i| v["s"] == i.symbol)
                .context("unknown execution symbol")?;
            let i = &instruments[runtime];
            let id = v["i"].as_i64().context("execution order ID missing")?;
            anyhow::ensure!(id > 0, "invalid order ID");
            result.push(AccountEvent::Execution {
                runtime,
                buy: v["S"] == "BUY",
                id,
                filled: i.quantity_units(decimal(&v["z"])?),
                price: i.price_ticks(decimal(&v["L"])?),
                time: v["T"]
                    .as_i64()
                    .context("execution time missing")?
                    .checked_mul(1_000_000)
                    .context("time overflow")?,
                status: Status::parse(v["X"].as_str().context("execution status missing")?)?,
            });
        }
        _ => anyhow::bail!("unsupported account event; reconciliation lost"),
    }
    Ok(result)
}
fn drain(f: &mut Feed, rx: &mut mpsc::Receiver<AccountEvent>, fault: &AtomicBool) -> Result<()> {
    if fault.load(Ordering::Acquire) {
        f.ledger.ready = false;
        for r in &mut f.runtimes {
            r.invalidate();
        }
        anyhow::bail!("account stream lost reconciliation");
    }
    while let Ok(e) = rx.try_recv() {
        match e {
            AccountEvent::Balance {
                asset,
                free,
                locked,
            } => {
                f.ledger.set_balance(asset, free, locked);
                for r in &mut f.runtimes {
                    r.balance_changed(&mut f.ledger, Some(asset));
                }
            }
            AccountEvent::Execution {
                runtime,
                buy,
                id,
                filled,
                price,
                time,
                status,
            } => f.runtimes[runtime].execution(buy, id, filled, price, time, status, &mut f.ledger),
        }
    }
    Ok(())
}
pub async fn normal(log: bool) -> Result<()> {
    let boot = Bootstrap::new()?;
    let instruments = boot.instruments().await?;
    let mut assets = Vec::new();
    for i in &instruments {
        for a in [&i.base, &i.quote] {
            if !assets.contains(a) {
                assets.push(a.clone());
            }
        }
    }
    let ledger = Ledger::new(assets);
    let runtimes = instruments
        .iter()
        .cloned()
        .map(|i| Runtime::new(i, Config::default(), &ledger, 0, 10_000, 0, false))
        .collect();
    let mut feed = Feed::new(ledger, runtimes, false, log);
    let account_url = std::env::var("BINANCE_ACCOUNT_URL")
        .unwrap_or_else(|_| "wss://ws-api.testnet.binance.vision/ws-api/v3".into());
    let mut socket = connect(&account_url, None).await?;
    let time = millis();
    let signature = sign(
        &boot.signer,
        &format!("apiKey={}&timestamp={time}", boot.key),
    );
    socket.send(Message::Text(json!({"id":1,"method":"userDataStream.subscribe.signature","params":{"apiKey":boot.key,"timestamp":time as u64,"signature":signature}}).to_string().into())).await?;
    let response = socket
        .next()
        .await
        .context("account subscription closed")??;
    anyhow::ensure!(response.is_text(), "non-text subscription response");
    let v: Value = serde_json::from_slice(&response.into_data())?;
    anyhow::ensure!(v["status"] == 200, "account subscription failed");
    let (tx, mut rx) = mpsc::channel(8192);
    let fault = Arc::new(AtomicBool::new(false));
    let task_fault = fault.clone();
    let assets = feed.ledger.assets.clone();
    let account_task = tokio::spawn(async move {
        let result: Result<()> = async {
            while let Some(message) = socket.next().await {
                let message = message?;
                match message {
                    Message::Text(t) => {
                        let v = serde_json::from_str(&t)?;
                        for e in account_events(&v, &assets, &instruments)? {
                            tx.try_send(e).context("account queue overflow")?;
                        }
                    }
                    Message::Ping(_) | Message::Pong(_) => (),
                    _ => anyhow::bail!("account socket closed or unexpected format"),
                }
            }
            anyhow::bail!("account socket ended")
        }
        .await;
        if result.is_err() {
            task_fault.store(true, Ordering::Release);
        }
    });
    let mut first = true;
    let result=async{loop{
        feed.ledger.ready=false;boot.account(&mut feed,first).await?;first=false;
        let names=feed.runtimes.iter().map(|r|r.instrument.symbol.clone()).collect::<Vec<_>>();let mut market=connect(&stream_url(&names),Some(&boot.key)).await?;boot.books(&mut feed).await?;
        let result:Result<()>=async{loop{tokio::select!{
            _=tokio::signal::ctrl_c()=>return Ok(()),
            next=market.next()=>{let msg=next.context("market socket ended")??;match msg{Message::Binary(b)=>{drain(&mut feed,&mut rx,&fault)?;feed.binary(&b)?;},Message::Text(t)=>text_message(t.as_bytes(),log)?,Message::Ping(_)|Message::Pong(_)=>(),_=>anyhow::bail!("market socket closed")}}
        }}}.await;
        match result{Ok(())=>return Ok(()),Err(e)=>{feed.ledger.ready=false;for r in &mut feed.runtimes{r.invalidate();}if fault.load(Ordering::Acquire){return Err(e);}eprintln!("Market feed resynchronizing: {e}");tokio::select!{_=tokio::signal::ctrl_c()=>return Ok(()),_=tokio::time::sleep(std::time::Duration::from_secs(1))=>()}}}
    }}.await;
    account_task.abort();
    let _ = account_task.await;
    result
}
pub fn pg_config(s: &str) -> Result<tokio_postgres::Config> {
    Ok(pg_settings(s)?.0)
}
fn pg_settings(s: &str) -> Result<(tokio_postgres::Config, bool)> {
    if !s.contains(';') {
        return Ok((s.parse()?, false));
    }
    let mut c = tokio_postgres::Config::new();
    let mut trust_certificate = false;
    // Npgsql-style local connection strings. Quoted values support semicolons.
    let mut fields = Vec::new();
    let (mut part, mut quote) = (String::new(), None);
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if let Some(q) = quote {
            if ch == q {
                if chars.peek() == Some(&q) {
                    part.push(ch);
                    chars.next();
                } else {
                    quote = None;
                }
            } else {
                part.push(ch);
            }
        } else if ch == '\'' || ch == '"' {
            quote = Some(ch);
        } else if ch == ';' {
            fields.push(std::mem::take(&mut part));
        } else {
            part.push(ch);
        }
    }
    anyhow::ensure!(quote.is_none(), "unterminated connection-string quote");
    fields.push(part);
    for field in fields {
        if field.trim().is_empty() {
            continue;
        }
        let (k, v) = field
            .split_once('=')
            .context("invalid PG_CONNECTION field")?;
        let v = v.trim();
        match k.trim().to_lowercase().as_str() {
            "host" | "server" => {
                c.host(v);
            }
            "database" => {
                c.dbname(v);
            }
            "username" | "user id" | "user" => {
                c.user(v);
            }
            "password" => {
                c.password(v);
            }
            "port" => {
                c.port(v.parse()?);
            }
            "timeout" | "connection timeout" => {
                c.connect_timeout(std::time::Duration::from_secs(v.parse()?));
            }
            "ssl mode" => {
                c.ssl_mode(match v.to_lowercase().as_str() {
                    "disable" => tokio_postgres::config::SslMode::Disable,
                    "prefer" => tokio_postgres::config::SslMode::Prefer,
                    "require" | "verifyfull" | "verify-full" => {
                        tokio_postgres::config::SslMode::Require
                    }
                    _ => anyhow::bail!("unsupported PostgreSQL SSL mode"),
                });
            }
            "search path" => {
                c.options(format!("-csearch_path={v}"));
            }
            "trust server certificate" => {
                trust_certificate =
                    v.parse::<bool>()
                        .or_else(|_| match v.to_lowercase().as_str() {
                            "true" => Ok(true),
                            "false" => Ok(false),
                            _ => Err(anyhow::anyhow!("invalid Trust Server Certificate")),
                        })?;
            }
            "pooling" => (),
            _ => anyhow::bail!("unsupported PostgreSQL option {k}"),
        }
    }
    Ok((c, trust_certificate))
}
#[derive(Debug)]
struct ExplicitlyTrustedCertificate;
impl rustls::client::danger::ServerCertVerifier for ExplicitlyTrustedCertificate {
    fn verify_server_cert(
        &self,
        _end: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
fn pg_tls(trust_certificate: bool) -> Result<tokio_postgres_rustls::MakeRustlsConnect> {
    let mut roots = rustls::RootCertStore::empty();
    for certificate in rustls_native_certs::load_native_certs().certs {
        roots.add(certificate)?;
    }
    let mut config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    // Only the user's explicit connection-string setting bypasses certificate trust.
    if trust_certificate {
        config
            .dangerous()
            .set_certificate_verifier(Arc::new(ExplicitlyTrustedCertificate));
    }
    Ok(tokio_postgres_rustls::MakeRustlsConnect::new(config))
}
pub async fn capture(log: bool) -> Result<()> {
    let (config, trust_certificate) = pg_settings(
        &std::env::var("PG_CONNECTION")
            .ok()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| DEFAULT_PG_CONNECTION.to_owned()),
    )?;
    let (client, connection) = config.connect(pg_tls(trust_certificate)?).await?;
    let pg_task = tokio::spawn(connection);
    let statement=client.prepare("INSERT INTO recorded_messages (recording_id,sequence_number,elapsed_ns,message_type,payload) VALUES ($1,$2,$3,$4,$5)").await?;
    let (tx, mut rx) = mpsc::channel::<(uuid::Uuid, i64, i64, Vec<u8>)>(8192);
    let writer = tokio::spawn(async move {
        while let Some((id, seq, time, payload)) = rx.recv().await {
            client
                .execute(&statement, &[&id, &seq, &time, &1i16, &payload])
                .await?;
        }
        Ok::<(), tokio_postgres::Error>(())
    });
    let result:Result<()>=async{let key=required("BINANCE_API_KEY")?;let mut socket=connect(&stream_url(&symbols()),Some(&key)).await?;let origin=ticks();let id=uuid::Uuid::new_v4();let mut seq=0i64;println!("Recording {id}");loop{tokio::select!{
        _=tokio::signal::ctrl_c()=>break,
        next=socket.next()=>{match next.context("capture socket ended")??{Message::Binary(b)=>{seq+=1;tx.try_send((id,seq,ticks()-origin,b.to_vec())).context("recording queue full or writer failed")?;},Message::Text(t)=>text_message(t.as_bytes(),log)?,Message::Ping(_)|Message::Pong(_)=>(),_=>anyhow::bail!("capture socket closed")}}
    }}Ok(())}.await;
    drop(tx);
    let written = writer.await?;
    pg_task.abort();
    written?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    #[tokio::test]
    async fn bootstrap_uses_local_protocol_fixtures_for_filters_account_orders_and_book() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let responses = [
                json!({"symbols":[{"symbol":"BTCUSDT","status":"TRADING","baseAsset":"BTC","quoteAsset":"USDT","filters":[
                    {"filterType":"PRICE_FILTER","tickSize":"0.01","minPrice":"0.01","maxPrice":"0"},
                    {"filterType":"LOT_SIZE","stepSize":"0.001","minQty":"0.001","maxQty":"100000"},
                    {"filterType":"MIN_NOTIONAL","minNotional":"1"}]}]}),
                json!({"commissionRates":{"maker":"0.001"},"balances":[{"asset":"BTC","free":"100","locked":"0"},{"asset":"USDT","free":"1000000","locked":"0"}]}),
                json!([{"symbol":"BTCUSDT","side":"BUY","status":"PARTIALLY_FILLED","orderId":42,"price":"100","origQty":"1","executedQty":"0.25"}]),
                json!({"lastUpdateId":123,"bids":[["100","10"]],"asks":[["101","20"]]}),
            ];
            for (n, response) in responses.iter().enumerate() {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut bytes = [0; 1024];
                    let count = socket.read(&mut bytes).await.unwrap();
                    assert!(count > 0);
                    request.extend_from_slice(&bytes[..count]);
                    if request.windows(4).any(|b| b == b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8(request).unwrap();
                if n == 1 || n == 2 {
                    assert!(request.contains("timestamp="));
                    assert!(request.contains("signature="));
                    assert!(request.to_lowercase().contains("x-mbx-apikey: test"));
                }
                let body = response.to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let boot = Bootstrap {
            key: "test".into(),
            signer: SigningKey::from_bytes(&[7; 32]),
            http: reqwest::Client::new(),
            base,
        };
        let instruments = boot.instruments().await.unwrap();
        assert_eq!(instruments[0].tick, Decimal::new(1, 2));
        assert_eq!(instruments[0].max_price, Decimal::MAX);
        let ledger = Ledger::new(vec!["BTC".into(), "USDT".into()]);
        let r = Runtime::new(
            instruments[0].clone(),
            Config {
                book_levels: 1,
                ..Config::default()
            },
            &ledger,
            0,
            10000,
            0,
            false,
        );
        let mut feed = Feed::new(ledger, vec![r], false, false);
        boot.account(&mut feed, true).await.unwrap();
        assert!(feed.ledger.ready);
        assert_eq!(feed.runtimes[0].inventory.target, 100000);
        assert_eq!(feed.runtimes[0].orders[0].filled, 250);
        assert_eq!(feed.runtimes[0].inventory.reserved_buy, 750);
        boot.books(&mut feed).await.unwrap();
        assert!(feed.runtimes[0].book.synchronized);
        assert_eq!(feed.runtimes[0].book.last_id, 123);
        server.await.unwrap();
    }
    #[tokio::test]
    async fn account_queue_overflow_fault_invalidates_market_state() {
        let (ledger, runtimes) = crate::state::synthetic();
        let mut feed = Feed::new(ledger, runtimes, false, false);
        let (tx, mut rx) = mpsc::channel(2);
        for _ in 0..2 {
            tx.try_send(AccountEvent::Balance {
                asset: 0,
                free: Decimal::ONE,
                locked: Decimal::ZERO,
            })
            .unwrap();
        }
        assert!(
            tx.try_send(AccountEvent::Balance {
                asset: 0,
                free: Decimal::ONE,
                locked: Decimal::ZERO
            })
            .is_err()
        );
        assert!(drain(&mut feed, &mut rx, &AtomicBool::new(true)).is_err());
        assert!(!feed.ledger.ready);
    }
}
