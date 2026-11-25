//! The relayer's TOML file: one chain's [`Config`], its node and key, its state and how
//! it serves. `RELAYER_RPC_URL`, `RELAYER_SIGNER` and `RELAYER_PORT` override the
//! file; relative paths are read from the file's directory.

use std::{
    path::{
        Path,
        PathBuf,
    },
    time::Duration,
};

use alloy::primitives::{
    Address,
    U256,
};
use serde::Deserialize;

use crate::{
    config::{
        Budget,
        Config,
        FeeParams,
        L1Fee,
        LOSS_HOLD,
        TICK_TIMEOUT,
    },
    http::Limits,
    relay::Error,
};

/// A loaded file, with the environment's overrides. Holds the signer spec, so it has
/// no `Debug`.
pub struct Settings {
    /// The chain configuration.
    pub config: Config,
    /// The node's JSON-RPC endpoint.
    pub rpc_url: Option<String>,
    /// The key, for `libid-signer`: a hex key or an AWS KMS key id.
    pub signer: Option<String>,
    /// The SQLite ledger.
    pub db: PathBuf,
    /// The API's port, on every interface.
    pub port: u16,
    /// The port of `/metrics`, on every interface.
    pub metrics_port: u16,
    /// What bounds the API's requests.
    pub limits: Limits,
    /// How often the relayer ticks.
    pub poll_interval: Duration,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    chain: Chain,
    contracts: Contracts,
    key: Key,
    fees: Fees,
    state: State,
    http: Http,
    limits: Limit,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Chain {
    rpc_url: Option<String>,
    chain_id: u64,
    network_file: Option<PathBuf>,
    #[serde(with = "humantime_serde")]
    block_time: Duration,
    #[serde(with = "humantime_serde")]
    poll_interval: Duration,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Contracts {
    factory: Address,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Key {
    signer: Option<String>,
    address: Address,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fees {
    #[serde(with = "humantime_serde")]
    inclusion_window: Duration,
    #[serde(with = "humantime_serde")]
    quote_validity: Duration,
    tip_percentile: f64,
    l1_fee: L1Fee,
    l1_margin_percent: u64,
    max_fee_percent: u64,
    max_cancel_fee_percent: u64,
    fee_params: FeeParams,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    db: PathBuf,
    #[serde(default = "tick_timeout", with = "humantime_serde")]
    tick_timeout: Duration,
}

fn tick_timeout() -> Duration {
    TICK_TIMEOUT
}

fn loss_hold() -> Duration {
    LOSS_HOLD
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Http {
    port: u16,
    metrics_port: u16,
    #[serde(with = "humantime_serde")]
    request_timeout: Duration,
    concurrency_limit: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Limit {
    global_budget: Budget,
    #[serde(default = "loss_hold", with = "humantime_serde")]
    loss_hold: Duration,
    max_gas_limit: u64,
    #[serde(default)]
    max_value: Option<U256>,
    supported_releases: Vec<u64>,
    node_error_ticks: u32,
    receipt_lag_blocks: u64,
    min_balance: U256,
}

/// The fields of a chain-configurations `networks/*.toml` the relayer reads.
#[derive(Deserialize)]
struct Network {
    network: NetworkId,
}

#[derive(Deserialize)]
struct NetworkId {
    chain_id: u64,
}

impl Settings {
    /// The file at `path`, with the overrides `env` names. A file that cannot be read
    /// is its `std::io::Error`.
    pub fn load(
        path: &Path,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, Error> {
        let base = path.parent().unwrap_or(Path::new("."));
        let File {
            chain,
            contracts,
            key,
            fees,
            state,
            http,
            limits,
        } = parse(path)?;
        if let Some(file) = &chain.network_file {
            let named = |e| format!("network_file {}: {e}", file.display());
            let Network { network } = parse(&base.join(file)).map_err(named)?;
            let (theirs, ours) = (network.chain_id, chain.chain_id);
            if theirs != ours {
                let file = file.display();
                let both =
                    format!("{file} has chain_id {theirs}, [chain] chain_id {ours}");
                return Err(format!("{both}: name the same chain").into());
            }
        }
        let (port, named) = match env("RELAYER_PORT") {
            Some(port) => (
                port.parse().map_err(|e| format!("RELAYER_PORT: {e}"))?,
                "RELAYER_PORT",
            ),
            None => (http.port, "[http] port"),
        };
        if port != 0 && port == http.metrics_port {
            let same = format!("{named} {port} is [http] metrics_port");
            return Err(format!("{same}: give each its own port").into());
        }
        let zero = [
            (chain.poll_interval.is_zero(), "[chain] poll_interval"),
            (http.request_timeout.is_zero(), "[http] request_timeout"),
            (state.tick_timeout.is_zero(), "[state] tick_timeout"),
            (http.concurrency_limit == 0, "[http] concurrency_limit"),
        ];
        if let Some((_, name)) = zero.iter().find(|(zero, _)| *zero) {
            return Err(format!("{name} is 0: set it above 0").into());
        }
        let signer = env("RELAYER_SIGNER");
        if signer.as_deref() == Some("") {
            return Err(
                "RELAYER_SIGNER is empty: set it, or unset it for [key] signer".into(),
            );
        }
        let config = Config {
            chain_id: chain.chain_id,
            factory: contracts.factory,
            supported_releases: limits.supported_releases,
            max_gas_limit: limits.max_gas_limit,
            max_value: limits.max_value,
            relayer: key.address,
            l1_fee: fees.l1_fee,
            l1_margin: fees.l1_margin_percent,
            inclusion_window: fees.inclusion_window,
            quote_validity: fees.quote_validity,
            block_time: chain.block_time,
            fee_params: fees.fee_params,
            tip_percentile: fees.tip_percentile,
            max_fee_percent: fees.max_fee_percent,
            max_cancel_fee_percent: fees.max_cancel_fee_percent,
            min_balance: limits.min_balance,
            node_error_ticks: limits.node_error_ticks,
            receipt_lag_blocks: limits.receipt_lag_blocks,
            global_budget: limits.global_budget,
            loss_hold: limits.loss_hold,
            tick_timeout: state.tick_timeout,
        };
        config.validate()?;
        Ok(Self {
            config,
            rpc_url: env("RELAYER_RPC_URL").or(chain.rpc_url),
            signer: signer.or(key.signer),
            db: base.join(state.db),
            port,
            metrics_port: http.metrics_port,
            limits: Limits {
                timeout: http.request_timeout,
                concurrency: http.concurrency_limit,
            },
            poll_interval: chain.poll_interval,
        })
    }
}

/// The TOML file at `path`; an error is placed by line and column without the source
/// line toml quotes, which may hold a key, and a file that cannot be read is its
/// `std::io::Error`.
fn parse<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, Error> {
    let text = std::fs::read_to_string(path)?;
    toml::from_str(&text).map_err(|e| {
        let at = e.span().map_or(String::new(), |span| {
            let before = &text[..span.start];
            let column = before
                .rsplit('\n')
                .next()
                .unwrap_or_default()
                .chars()
                .count();
            let line = before.matches('\n').count() + 1;
            format!("line {line}, column {}: ", column + 1)
        });
        format!("{at}{}", e.message()).into()
    })
}
