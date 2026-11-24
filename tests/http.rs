//! The HTTP API on anvil, served in-process on a random port: quote, relay, status,
//! refusals by name, loss budgets, health, metrics and the ledger's stats. Needs
//! `anvil` on `PATH`.

#![allow(dead_code, unused_imports)]

include!("common/onboarding.rs");

use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        Mutex,
    },
    time::Duration,
};

use alloy::primitives::I256;
use relayer::{
    config::Budget,
    http::{
        serve,
        Service,
    },
    stats::stats,
};
use serde_json::{
    json,
    Value,
};

/// A key whose hex holds a marker no output may carry.
const MARKER: &str = "5ec7e75ec7e7";

/// A server on a random port, and every body it answered.
struct Server {
    url: String,
    service: Arc<Service<DynProvider>>,
    seen: Mutex<String>,
}

impl Server {
    /// `o`'s relayer under `config`, ticking every `every`.
    async fn start(o: &Onboarding, config: Config, db: &Path, every: Duration) -> Self {
        let relayer = Relayer::start(o.relayer.clone(), config, db).await.unwrap();
        let service = Service::new(relayer, Duration::from_secs(5));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(serve(
            service.clone(),
            every,
            listener,
            std::future::pending(),
        ));
        let seen = Mutex::default();
        Self { url, service, seen }
    }

    /// `path`'s status and body, JSON or text: a POST of `body` when given, else a GET.
    async fn call(&self, path: &str, body: Option<Value>) -> (u16, Value) {
        let (http, url) = (reqwest::Client::new(), format!("{}{path}", self.url));
        let request = match body {
            Some(body) => http.post(url).json(&body),
            None => http.get(url),
        };
        let response = request.send().await.unwrap();
        let headers = response.headers();
        assert_eq!(headers["cache-control"], "no-store");
        assert_eq!(headers["x-content-type-options"], "nosniff");
        let status = response.status().as_u16();
        let text = response.text().await.unwrap();
        self.seen.lock().unwrap().push_str(&text);
        let body = serde_json::from_str(&text).unwrap_or(Value::String(text));
        (status, body)
    }

    /// `request` relayed: the status and body answered.
    async fn relay(&self, request: &Request) -> (u16, Value) {
        self.call("/v1/relay", Some(wire(request))).await
    }

    /// Ticks and mines until request `id` is `status`.
    async fn settle(&self, o: &Onboarding, id: &Value, status: &str) -> Value {
        let path = format!("/v1/relay/{}", id.as_str().unwrap());
        for _ in 0..50 {
            self.service.tick().await;
            o.owner.evm_mine(None).await.unwrap();
            let (_, body) = self.call(&path, None).await;
            if body["status"] == status {
                return body;
            }
        }
        panic!("unsettled: {}", self.call(&path, None).await.1)
    }

    /// The onboarding relayed, then `planted` code at the wallet before its send is
    /// mined: it lands with no refund. Its loss: gas, plus the value fronted unless
    /// it reverted.
    async fn unrefunded(&self, o: &Onboarding, planted: &str) -> I256 {
        let (code, accepted) = self.relay(&o.request_for(&o.onboarding()).await).await;
        assert_eq!(code, 202, "{accepted}");
        o.owner.anvil_set_auto_mine(false).await.unwrap();
        self.service.tick().await;
        o.set_code(o.wallet, planted).await;
        o.owner.anvil_set_auto_mine(true).await.unwrap();
        let done = self.settle(o, &accepted["id"], "included").await;
        let value = if done["reverted"] == true {
            U256::ZERO
        } else {
            o.value
        };
        I256::try_from(cost(o, &done).await + value).unwrap()
    }

    /// The value of the gauge `name` at a scrape.
    async fn gauge(&self, name: &str) -> f64 {
        let (_, metrics) = self.call("/metrics", None).await;
        let (metrics, name) =
            (metrics.as_str().unwrap(), format!("libid_relayer_{name} "));
        let line = metrics.lines().find(|l| l.starts_with(&name)).unwrap();
        line.rsplit(' ').next().unwrap().parse().unwrap()
    }
}

/// `request` as JSON; a quote ignores its `gasLimit`.
fn wire(request: &Request) -> Value {
    let c = request.create.unwrap();
    let create =
        json!({"platformId": c.platform_id, "idNode": c.id_node, "salt": c.salt});
    json!({"wallet": request.wallet, "create": create, "data": request.data,
           "gasLimit": request.gas_limit})
}

/// The cost of the receipt of a status body's send: gas at its price.
async fn cost(o: &Onboarding, status: &Value) -> U256 {
    let hash: B256 = status["txHashes"][0].as_str().unwrap().parse().unwrap();
    let receipt = o
        .owner
        .get_transaction_receipt(hash)
        .await
        .unwrap()
        .unwrap();
    U256::from(receipt.gas_used) * U256::from(receipt.effective_gas_price)
}

