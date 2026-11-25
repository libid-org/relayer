//! The relayer's own preconditions on a request, checked before it sends:
//! canonical data, the refund, the target, the deadline and the gas limit.

use std::{
    fmt::Display,
    time::Duration,
};

use alloy::{
    eips::BlockId,
    network::TransactionBuilder,
    primitives::{
        Address,
        Bytes,
        B256,
        U256,
    },
    providers::Provider,
    rpc::types::{
        BlockOverrides,
        TransactionRequest,
    },
    sol_types::SolCall,
    transports::TransportError,
};
use libid_contracts::{
    bindings::wallet::{
        decode_executions,
        ExecuteAuthorization,
        ExecuteMessage,
        Execution,
        IWebWallet,
        WalletFactory,
        AUTHORIZED_MODE,
    },
    wallet_target::{
        check_target,
        Target,
    },
    WalletRegistryError,
};
use serde::Deserialize;

use crate::{
    config::Config,
    gas,
};

/// The create path's wallet: `WalletFactory.createAndExecute(platformId, idNode, salt, …)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Create {
    /// The genesis identity's platform.
    pub platform_id: B256,
    /// The genesis identity's id node.
    pub id_node: B256,
    /// The wallet's salt.
    pub salt: U256,
}

/// A signed batch to relay. Its `value` is the one `data` carries.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    /// The wallet that runs the batch.
    pub wallet: Address,
    /// Set when the wallet is created in the same transaction.
    #[serde(default)]
    pub create: Option<Create>,
    /// `execute(AUTHORIZED_MODE, data)`'s `data`, signed.
    pub data: Bytes,
    /// The gas limit the client's refund was priced for; the send's limit is at most this.
    pub gas_limit: u64,
}

/// A request that passed every check: the only thing the relayer queues.
#[derive(Clone, Debug)]
pub struct Checked {
    /// The transaction to send, at the cap from R; simulated at its cap bounded by M.
    pub(crate) tx: TransactionRequest,
    /// `R - value`: the most a send may cost the relayer.
    pub(crate) net: U256,
    /// The L1 fee bound with its margin, which the refund covers.
    pub(crate) l1_fee_bound: U256,
}

/// Why a request is not sent.
#[derive(Debug)]
pub enum Refusal {
    /// `data` does not decode, or does not re-encode to itself byte for byte.
    NoncanonicalData,
    /// No top-level `Execution(relayer, R, "")` in the batch.
    RefundMissing,
    /// `R - value` does not cover the gas and L1 fee bound.
    RefundTooLow {
        /// The batch's refund.
        refund: U256,
        /// The least refund the relayer accepts.
        required: U256,
    },
    /// The signed batch needs a higher gas limit than the one its refund was priced for.
    GasLimitTooLow {
        /// The limit to price a re-signed batch for: the zero bytes its client fills
        /// priced as nonzero.
        required: u64,
    },
    /// The gas limit is above the configured `max_gas_limit`.
    GasLimitTooHigh {
        /// The highest gas limit relayed.
        max: u64,
    },
    /// The batch's value is above the configured `max_value`.
    ValueTooHigh {
        /// The most value relayed, in wei.
        max: U256,
    },
    /// The contracts do not vouch for the target.
    UnknownWallet,
    /// An EIP-7702 account's batch carries value, which the relayer does not front.
    ValueNotAllowed,
    /// The target runs a release outside the configured supported set.
    UnsupportedRelease {
        /// The target's release version.
        version: u64,
    },
    /// The batch's deadline falls before the inclusion window ends.
    Expired,
    /// The simulation reverted, decoded on the wallet route where it can be.
    Reverted {
        /// The decoded error.
        error: Option<WalletRegistryError>,
        /// The revert data.
        data: Bytes,
    },
    /// The relayer's balance at its send does not cover its worst cost and value.
    RelayerUnderfunded {
        /// The balance at the pinned block.
        balance: U256,
        /// The send's worst cost plus its value.
        required: U256,
    },
    /// Another batch of the wallet is in flight.
    WalletBusy {
        /// The request in flight.
        id: B256,
    },
    /// The wallet lost its loss budget.
    RateLimited,
    /// Sending paused: all wallets lost the global loss budget.
    Paused,
    /// The node could not answer.
    Node(String),
}

impl Refusal {
    /// The refusal's name, as the API and metrics report it.
    pub fn name(&self) -> &'static str {
        match self {
            Self::NoncanonicalData => "NoncanonicalData",
            Self::RefundMissing => "RefundMissing",
            Self::RefundTooLow { .. } => "RefundTooLow",
            Self::GasLimitTooLow { .. } => "GasLimitTooLow",
            Self::GasLimitTooHigh { .. } => "GasLimitTooHigh",
            Self::ValueTooHigh { .. } => "ValueTooHigh",
            Self::UnknownWallet => "UnknownWallet",
            Self::ValueNotAllowed => "ValueNotAllowed",
            Self::UnsupportedRelease { .. } => "UnsupportedRelease",
            Self::Expired => "Expired",
            Self::Reverted { .. } => "Reverted",
            Self::RelayerUnderfunded { .. } => "RelayerUnderfunded",
            Self::WalletBusy { .. } => "WalletBusy",
            Self::RateLimited => "RateLimited",
            Self::Paused => "Paused",
            Self::Node(_) => "Node",
        }
    }
}

