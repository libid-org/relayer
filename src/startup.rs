//! `relayer run`: the signer, the chain checked against the configuration, then the
//! relayer served. Each check fails naming its fix.

use std::{
    future::Future,
    net::Ipv4Addr,
    time::Duration,
};

use alloy::{
    eips::{
        BlockId,
        BlockNumberOrTag,
    },
    primitives::{
        Address,
        U256,
    },
    providers::{
        Provider,
        ProviderBuilder,
    },
    transports::http::reqwest,
};
use libid_contracts::bindings::wallet::{
    IWebWallet,
    WalletFactory,
};
use libid_signer::SignerSource;
use tokio::net::TcpListener;

use crate::{
    config::{
        Config,
        FeeParams,
        L1Fee,
    },
    gas,
    http::{
        serve,
        Service,
    },
    relay::{
        Error,
        Relayer,
    },
    settings::Settings,
};

/// How far back the chain's block time is measured.
const BLOCK_TIME_SPAN: Duration = Duration::from_secs(10);

/// Build the signer, check the chain, take the ledger's lock and check its nonces,
/// then serve until `shutdown` resolves.
pub async fn run(
    settings: Settings,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), Error> {
    let Settings {
        config,
        rpc_url,
        signer,
        db,
        port,
        metrics_port,
        limits,
        poll_interval,
    } = settings;
    let rpc_url = rpc_url.ok_or("no node: set [chain] rpc_url or RELAYER_RPC_URL")?;
    let spec = signer.ok_or("no key: set RELAYER_SIGNER, or [key] signer")?;
    // Its error may quote the spec.
    let source = SignerSource::from_spec(&spec).map_err(|_| {
        "the signer is neither a 64-hex-digit key nor an AWS KMS key id: fix RELAYER_SIGNER"
    })?;
    tracing::info!("signing with {}", source.describe());
    let signer = source.build_managed(Some(config.chain_id)).await?;
    let http = reqwest::Client::builder()
        .timeout(config.tick_timeout)
        .build()?;
    let provider = ProviderBuilder::new()
        .wallet(signer.wallet())
        .connect_reqwest(http, rpc_url.parse()?);
    for warning in check(&provider, &config, signer.address()).await? {
        tracing::warn!("{warning}");
    }
    let stale_after = stale_after(config.tick_timeout, poll_interval);
    let relayer = Relayer::start(provider, config, &db).await?;
    let api = TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).await?;
    let metrics = TcpListener::bind((Ipv4Addr::UNSPECIFIED, metrics_port)).await?;
    let service = Service::new(relayer, stale_after);
    Ok(serve(service, poll_interval, limits, (api, metrics), shutdown).await?)
}