fn name(body: &Value) -> &str {
    body["error"]["name"].as_str().unwrap_or_default()
}

/// Quoted, relayed and included over HTTP; health, metrics and stats then count it.
#[tokio::test]
async fn an_onboarding_relayed_over_http_lands_and_is_counted() {
    let (o, path) = (Onboarding::new(true).await, db("http"));
    let s = Server::start(&o, o.config(), &path, Duration::from_millis(100)).await;
    let request = o.request_for(&o.onboarding()).await;
    let (code, quote) = s.call("/v1/quote", Some(wire(&request))).await;
    assert_eq!(code, 200, "{quote}");
    assert_eq!(quote["relayer"], json!(o.relayer_address));
    assert!(
        quote["minRefund"]
            .as_str()
            .unwrap()
            .parse::<U256>()
            .unwrap()
            > U256::ZERO
    );
    let (code, accepted) = s.relay(&request).await;
    assert_eq!((code, &accepted["status"]), (202, &json!("queued")));
    let done = s.settle(&o, &accepted["id"], "included").await;
    assert_eq!(done["reverted"], false, "{done}");
    o.assert_onboarded(o.refund()).await;

    let (cost, net) = (cost(&o, &done).await, o.refund() - o.value);
    let stats = stats(&path).unwrap();
    assert_eq!(stats.requests, BTreeMap::from([("included".to_owned(), 1)]));
    let profit = I256::try_from(net).unwrap() - I256::try_from(cost).unwrap();
    assert_eq!((stats.profit, stats.profit_per_request), (profit, profit));
    assert_eq!(stats.cancel_rate, 0.0);
    assert_eq!(stats.margin, Some(f64::from(cost) / f64::from(net)));
    assert!(stats.inclusion_seconds.is_some());

    let noncanonical = json!({"wallet": o.wallet, "data": "0x00", "gasLimit": GAS_LIMIT});
    let (code, refused) = s.call("/v1/relay", Some(noncanonical)).await;
    assert_eq!((code, name(&refused)), (400, "NoncanonicalData"));
    assert_eq!(s.call("/health", None).await, (200, json!("OK")));
    let (_, metrics) = s.call("/metrics", None).await;
    let refusals = "libid_relayer_refusals_total{reason=\"NoncanonicalData\"} 1";
    assert!(metrics.as_str().unwrap().contains(refusals), "{metrics}");
    assert_eq!(s.gauge("paused").await, 0.0);
    assert_eq!(s.gauge("oldest_in_flight_seconds").await, 0.0);
    assert!(s.gauge("since_last_tick_seconds").await < 5.0);
    assert!(s.gauge("key_balance_wei").await > 0.0);
    assert!(s.gauge("budget_left_wei").await > 0.0);
    assert_eq!(s.call("/nowhere", None).await.0, 404);
}

/// Refusals carry their names: the relayer's own with their fields, and a revert
/// decoded on the wallet route; a wallet with a batch queued is busy.
#[tokio::test]
async fn refusals_carry_their_decoded_names() {
    let o = Onboarding::new(false).await;
    let mut underfunded = o.config();
    underfunded.min_balance = U256::MAX;
    let s = Server::start(&o, underfunded, &db("names"), Duration::from_secs(3600)).await;
    assert_eq!(
        s.call("/health", None).await.0,
        503,
        "healthy before a tick"
    );
    s.service.tick().await; // finds the balance low: a heartbeat all the same
    assert_eq!(
        s.call("/health", None).await.0,
        200,
        "unhealthy on a low balance"
    );
    let (code, low) = s
        .relay(&o.request_for(&o.batch(o.repay(o.value))).await)
        .await;
    assert_eq!((code, name(&low)), (400, "RefundTooLow"), "{low}");
    let required = low["error"]["args"]["required"].as_str().unwrap();
    assert!(required.parse::<U256>().unwrap() > o.value, "{low}");
    let unclaimed = o.request_for(&o.onboarding()).await;
    let (code, quoted) = s.call("/v1/quote", Some(wire(&unclaimed))).await;
    let (_, relayed) = s.relay(&unclaimed).await;
    assert_eq!(quoted, relayed);
    assert_eq!((code, name(&quoted)), (400, "NothingHeld"), "{quoted}");
    assert_eq!(quoted["error"]["contract"], "HandleEscrow");
    let (code, bad) = s.call("/v1/quote", Some(json!({"wallet": 1}))).await;
    assert_eq!((code, name(&bad)), (400, "BadRequest"));

    let mut calls = o.onboarding();
    calls.remove(2); // no claim: the wallet pays the refund from its own funds
    o.set_balance(o.wallet, o.refund() * U256::from(2)).await;
    let (code, first) = s.relay(&o.request_for(&calls).await).await;
    assert_eq!(code, 202, "{first}");
    calls[2] = o.repay(o.refund() + U256::ONE);
    let (code, busy) = s.relay(&o.request_for(&calls).await).await;
    assert_eq!((code, name(&busy)), (409, "WalletBusy"));
    assert_eq!(busy["error"]["args"]["id"], first["id"]);
    let (code, unknown) = s.call(&format!("/v1/relay/{}", B256::ZERO), None).await;
    assert_eq!((code, name(&unknown)), (404, "UnknownRequest"));
}