impl Request {
    /// Check this request against `config` on the chain `provider` reads, and set
    /// its gas limit from its simulation.
    pub async fn check<P: Provider>(
        &self,
        provider: &P,
        config: &Config,
    ) -> Result<Checked, Refusal> {
        under_max(self.gas_limit, config)?;
        let (message, authorization, executions) =
            decode(&self.data).ok_or(Refusal::NoncanonicalData)?;
        under_max_value(message.value, config)?;
        let refund = refund(&executions, config.relayer).ok_or(Refusal::RefundMissing)?;
        let late =
            unexpired(provider, message.deadline.to(), config.inclusion_window).await?;
        let (wallet, create) = (self.wallet, self.create);
        let call = target(provider, config, wallet, create, &self.data, message.value);
        let call = call.await?;
        let head = gas::head(provider, config).await?;
        let priced = gas::price(provider, config, &call, self.gas_limit, head.floor);
        let (l1_fee_bound, cost) = priced.await?;
        let value = message.value;
        let Some(net) = refund.checked_sub(value).filter(|net| *net >= cost) else {
            let required = value.saturating_add(cost);
            return Err(Refusal::RefundTooLow { refund, required });
        };
        let call_gas = message.call_gas.try_into().unwrap_or(u64::MAX);
        let (create, priced_for) = (create.is_some(), self.gas_limit);
        let refunds = refunds(&executions, config.relayer);
        let fillable = gas::fillable(&message, &authorization, refunds);
        let limit = relay_limit(
            provider, &call, late, call_gas, create, priced_for, fillable,
        );
        let limit = limit.await?;
        // The fee cap the refund pays for at the limit sent: at least the floor.
        let cap = (net - l1_fee_bound) / U256::from(limit);
        let cap = u128::try_from(cap).unwrap_or(u128::MAX);
        let tip = gas::tip(provider, config.tip_percentile).await?;
        let at_cap = |cap: u128| {
            call.clone()
                .with_gas_limit(limit)
                .with_max_fee_per_gas(cap)
                .with_max_priority_fee_per_gas(tip.min(cap))
                .with_chain_id(config.chain_id)
        };
        // Run at the cap a send uses; queued at the cap from R.
        simulate(
            provider,
            &at_cap(cap.min(head.fee_bound(config))),
            None,
            late,
        )
        .await?;
        let tx = at_cap(cap);
        Ok(Checked {
            tx,
            net,
            l1_fee_bound,
        })
    }
}

/// The message `data` carries, its authorization and its batch, all canonical.
pub(crate) fn decode(
    data: &Bytes,
) -> Option<(ExecuteMessage, ExecuteAuthorization, Vec<Execution>)> {
    let (message, authorization) = ExecuteMessage::from_authorized_data(data).ok()?;
    if message.authorized_data(authorization.clone()) != *data {
        return None;
    }
    let executions = decode_executions(&message.execution_calldata).ok()?;
    Some((message, authorization, executions))
}

/// R: what the batch's top-level `Execution(relayer, R, "")` calls pay.
fn refund(executions: &[Execution], relayer: Address) -> Option<U256> {
    refunds(executions, relayer).reduce(U256::saturating_add)
}

/// The values of the batch's top-level `Execution(relayer, R, "")` calls.
pub(crate) fn refunds(
    executions: &[Execution],
    relayer: Address,
) -> impl Iterator<Item = U256> + Clone + '_ {
    executions
        .iter()
        .filter(move |e| e.target == relayer && e.callData.is_empty())
        .map(|e| e.value)
}

/// The gas limit `call` is sent at, from its run at `late`, for its signed
/// `call_gas` and path; at most the limit its refund was priced for. A refusal
/// reports the limit with the `fillable` gas a re-signed batch may add.
async fn relay_limit<P: Provider>(
    provider: &P,
    call: &TransactionRequest,
    late: u64,
    call_gas: u64,
    create: bool,
    priced_for: u64,
    fillable: u64,
) -> Result<u64, Refusal> {
    let sim = gas::simulate(provider, call, late).await?;
    if !sim.status {
        return Err(reverted(sim.return_data));
    }
    let required = gas::limit(sim.gas_used, call_gas, create);
    if required > priced_for {
        let required = gas::limit(sim.gas_used + fillable, call_gas, create);
        return Err(Refusal::GasLimitTooLow { required });
    }
    Ok(required)
}

/// The end of the inclusion window after the latest block, which `deadline` is not before.
async fn unexpired<P: Provider>(
    provider: &P,
    deadline: u64,
    window: Duration,
) -> Result<u64, Refusal> {
    let late = gas::latest_time(provider)
        .await?
        .saturating_add(window.as_secs());
    covered(deadline, late)
}