/// The chain `provider` reads is the one `config` names, for the key at `signer`,
/// refusing what waiting does not cure: the chain id and block time, the signer's
/// address, the factory's code, a published release, the addresses it names, its
/// EntryPoint, and a readable operator fee. What the running relayer serves through,
/// pausing or refusing requests, it returns as warnings: the latest release outside
/// `supported_releases`, a balance under `min_balance`, a global budget under one
/// request's worst loss, and the operator fee on.
pub async fn check<P: Provider>(
    provider: &P,
    config: &Config,
    signer: Address,
) -> Result<Vec<String>, Error> {
    let fail = |e: String| Err(e.into());
    let mut warnings = vec![];
    let (chain_id, ours) = (provider.get_chain_id().await?, config.chain_id);
    if chain_id != ours {
        return fail(format!(
            "the node is on chain {chain_id}, [chain] chain_id {ours}: point rpc_url at it"
        ));
    }
    block_time(provider, config).await?;
    if signer != config.relayer {
        let at = config.relayer;
        return fail(format!(
            "the signer is {signer}, [key] address {at}: set address to the signer's"
        ));
    }
    let at = config.factory;
    if provider.get_code_at(at).await?.is_empty() {
        return fail(format!(
            "no code at [contracts] factory {at}: set its address"
        ));
    }
    let factory = WalletFactory::new(config.factory, provider);
    let latest = factory.latest().call().await?;
    if latest.implementation == Address::ZERO {
        return fail(
            "the factory has published no release: publish one, or set [contracts] \
             factory to one that has"
                .into(),
        );
    }
    let (version, wallet) = (
        latest.version,
        IWebWallet::new(latest.implementation, provider),
    );
    let registry = factory.REGISTRY().call().await?;
    let names = (
        wallet.FACTORY().call().await?,
        wallet.REGISTRY().call().await?,
    );
    if names != (config.factory, registry) {
        let (f, r) = names;
        return fail(format!(
            "release {version} names FACTORY() {f} and REGISTRY() {r}, not the factory and \
             its REGISTRY() {registry}: set the factory that published it"
        ));
    }
    let entry_point = wallet.entryPoint().call().await?;
    if provider.get_code_at(entry_point).await?.is_empty() {
        return fail(format!(
            "no EntryPoint v0.9 at {entry_point}, which release {version} calls: deploy it"
        ));
    }
    if !config.supported_releases.contains(&version) {
        warnings.push(format!(
            "the factory's latest release {version} is not in [limits] supported_releases, \
             whose wallets are UnsupportedRelease: run a relayer that decodes it"
        ));
    }
    let (balance, floor) = (provider.get_balance(signer).await?, config.min_balance);
    if balance < floor {
        warnings.push(format!(
            "the signer's balance {balance} is below [limits] min_balance {floor}, so \
             sending pauses: fund {signer}"
        ));
    }
    let refused = |e| format!("{e:?}");
    let head = gas::head(provider, config).await.map_err(refused)?;
    // The largest transaction of max_gas_limit gas: all its calldata zero bytes, at
    // most the bytes a pool accepts.
    let size = U256::from((config.max_gas_limit / gas::ZERO_BYTE).min(gas::MAX_TX_BYTES));
    let l1 = gas::l1_bound(provider, config, size, head.at());
    let l1 = l1.await.map_err(refused)?;
    let gas = U256::from(config.max_gas_limit) * U256::from(head.fee_bound(config));
    let worst = gas.saturating_add(l1);
    let budget = config.global_budget.wei;
    if budget < worst {
        warnings.push(format!(
            "[limits] global_budget {budget} is below one request's worst loss {worst}, \
             max_gas_limit at max_fee_percent of the base fee plus the L1 fee bound of \
             that much calldata, so such a request is OverBudget: raise it"
        ));
    }
    warnings.extend(operator_fee(provider, config).await?);
    Ok(warnings)
}

/// `block_time` within 25% of the chain's mean over the blocks of the last
/// [`BLOCK_TIME_SPAN`], on an OP-stack chain or one whose fee rule is in its headers;
/// anvil mines on demand. A chain younger than the span is not measured.
async fn block_time<P: Provider>(provider: &P, config: &Config) -> Result<(), Error> {
    if config.l1_fee != L1Fee::OpStack && config.fee_params != FeeParams::Header {
        return Ok(());
    }
    let n = u64::try_from(config.blocks(BLOCK_TIME_SPAN).max(1))?;
    let time = async |at: BlockNumberOrTag| -> Result<(u64, u64), Error> {
        let block = provider.get_block_by_number(at).await?;
        let header = block.ok_or("the node has no such block")?.header;
        Ok((header.number, header.timestamp))
    };
    let (latest, now) = time(BlockNumberOrTag::Latest).await?;
    let Some(back) = latest.checked_sub(n) else {
        return Ok(());
    };
    let (_, then) = time(back.into()).await?;
    let measured = Duration::from_millis(now.saturating_sub(then) * 1000 / n);
    let ours = config.block_time;
    if measured.abs_diff(ours) * 4 > ours {
        return Err(format!(
            "[chain] block_time {ours:?} is off the chain's {measured:?} a block over its \
             last {n} blocks: set it"
        )
        .into());
    }
    Ok(())
}

/// The OP-stack operator fee, which no refund bounds, read: a warning when it is on or
/// has no getters.
async fn operator_fee<P: Provider>(
    provider: &P,
    config: &Config,
) -> Result<Option<String>, Error> {
    let fee = gas::operator_fee(provider, config, BlockId::latest()).await;
    Ok(match fee.map_err(|e| format!("{e:?}"))? {
        gas::OperatorFee::On(s, c) => Some(format!(
            "the operator fee is on (scalar {s}, constant {c}), which no refund bounds, \
             so sending pauses until it is off"
        )),
        gas::OperatorFee::Unread => {
            Some("L1Block has no operator fee getters: none to check".into())
        }
        gas::OperatorFee::Off => None,
    })
}

/// How old the last good tick may be while `/health` answers `OK`: a tick's whole
/// timeout and ten more polls.
fn stale_after(tick_timeout: Duration, poll_interval: Duration) -> Duration {
    tick_timeout + poll_interval * 10
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_goes_stale_after_a_tick_timeout_and_ten_polls() {
        let stale = stale_after(Duration::from_secs(30), Duration::from_secs(1));
        assert_eq!(stale, Duration::from_secs(40));
    }
}
