//! The HTTP API on anvil, served in-process on a random port: quote, relay, status,
//! refusals by name, the loss hold and budget, health, metrics and the ledger's stats. Needs
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
        let limits = Limits {
            timeout: Duration::from_secs(30),
            concurrency: 8,
        };
        Self::within(o, config, db, every, limits).await
    }

    /// [`Self::start`], its API within `limits`.
    async fn within(
        o: &Onboarding,
        config: Config,
        db: &Path,
        every: Duration,
        limits: Limits,
    ) -> Self {
        let relayer = Relayer::start(o.relayer.clone(), config, db).await.unwrap();
        let service = Service::new(relayer, Duration::from_secs(5));
        let bind = || tokio::net::TcpListener::bind("127.0.0.1:0");
        let (api, metrics) = (bind().await.unwrap(), bind().await.unwrap());
        let at =
            |l: &tokio::net::TcpListener| format!("http://{}", l.local_addr().unwrap());
        let (url, metrics_url) = (at(&api), at(&metrics));
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
    async fn unrefunded(&self, o: &Onboarding, planted: &str) -> U256 {
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
        cost(o, &done).await + value
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
    let mut low = wire(&request);
    low["gasLimit"] = 50_000.into();
    let (code, low) = s.call("/v1/relay", Some(low)).await;
    assert_eq!((code, name(&low)), (400, "GasLimitTooLow"), "{low}");
    assert!(low["error"]["args"]["required"].as_u64().unwrap() > 50_000);
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

    let stats = stats(&path, &o.config()).unwrap();
    let shown = "requests      included 1\n\
                 held          0\n\
                 lost          0 wei\n\
                 pending       0\n";
    assert_eq!(stats.to_string(), shown);
    let json = json!({"requests": {"included": 1}, "held": 0, "lost": "0", "pending": 0});
    assert_eq!(stats.json(), json);

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
/// decoded on the wallet route; an identity with a batch queued is busy; a low balance
/// is a heartbeat and pauses relays.
#[tokio::test]
async fn refusals_carry_their_decoded_names() {
    let o = Onboarding::new(false).await;
    let mut underfunded = o.config();
    (underfunded.min_balance, underfunded.max_gas_limit) = (U256::MAX, GAS_LIMIT);
    let hour = Duration::from_secs(3600);
    let low = Server::start(&o, underfunded.clone(), &db("low"), hour).await;
    tokio::time::sleep(Duration::from_millis(600)).await; // no tick before an hour
    assert_eq!(
        low.call("/health", None).await.0,
        503,
        "healthy before a tick"
    );
    low.service.tick().await; // finds the balance low: a heartbeat all the same
    assert_eq!(
        low.call("/health", None).await.0,
        200,
        "unhealthy on a low balance"
    );
    assert_eq!(
        low.gauge("since_last_tick_seconds").await,
        0.0,
        "since start"
    );
    let (code, paused) = low.relay(&o.request_for(&o.onboarding()).await).await;
    assert_eq!((code, name(&paused)), (503, "Paused"), "{paused}");
    // Unticked: the balance not read low.
    let s = Server::start(&o, underfunded, &db("names"), hour).await;
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
    let limits = Limits {
        timeout: Duration::from_nanos(1),
        concurrency: 1,
    };
    let every = Duration::from_secs(3600);
    let slow = Server::within(&o, o.config(), &db("slow"), every, limits).await;
    let timed_out = slow.call("/v1/quote", Some(wire(&unclaimed))).await.0;
    let health = slow.call("/health", None).await.0; // outside the limits
    assert_eq!((timed_out, health), (408, 503));
    let unsupported = Config {
        supported_releases: vec![2],
        ..o.config()
    };
    let capped = |max: U256| Config {
        max_value: Some(max),
        ..o.config()
    };
    let at = Server::start(&o, capped(o.value), &db("at-max"), every).await;
    let (_, quoted) = at.call("/v1/quote", Some(wire(&unclaimed))).await;
    assert_eq!(name(&quoted), "NothingHeld", "{quoted}");
    let max = o.value - U256::ONE;
    let under = Server::start(&o, capped(max), &db("under-max"), every).await;
    let (_, config) = under.call("/v1/config", None).await;
    assert_eq!(config["maxValue"], max.to_string());
    let (code, quoted) = under.call("/v1/quote", Some(wire(&unclaimed))).await;
    let (_, relayed) = under.relay(&unclaimed).await;
    assert_eq!(quoted, relayed);
    assert_eq!((code, name(&quoted)), (400, "ValueTooHigh"), "{quoted}");
    assert_eq!(quoted["error"]["args"]["max"], max.to_string());
    let newer = Server::start(&o, unsupported, &db("newer"), every).await;
    let (code, release) = newer.relay(&unclaimed).await;
    assert_eq!((code, name(&release)), (400, "UnsupportedRelease"));
    assert_eq!(release["error"]["args"]["version"], 1);
    let (code, bad) = s.call("/v1/quote", Some(json!({"wallet": 1}))).await;
    assert_eq!((code, name(&bad)), (400, "BadRequest"));
    let (_, config) = s.call("/v1/config", None).await;
    let max = config["maxGasLimit"].as_u64().unwrap();
    assert_eq!(config["maxTransactionBytes"], 131_072);
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
    assert_eq!((code, name(&busy)), (409, "IdentityBusy"));
    assert_eq!(busy["error"]["args"]["id"], first["id"]);
    let (code, unknown) = s.call(&format!("/v1/relay/{}", B256::ZERO), None).await;
    assert_eq!((code, name(&unknown)), (404, "UnknownRequest"));
}

/// With every permit held by a half-sent body, config and status still answer; at
/// shutdown, that request and one with unfinished headers hold the drain no longer
/// than `request_timeout`.
#[tokio::test]
async fn a_half_sent_request_holds_neither_status_nor_shutdown() {
    use tokio::io::AsyncWriteExt;
    let o = Onboarding::new(false).await;
    let relayer = Relayer::start(o.relayer.clone(), o.config(), &db("half"))
        .await
        .unwrap();
    let service = Service::new(relayer, Duration::from_secs(5));
    let bind = || tokio::net::TcpListener::bind("127.0.0.1:0");
    let (api, metrics) = (bind().await.unwrap(), bind().await.unwrap());
    let at = api.local_addr().unwrap();
    let limits = Limits {
        timeout: Duration::from_secs(1),
        concurrency: 1,
    };
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let shutdown = async move { drop(stopped.await) };
    let hour = Duration::from_secs(3600);
    let served = tokio::spawn(serve(service, hour, limits, (api, metrics), shutdown));
    let mut half = tokio::net::TcpStream::connect(at).await.unwrap();
    let head = "POST /v1/relay HTTP/1.1\r\nhost: x\r\ncontent-length: 100\r\n\r\n{";
    half.write_all(head.as_bytes()).await.unwrap();
    // Its headers unfinished: no handler runs, so no request timeout ends it.
    let mut headless = tokio::net::TcpStream::connect(at).await.unwrap();
    headless.write_all(b"POST /v1/relay HTTP/1.1\r\nhost: x\r\n").await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let url = format!("http://{at}");
    let config = reqwest::get(format!("{url}/v1/config")).await.unwrap();
    assert_eq!(config.status(), 200);
    let status = reqwest::get(format!("{url}/v1/relay/{}", B256::ZERO)).await;
    assert_eq!(status.unwrap().status(), 404);
    stop.send(()).unwrap();
    let drained = tokio::time::timeout(Duration::from_secs(5), served).await;
    drained
        .expect("held past request_timeout")
        .unwrap()
        .unwrap();
    drop((half, headless));
}

/// An identity whose send lost the relayer gas is `IdentityHeld` until `loss_hold`
/// has passed, while others relay; once identities' losses, counted whole, leave less of
/// the global budget than a request's worst loss, that request is `OverBudget`.
#[tokio::test]
async fn a_loss_holds_its_identity_and_the_global_budget_bounds_the_next() {
    let (mut o, path) = (Onboarding::new(true).await, db("budgets"));
    // A request's worst loss is its value and its gas at the head's cap, far under R.
    // Over one worst loss and a reverted send's gas, under two with a mined send's loss.
    let budget = o.value + o.value / U256::from(2);
    let config = Config {
        global_budget: Budget {
            wei: budget,
            window: Duration::from_secs(3600),
        },
        ..o.config()
    };
    let s = Server::start(&o, config.clone(), &path, Duration::from_secs(3600)).await;
    let first = s.unrefunded(&o, "60006000fd").await; // REVERT: mined, status 0
    o.set_code(o.wallet, "").await; // its batch reverted: the wallet was not created
    let again = o
        .request_for(&o.batch(o.repay(o.refund() + U256::ONE)))
        .await;
    let (code, held) = s.relay(&again).await;
    assert_eq!((code, name(&held)), (429, "IdentityHeld"), "{held}");
    assert!(
        held["error"]["args"]["until"].as_u64().unwrap() > 0,
        "{held}"
    );
    o.other_identity(1).await;
    // Accepted within the global budget; STOP: mined, status 1, its value lost too.
    let second = s.unrefunded(&o, "00").await;
    o.other_identity(2).await;
    let (code, over) = s.relay(&o.request_for(&o.onboarding()).await).await;
    assert_eq!((code, name(&over)), (503, "OverBudget"), "{over}");
    let left = budget - first - second;
    assert_eq!(over["error"]["args"]["left"], left.to_string());
    let worst = over["error"]["args"]["worst"].as_str().unwrap();
    let worst = worst.parse::<U256>().unwrap();
    assert!(worst > left && worst > o.value, "{over}");
    assert_eq!(s.gauge("paused").await, 0.0);
    assert_eq!(s.gauge("budget_left_wei").await, f64::from(left));
    let stats = stats(&path, &config).unwrap();
    let requests = [("included".to_owned(), 1), ("reverted".to_owned(), 1)];
    assert_eq!(stats.requests, BTreeMap::from(requests));
    assert_eq!(
        (stats.held, stats.lost),
        (2, first + second),
        "not charged its loss"
    );
    // Past the budget's window both still hold their identities; past the hold, the
    // rows go: nothing is held or counted.
    let ledger = rusqlite::Connection::open(&path).unwrap();
    let back = |secs: u64| {
        let sql = format!("UPDATE identities SET at = at - {secs}");
        ledger.execute(&sql, []).unwrap();
    };
    let (hold, window) = (
        config.loss_hold.as_secs(),
        config.global_budget.window.as_secs(),
    );
    back(window + 1);
    s.service.tick().await;
    let held = relayer::stats::stats(&path, &config).unwrap().held;
    assert_eq!(held, 2, "held only for the budget's window");
    back(hold - window - 1);
    s.service.tick().await;
    let after = relayer::stats::stats(&path, &config).unwrap();
    assert_eq!((after.held, after.lost), (0, U256::ZERO), "{after:?}");
}

/// A browser's preflight of a relay is answered for any origin.
#[tokio::test]
async fn a_browser_preflight_is_allowed() {
    let (o, path) = (Onboarding::new(true).await, db("cors"));
    let s = Server::start(&o, o.config(), &path, Duration::from_secs(3600)).await;
    let preflight = reqwest::Client::new()
        .request(reqwest::Method::OPTIONS, format!("{}/v1/relay", s.url))
        .header("origin", "https://claim.example")
        .header("access-control-request-method", "POST")
        .header("access-control-request-headers", "content-type");
    let response = preflight.send().await.unwrap();
    assert_eq!(response.status(), 200);
    let headers = response.headers();
    assert_eq!(headers["access-control-allow-origin"], "*");
    assert!(headers["access-control-allow-methods"]
        .to_str()
        .unwrap()
        .contains("POST"));
}