/// `GasLimitTooHigh` above `config.max_gas_limit`.
#[allow(clippy::result_large_err)] // the `Refusal` that its callers return
pub(crate) fn under_max(gas_limit: u64, config: &Config) -> Result<(), Refusal> {
    let max = config.max_gas_limit;
    (gas_limit <= max)
        .then_some(())
        .ok_or(Refusal::GasLimitTooHigh { max })
}

/// `ValueTooHigh` above `config.max_value`.
#[allow(clippy::result_large_err)] // the `Refusal` that its callers return
pub(crate) fn under_max_value(value: U256, config: &Config) -> Result<(), Refusal> {
    match config.max_value {
        Some(max) if value > max => Err(Refusal::ValueTooHigh { max }),
        _ => Ok(()),
    }
}

/// `late`, unless `deadline` falls before it.
#[allow(clippy::result_large_err)] // the `Refusal` that its callers return
fn covered(deadline: u64, late: u64) -> Result<u64, Refusal> {
    if deadline < late {
        return Err(Refusal::Expired);
    }
    Ok(late)
}

/// The relayer's call of `wallet`'s batch `data` with `value`, when the contracts
/// vouch for the wallet on a supported release; a 7702 account only with no value.
pub(crate) async fn target<P: Provider>(
    provider: &P,
    config: &Config,
    wallet: Address,
    create: Option<Create>,
    data: &Bytes,
    value: U256,
) -> Result<TransactionRequest, Refusal> {
    if let Some(c) = create {
        let factory = WalletFactory::new(config.factory, provider);
        let address = factory.walletAddress(c.platform_id, c.id_node, c.salt);
        if address.call().await.map_err(node)? != wallet {
            return Err(Refusal::UnknownWallet);
        }
    }
    vouched(provider, config, wallet, create.is_some(), value).await?;
    Ok(outer(config, wallet, create, data, value))
}

/// The relayer's call of `wallet`'s batch `data` with `value`, unchecked.
pub(crate) fn outer(
    config: &Config,
    wallet: Address,
    create: Option<Create>,
    data: &Bytes,
    value: U256,
) -> TransactionRequest {
    let call = TransactionRequest::default()
        .with_from(config.relayer)
        .with_value(value);
    let data = data.clone();
    if let Some(c) = create {
        let input = (c.platform_id, c.id_node, c.salt, AUTHORIZED_MODE, data);
        let input = WalletFactory::createAndExecuteCall::new(input).abi_encode();
        return call.with_to(config.factory).with_input(input);
    }
    let input = IWebWallet::executeCall::new((AUTHORIZED_MODE, data)).abi_encode();
    call.with_to(wallet).with_input(input)
}

/// The contracts vouch for `wallet` on a supported release: on the create path with
/// no code yet, the factory's latest; a 7702 account only with no value.
pub(crate) async fn vouched<P: Provider>(
    provider: &P,
    config: &Config,
    wallet: Address,
    create: bool,
    value: U256,
) -> Result<(), Refusal> {
    let new = create && provider.get_code_at(wallet).await.map_err(node)?.is_empty();
    let version = if new {
        let factory = WalletFactory::new(config.factory, provider);
        factory.latest().call().await.map_err(node)?.version
    } else {
        let vouched = check_target(provider, config.factory, config.forwarder, wallet);
        match vouched.await.map_err(node)? {
            Some(Target::FactoryWallet(release)) => release.version,
            Some(Target::Account(release)) if value.is_zero() => release.version,
            Some(Target::Account(_)) => return Err(Refusal::ValueNotAllowed),
            None => return Err(Refusal::UnknownWallet),
        }
    };
    if !config.supported_releases.contains(&version) {
        return Err(Refusal::UnsupportedRelease { version });
    }
    Ok(())
}

/// `tx`, as it is sent, still runs after the block `at` and at `late`, which
/// `deadline` is not before.
pub(crate) async fn rerun<P: Provider>(
    provider: &P,
    tx: &TransactionRequest,
    deadline: u64,
    at: BlockId,
    late: u64,
) -> Result<(), Refusal> {
    let late = covered(deadline, late)?;
    simulate(provider, tx, Some(at), late).await
}

/// `tx` runs without reverting after `block`, latest by default, and with its
/// timestamp at `late`.
async fn simulate<P: Provider>(
    provider: &P,
    tx: &TransactionRequest,
    block: Option<BlockId>,
    late: u64,
) -> Result<(), Refusal> {
    let at_late = BlockOverrides {
        time: Some(late),
        ..Default::default()
    };
    for overrides in [None, Some(at_late)] {
        let call = provider
            .call(tx.clone())
            .block_opt(block)
            .with_block_overrides_opt(overrides);
        call.await.map_err(refusal)?;
    }
    Ok(())
}

fn refusal(e: TransportError) -> Refusal {
    match e.as_error_resp() {
        Some(resp) if resp.code == 3 => {
            reverted(resp.as_revert_data().unwrap_or_default())
        }
        _ => node(e),
    }
}

/// A revert with `data`, decoded on the wallet route where it can be.
pub(crate) fn reverted(data: Bytes) -> Refusal {
    let error = WalletRegistryError::decode(&data);
    Refusal::Reverted { error, data }
}

pub(crate) fn node(e: impl Display) -> Refusal {
    Refusal::Node(e.to_string())
}