/// A wallet that lost its budget is `RateLimited` while others relay; once wallets
/// lose the global budget, each counted up to its own, requests are `Paused`.
#[tokio::test]
async fn a_wallet_budget_trips_before_the_global_pause() {
    let (mut o, path) = (Onboarding::new(true).await, db("budgets"));
    let window = Duration::from_secs(3600);
    let config = Config {
        wallet_budget: Budget {
            wei: U256::from(1),
            window,
        },
        global_budget: Budget {
            wei: U256::from(2),
            window,
        },
        ..o.config()
    };
    let s = Server::start(&o, config, &path, Duration::from_secs(3600)).await;
    let first = s.unrefunded(&o, "00").await; // STOP: mined, status 1
    let again = o
        .request_for(&o.batch(o.repay(o.refund() + U256::ONE)))
        .await;
    let (code, limited) = s.relay(&again).await;
    assert_eq!((code, name(&limited)), (429, "RateLimited"));
    o.wallet_at(U256::ONE).await;
    // Accepted, one wei of the global budget left; REVERT: mined, status 0.
    let second = s.unrefunded(&o, "60006000fd").await;
    o.wallet_at(U256::from(2)).await;
    let (code, paused) = s.relay(&o.request_for(&o.onboarding()).await).await;
    assert_eq!((code, name(&paused)), (503, "Paused"));
    let (code, limited) = s.relay(&again).await; // its wallet's budget answers first
    assert_eq!((code, name(&limited)), (429, "RateLimited"));
    assert_eq!(s.gauge("paused").await, 1.0);
    assert_eq!(s.gauge("budget_left_wei").await, 0.0);
    let stats = stats(&path).unwrap();
    let requests = [("included".to_owned(), 1), ("reverted".to_owned(), 1)];
    assert_eq!(stats.requests, BTreeMap::from(requests));
    assert_eq!(stats.profit, -first - second, "not charged its loss");
}

/// A relayer whose hex key holds a marker: no log line of this suite, at every
/// level, and no response carries it.
#[tokio::test]
async fn the_signer_key_is_in_no_log_line_or_response() {
    let logs = crate::db("logs").with_extension("log");
    let file = Mutex::new(std::fs::File::create(&logs).unwrap());
    let subscriber = tracing_subscriber::fmt().with_max_level(tracing::Level::TRACE);
    // Global: a thread's own subscriber misses events once other tests cached them.
    let subscriber = subscriber.with_writer(file).finish();
    tracing::subscriber::set_global_default(subscriber).unwrap();
    let mut o = Onboarding::new(true).await;
    let key: PrivateKeySigner = format!("0x{}0123", MARKER.repeat(5)).parse().unwrap();
    let url = o._anvil.endpoint_url();
    o.relayer = ProviderBuilder::new()
        .wallet(key.clone())
        .connect_http(url)
        .erased();
    o.relayer_address = key.address();
    o.set_balance(key.address(), U256::from(CLAIMED)).await;
    let s =
        Server::start(&o, o.config(), &db("marker"), Duration::from_millis(100)).await;

    assert_eq!(
        s.call("/v1/config", None).await.1["relayer"],
        json!(key.address())
    );
    let request = o.request_for(&o.onboarding()).await;
    s.call("/v1/quote", Some(wire(&request))).await;
    let (_, accepted) = s.relay(&request).await;
    s.settle(&o, &accepted["id"], "included").await;
    s.relay(&o.request_for(&o.batch(o.repay(o.value))).await)
        .await;
    for path in ["/health", "/metrics", "/nowhere"] {
        s.call(path, None).await;
    }
    let logs = std::fs::read_to_string(logs).unwrap();
    assert!(logs.contains("relayer listening on"), "no log captured");
    let seen = s.seen.lock().unwrap().clone();
    assert!(seen.contains("RefundTooLow"), "no refusal answered");
    for (what, text) in [("a log line", logs), ("a response", seen)] {
        assert!(!text.to_lowercase().contains(MARKER), "the key in {what}");
    }
}
