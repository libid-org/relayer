//! The HTTP API over a [`Relayer`], and the ticks that drive it:
//!
//! - `GET  /v1/config`
//! - `POST /v1/quote`: a [`Draft`]
//! - `POST /v1/relay`: a [`Request`]; `202 {id, status}`
//! - `GET  /v1/relay/{id}`
//! - `GET  /health`: `OK` while the last tick succeeded recently, else `503`
//! - `GET  /metrics`
//!
//! A refusal is `{"error": {name, contract?, args?, message}}`; wei amounts are
//! decimal strings.

use std::{
    future::Future,
    sync::{
        atomic::{
            AtomicU64,
            Ordering::Relaxed,
        },
        Arc,
    },
    time::Duration,
};

use alloy::{
    primitives::B256,
    providers::Provider,
};
use axum::{
    body::Bytes,
    extract::{
        Path,
        State,
    },
    http::{
        header,
        HeaderValue,
        StatusCode as C,
    },
    response::{
        IntoResponse,
        Response,
    },
    routing::{
        get,
        post,
    },
    Json,
    Router,
};
use prometheus_client::{
    metrics::{
        counter::Counter,
        family::Family,
        gauge::Gauge,
    },
    registry::Registry,
};
use serde_json::{
    json,
    Value,
};
use tokio::time::Instant;

use crate::{
    check::{
        Refusal,
        Request,
    },
    quote::Draft,
    relay::{
        Relayer,
        Status,
        Stop,
    },
};

pub const CONFIG_PATH: &str = "/v1/config";
pub const QUOTE_PATH: &str = "/v1/quote";
pub const RELAY_PATH: &str = "/v1/relay";
pub const STATUS_PATH: &str = "/v1/relay/{id}";
pub const HEALTH_PATH: &str = "/health";
pub const METRICS_PATH: &str = "/metrics";

/// A relayer served over HTTP, the heartbeat of its ticks, and its refusals counted.
pub struct Service<P> {
    relayer: Relayer<P>,
    /// How old the last successful tick may be while `/health` answers `OK`.
    stale_after: Duration,
    started: Instant,
    /// When the last tick succeeded, in milliseconds after `started` plus one; 0 before.
    last_tick: AtomicU64,
    refusals: Family<[(&'static str, &'static str); 1], Counter>,
}

impl<P: Provider + 'static> Service<P> {
    pub fn new(relayer: Relayer<P>, stale_after: Duration) -> Arc<Self> {
        let (started, last_tick, refusals) =
            (Instant::now(), 0.into(), Family::default());
        Arc::new(Self {
            relayer,
            stale_after,
            started,
            last_tick,
            refusals,
        })
    }

    /// One tick, logged when it fails. A tick that ran, one that found the balance
    /// low included, is the heartbeat.
    pub async fn tick(&self) {
        match self.relayer.tick().await {
            Err(e) if matches!(e.downcast_ref(), Some(Stop::LowBalance(_))) => {
                tracing::warn!("{e}")
            }
            Err(e) => return tracing::error!("tick failed: {e}"),
            Ok(()) => {}
        }
        let ms = self.started.elapsed().as_millis() as u64;
        self.last_tick.store(ms + 1, Relaxed);
    }

    /// Whether a tick succeeded, and how long ago, or since start.
    fn since_tick(&self) -> (bool, Duration) {
        let last = Duration::from_millis(self.last_tick.load(Relaxed).saturating_sub(1));
        (last > Duration::ZERO, self.started.elapsed() - last)
    }

    fn refused(&self, refusal: Refusal) -> Response {
        let reason = [("reason", refusal.name())];
        self.refusals.get_or_create(&reason).inc();
        if let Refusal::Node(e) = &refusal {
            tracing::error!("node: {e}");
        }
        let (status, body) = described(&refusal);
        (status, Json(json!({ "error": body }))).into_response()
    }
}

