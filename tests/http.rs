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
        Limits,
        Service,
    },
    stats::stats,
};
use serde_json::{
    json,
    Value,
};

/// A server on random ports, and every body it answered.
struct Server {
    url: String,
    metrics: String,
    service: Arc<Service<DynProvider>>,
    seen: Mutex<String>,
}

impl Server {
    /// `o`'s relayer under `config`, ticking every `every`.
    async fn start(o: &Onboarding, config: Config, db: &Path, every: Duration) -> Self {
        let relayer = Relayer::start(o.relayer.clone(), config, db).await.unwrap();
        let service = Service::new(relayer, Duration::from_secs(5));
        let bind = || tokio::net::TcpListener::bind("127.0.0.1:0");
        let (api, metrics) = (bind().await.unwrap(), bind().await.unwrap());
        let at =
            |l: &tokio::net::TcpListener| format!("http://{}", l.local_addr().unwrap());
        let (url, metrics_url) = (at(&api), at(&metrics));
        let limits = Limits {
            timeout: Duration::from_secs(30),
            concurrency: 8,
        };
        let listeners = (api, metrics);
        let serving = serve(
            service.clone(),
            every,
            limits,
            listeners,
            std::future::pending(),
        );
        tokio::spawn(serving);
        let seen = Mutex::default();
        Self {
            url,
            metrics: metrics_url,
            service,
            seen,
        }
    }

    /// `path`'s status and body, JSON or text: a POST of `body` when given, else a GET;
    /// `/metrics` on its own listener.
    async fn call(&self, path: &str, body: Option<Value>) -> (u16, Value) {
        let base = if path == "/metrics" {
            &self.metrics
        } else {
            &self.url
        };
        let (http, url) = (reqwest::Client::new(), format!("{base}{path}"));
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
    let (code, again) = s.relay(&request).await;
    assert_eq!(
        (code, again),
        (202, json!({"id": accepted["id"], "status": "included"}))
    );
    o.assert_onboarded(o.refund()).await;

    let (cost, net) = (cost(&o, &done).await, o.refund() - o.value);
    let stats = stats(&path).unwrap();
    assert_eq!(stats.requests, BTreeMap::from([("included".to_owned(), 1)]));
    let profit = I256::try_from(net).unwrap() - I256::try_from(cost).unwrap();
    assert_eq!((stats.profit, stats.profit_per_request), (profit, profit));
    assert_eq!(stats.cancel_rate, 0.0);
    assert_eq!(stats.cost_ratio, Some(f64::from(cost) / f64::from(net)));
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
    let main_port = reqwest::get(format!("{}/metrics", s.url)).await.unwrap();
    assert_eq!(main_port.status(), 404, "/metrics on the API's port");
}

/// Refusals carry their names: the relayer's own with their fields, and a revert
/// decoded on the wallet route; a wallet with a batch queued is busy.
#[tokio::test]
async fn refusals_carry_their_decoded_names() {
    let o = Onboarding::new(false).await;
    let mut underfunded = o.config();
    (underfunded.min_balance, underfunded.max_gas_limit) = (U256::MAX, GAS_LIMIT);
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
    let (_, config) = s.call("/v1/config", None).await;
    let max = config["maxGasLimit"].as_u64().unwrap();
    assert_eq!(config["registry"], json!(o.stack.wallet_registry));
    let mut over = wire(&unclaimed);
    over["gasLimit"] = (max + 1).into();
    let (code, high) = s.call("/v1/relay", Some(over)).await;
    assert_eq!(
        (code, name(&high), &high["error"]["args"]["max"]),
        (400, "GasLimitTooHigh", &config["maxGasLimit"])
    );

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
/// lose the global budget, their whole losses counted, requests are `Paused`.
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
            wei: o.value, // over a reverted send's gas, under a mined one's loss
            window,
        },
        ..o.config()
    };
    let s = Server::start(&o, config, &path, Duration::from_secs(3600)).await;
    let first = s.unrefunded(&o, "60006000fd").await; // REVERT: mined, status 0
    let again = o
        .request_for(&o.batch(o.repay(o.refund() + U256::ONE)))
        .await;
    let (code, limited) = s.relay(&again).await;
    assert_eq!((code, name(&limited)), (429, "RateLimited"));
    o.wallet_at(U256::ONE).await;
    // Accepted within the global budget; STOP: mined, status 1, its value lost too.
    let second = s.unrefunded(&o, "00").await;
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
