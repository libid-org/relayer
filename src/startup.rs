//! `relayer run`: the signer, the chain checked against the configuration, then the
//! relayer served. Each check fails naming its fix.

use std::{
    future::Future,
    net::Ipv4Addr,
};

use alloy::{
    primitives::{
        address,
        Address,
        U256,
    },
    providers::{
        Provider,
        ProviderBuilder,
    },
};
use libid_contracts::bindings::wallet::{
    IWebWallet,
    WalletFactory,
    WalletForwarder,
};
use libid_signer::SignerSource;
use tokio::net::TcpListener;

use crate::{
    config::{
        Config,
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

/// The OP-stack `L1Block` predeploy.
const L1_BLOCK: Address = address!("4200000000000000000000000000000000000015");

alloy::sol! {
    #[sol(rpc)]
    interface L1Block {
        function operatorFeeScalar() external view returns (uint32);
        function operatorFeeConstant() external view returns (uint64);
    }
}

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
    let provider = ProviderBuilder::new()
        .wallet(signer.wallet())
        .connect_http(rpc_url.parse()?);
    check(&provider, &config, signer.address()).await?;
    let relayer = Relayer::start(provider, config, &db).await?;
    let api = TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).await?;
    let metrics = TcpListener::bind((Ipv4Addr::UNSPECIFIED, metrics_port)).await?;
    let service = Service::new(relayer, poll_interval * 10);
    Ok(serve(service, poll_interval, limits, (api, metrics), shutdown).await?)
}

/// The chain `provider` reads is the one `config` names, for the key at `signer`:
/// the chain id, the signer's address and balance, the contracts' code and the
/// addresses they name, the factory's latest release, its EntryPoint, the wallet budget
/// against one request's worst loss, and no OP-stack operator fee.
pub async fn check<P: Provider>(
    provider: &P,
    config: &Config,
    signer: Address,
) -> Result<(), Error> {
    let fail = |e: String| Err(e.into());
    let (chain_id, ours) = (provider.get_chain_id().await?, config.chain_id);
    if chain_id != ours {
        return fail(format!(
            "the node is on chain {chain_id}, [chain] chain_id {ours}: point rpc_url at it"
        ));
    }
    if signer != config.relayer {
        let at = config.relayer;
        return fail(format!(
            "the signer is {signer}, [key] address {at}: set address to the signer's"
        ));
    }
    for (name, at) in [("factory", config.factory), ("forwarder", config.forwarder)] {
        if provider.get_code_at(at).await?.is_empty() {
            return fail(format!(
                "no code at [contracts] {name} {at}: set its address"
            ));
        }
    }
    let factory = WalletFactory::new(config.factory, provider);
    let forwarder = WalletForwarder::new(config.forwarder, provider);
    let named = forwarder.FACTORY().call().await?;
    if named != config.factory {
        return fail(format!(
            "the forwarder's FACTORY() is {named}, not the factory: set this factory's forwarder"
        ));
    }
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
        return fail(format!(
            "the factory's latest release {version} is not in [limits] supported_releases: \
             run a relayer that decodes it"
        ));
    }
    let (balance, floor) = (provider.get_balance(signer).await?, config.min_balance);
    if balance < floor {
        return fail(format!(
            "the signer's balance {balance} is below [limits] min_balance {floor}: fund {signer}"
        ));
    }
    let refused = |e| format!("{e:?}");
    let head = gas::head(provider, config).await.map_err(refused)?;
    // The largest transaction of max_gas_limit gas: all its calldata zero bytes.
    let size = U256::from(config.max_gas_limit / gas::ZERO_BYTE);
    let l1 = gas::l1_bound(provider, config, size, head.at());
    let l1 = l1.await.map_err(refused)?;
    let gas = U256::from(config.max_gas_limit) * U256::from(head.fee_bound(config));
    let value = config.max_value.unwrap_or_default();
    let worst = gas.saturating_add(l1).saturating_add(value);
    let budget = config.wallet_budget.wei;
    if budget < worst {
        let value = match config.max_value {
            Some(_) => "plus max_value",
            None => "and no batch value, as max_value is unset",
        };
        return fail(format!(
            "[limits] wallet_budget {budget} is below one request's worst loss {worst}, \
             max_gas_limit at max_fee_percent of the base fee plus the L1 fee bound of \
             that much calldata, {value}: raise it"
        ));
    }
    operator_fee(provider, config).await
}

/// No OP-stack operator fee, which no refund bounds; none to read on a chain before
/// Isthmus, whose `L1Block` has no getter and reverts.
async fn operator_fee<P: Provider>(provider: &P, config: &Config) -> Result<(), Error> {
    if config.l1_fee != L1Fee::OpStack {
        return Ok(());
    }
    let l1_block = L1Block::new(L1_BLOCK, provider);
    let (scalar, constant) = (
        l1_block.operatorFeeScalar().call().await,
        l1_block.operatorFeeConstant().call().await,
    );
    match (scalar, constant) {
        (Ok(0), Ok(0)) => Ok(()),
        (Ok(s), Ok(c)) => Err(format!(
            "the operator fee is on (scalar {s}, constant {c}), which no refund bounds: \
             relay on a chain without one"
        )
        .into()),
        (Err(e), _) | (_, Err(e)) if reverted(&e) => {
            tracing::warn!("L1Block has no operator fee getters: none to check");
            Ok(())
        }
        (Err(e), _) | (_, Err(e)) => Err(e.into()),
    }
}

/// The call reverted, or found no code.
fn reverted(e: &alloy::contract::Error) -> bool {
    use alloy::contract::Error::*;
    match e {
        ZeroData(..) => true,
        TransportError(e) => e
            .as_error_resp()
            .is_some_and(|r| r.code == 3 || r.message.contains("revert")),
        _ => false,
    }
}