/// Serve `service` on `listener`, ticking every `every` from one `every` on, until
/// `shutdown` resolves; requests in flight finish first.
pub async fn serve<P: Provider + 'static>(
    service: Arc<Service<P>>,
    every: Duration,
    listener: tokio::net::TcpListener,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let ticker = service.clone();
    let ticking = tokio::spawn(async move {
        let mut interval = tokio::time::interval_at(Instant::now() + every, every);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            ticker.tick().await;
        }
    });
    tracing::info!("relayer listening on {}", listener.local_addr()?);
    let app = Router::new()
        .route(CONFIG_PATH, get(config))
        .route(QUOTE_PATH, post(quote))
        .route(RELAY_PATH, post(relay))
        .route(STATUS_PATH, get(status))
        .route(HEALTH_PATH, get(health))
        .route(METRICS_PATH, get(metrics))
        .fallback(|| async { C::NOT_FOUND })
        .layer(axum::middleware::map_response(standing_headers))
        .with_state(service);
    let served = axum::serve(listener, app).with_graceful_shutdown(shutdown);
    let served = served.await;
    ticking.abort();
    served
}

type S<P> = State<Arc<Service<P>>>;

async fn config<P: Provider + 'static>(State(s): S<P>) -> Response {
    let c = s.relayer.config();
    Json(json!({
        "chainId": c.chain_id,
        "relayer": c.relayer,
        "factory": c.factory,
        "forwarder": c.forwarder,
        "supportedReleases": c.supported_releases,
        "l1MarginPercent": c.l1_margin,
        "inclusionWindowSeconds": c.inclusion_window.as_secs(),
        "quoteValiditySeconds": c.quote_validity.as_secs(),
    }))
    .into_response()
}

type Answer = Result<Response, Response>;

async fn quote<P: Provider + 'static>(State(s): S<P>, body: Bytes) -> Answer {
    let r = &s.relayer;
    let q = parse::<Draft>(&body)?.quote(&r.provider, r.config()).await;
    let q = q.map_err(|refusal| s.refused(refusal))?;
    Ok(Json(json!({
        "relayer": r.config().relayer,
        "callGas": q.call_gas,
        "gasLimit": q.gas_limit,
        "maxFeePerGas": q.max_fee_per_gas.to_string(),
        "l1FeeBound": q.l1_fee_bound.to_string(),
        "minRefund": q.min_refund.to_string(),
        "validUntil": q.valid_until,
    }))
    .into_response())
}

async fn relay<P: Provider + 'static>(State(s): S<P>, body: Bytes) -> Answer {
    let id = s.relayer.relay(&parse::<Request>(&body)?).await;
    let id = id.map_err(|refusal| s.refused(refusal))?;
    let body = json!({"id": id, "status": "queued"});
    Ok((C::ACCEPTED, Json(body)).into_response())
}

async fn status<P: Provider + 'static>(State(s): S<P>, Path(id): Path<String>) -> Answer {
    let id = id.parse::<B256>();
    let id = id.map_err(|_| error(C::BAD_REQUEST, "BadRequest", "an id is 32 bytes"))?;
    let report = s
        .relayer
        .status(id)
        .await
        .map_err(|e| match e.downcast_ref() {
            Some(Stop::Unknown) => {
                error(C::NOT_FOUND, "UnknownRequest", "no such request")
            }
            _ => {
                tracing::error!("status of {id}: {e}");
                error(C::SERVICE_UNAVAILABLE, "Ledger", "the ledger failed")
            }
        })?;
    let mut body = json!({ "txHashes": report.tx_hashes });
    body["status"] = match report.status {
        Status::Queued => "queued".into(),
        Status::Sent => "sent".into(),
        Status::Included { block, reverted } => {
            (body["blockNumber"], body["reverted"]) = (block.into(), reverted.into());
            "included".into()
        }
        Status::Cancelled => "cancelled".into(),
        Status::Failed(refusal) => {
            body["error"] = described(&refusal).1;
            "failed".into()
        }
    };
    Ok(Json(body).into_response())
}

async fn health<P: Provider + 'static>(State(s): S<P>) -> Response {
    match s.since_tick() {
        (true, age) if age <= s.stale_after => "OK".into_response(),
        _ => (C::SERVICE_UNAVAILABLE, "no recent successful tick").into_response(),
    }
}

/// The refusals counted, and gauges read at the scrape.
async fn metrics<P: Provider + 'static>(State(s): S<P>) -> Response {
    let r = &s.relayer;
    let mut registry = Registry::with_prefix("libid_relayer");
    registry.register("refusals", "Requests refused, by why", s.refusals.clone());
    let mut gauge = |name: &str, help: &str, value: Option<f64>| {
        let gauge = Gauge::<f64, AtomicU64>::default();
        gauge.set(value.unwrap_or(f64::NAN));
        registry.register(name, help, gauge);
    };
    let paused = Some(f64::from(u8::from(r.paused().await)));
    gauge("paused", "Whether sending paused", paused);
    let balance = r.provider.get_balance(r.config().relayer).await;
    gauge(
        "key_balance_wei",
        "The key's balance",
        balance.ok().map(f64::from),
    );
    let oldest = r.oldest_in_flight().ok().map(|age| age.unwrap_or(0) as f64);
    gauge(
        "oldest_in_flight_seconds",
        "The oldest request's wait",
        oldest,
    );
    let since = Some(s.since_tick().1.as_secs() as f64);
    gauge("since_last_tick_seconds", "Since the last good tick", since);
    let left = r.budget_left().ok().map(f64::from);
    gauge("budget_left_wei", "The global loss budget left", left);
    let mut text = String::new();
    prometheus_client::encoding::text::encode(&mut text, &registry).expect("a String");
    let openmetrics = "application/openmetrics-text; version=1.0.0; charset=utf-8";
    ([(header::CONTENT_TYPE, openmetrics)], text).into_response()
}

async fn standing_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    let nosniff = HeaderValue::from_static("nosniff");
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, nosniff);
    response
}

#[allow(clippy::result_large_err)] // a response, answered at once
fn parse<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, Response> {
    serde_json::from_slice(body)
        .map_err(|e| error(C::BAD_REQUEST, "BadRequest", &e.to_string()))
}

fn error(status: C, name: &str, message: &str) -> Response {
    let body = json!({"error": {"name": name, "message": message}});
    (status, Json(body)).into_response()
}

/// A refusal's status code and body: its name, or a revert's decoded name and the
/// contracts that declare it; its fields; what happened.
fn described(refusal: &Refusal) -> (C, Value) {
    use Refusal::*;
    let (status, message) = match refusal {
        NoncanonicalData => (C::BAD_REQUEST, "data does not re-encode to itself"),
        RefundMissing => (C::BAD_REQUEST, "no top-level Execution(relayer, R, \"\")"),
        RefundTooLow { .. } => (C::BAD_REQUEST, "sign a refund of at least `required`"),
        GasLimitTooLow { .. } => (C::BAD_REQUEST, "quote again for `required` gas"),
        UnknownWallet => (C::BAD_REQUEST, "the contracts do not vouch for the wallet"),
        ValueNotAllowed => (C::BAD_REQUEST, "an EIP-7702 account's batch carries value"),
        UnsupportedRelease { .. } => {
            (C::BAD_REQUEST, "the wallet's release is unsupported")
        }
        Expired => (
            C::BAD_REQUEST,
            "the deadline ends within the inclusion window",
        ),
        Reverted { .. } => (C::BAD_REQUEST, "the batch reverts"),
        WalletBusy { .. } => (C::CONFLICT, "a batch of the wallet is in flight"),
        RateLimited => (C::TOO_MANY_REQUESTS, "the wallet's loss budget is spent"),
        RelayerUnderfunded { .. } | Paused => (C::SERVICE_UNAVAILABLE, "sending paused"),
        Node(_) => (C::SERVICE_UNAVAILABLE, "the node could not answer"),
    };
    let mut body = json!({"name": refusal.name(), "message": message});
    let wei = |w: &alloy::primitives::U256| w.to_string();
    body["args"] = match refusal {
        RefundTooLow { refund, required } => {
            json!({"refund": wei(refund), "required": wei(required)})
        }
        GasLimitTooLow { required } => json!({ "required": required }),
        UnsupportedRelease { version } => json!({ "version": version }),
        WalletBusy { id } => json!({ "id": id }),
        Reverted { data, .. } => json!({ "data": data }),
        _ => return (status, body),
    };
    if let Reverted { error: Some(e), .. } = refusal {
        body["name"] = e.name().into();
        body["contract"] = e.contracts().join("|").into();
    }
    (status, body)
}
