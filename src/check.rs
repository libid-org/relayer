//! The relayer's own preconditions on a request, checked before it sends:
//! canonical data, the refund, the target, the deadline and the gas limit.

use std::{
    fmt::Display,
    time::Duration,
};

use alloy::{
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

use crate::{
    config::Config,
    gas,
};

/// The create path's wallet: `WalletFactory.createAndExecute(platformId, idNode, salt, …)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Create {
    /// The genesis identity's platform.
    pub platform_id: B256,
    /// The genesis identity's id node.
    pub id_node: B256,
    /// The wallet's salt.
    pub salt: U256,
}

/// A signed batch to relay. Its `value` is the one `data` carries.
#[derive(Clone, Debug)]
pub struct Request {
    /// The wallet that runs the batch.
    pub wallet: Address,
    /// Set when the wallet is created in the same transaction.
    pub create: Option<Create>,
    /// `execute(AUTHORIZED_MODE, data)`'s `data`, signed.
    pub data: Bytes,
    /// The gas limit the client's refund was priced for; the send's limit is at most this.
    pub gas_limit: u64,
}

/// A request that passed every check: the only thing the relayer queues.
#[derive(Clone, Debug)]
pub struct Checked {
    /// The transaction that was simulated, as it is sent.
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
    /// The contracts do not vouch for the target.
    UnknownWallet,
    /// The batch's deadline falls before the inclusion window ends.
    Expired,
    /// The simulation reverted, decoded on the wallet route where it can be.
    Reverted {
        /// The decoded error.
        error: Option<WalletRegistryError>,
        /// The revert data.
        data: Bytes,
    },
    /// Another batch of the wallet is in flight.
    WalletBusy {
        /// The request in flight.
        id: B256,
    },
    /// The node could not answer.
    Node(String),
}

/// Check `request` against `config` on the chain `provider` reads, and set its
/// gas limit from its simulation.
pub async fn check<P: Provider>(
    provider: &P,
    config: &Config,
    request: &Request,
) -> Result<Checked, Refusal> {
    let (message, authorization, executions) =
        decode(&request.data).ok_or(Refusal::NoncanonicalData)?;
    let refund = refund(&executions, config.relayer).ok_or(Refusal::RefundMissing)?;
    let late =
        unexpired(provider, message.deadline.to(), config.inclusion_window).await?;
    let (wallet, create) = (request.wallet, request.create);
    let call = target(
        provider,
        config,
        wallet,
        create,
        &request.data,
        message.value,
    );
    let call = call.await?;
    let floor = gas::fee_floor(provider, config, config.inclusion_window).await?;
    let priced = gas::price(provider, config, &call, request.gas_limit, floor);
    let (l1_fee_bound, cost) = priced.await?;
    let value = message.value;
    let Some(net) = refund.checked_sub(value).filter(|net| *net >= cost) else {
        let required = value.saturating_add(cost);
        return Err(Refusal::RefundTooLow { refund, required });
    };
    let call_gas = message.call_gas.try_into().unwrap_or(u64::MAX);
    let (create, priced_for) = (create.is_some(), request.gas_limit);
    let refunds = refunds(&executions, config.relayer);
    let fillable = gas::fillable(&message, &authorization, refunds);
    let limit = relay_limit(
        provider, &call, late, call_gas, create, priced_for, fillable,
    );
    let limit = limit.await?;
    // The fee cap the refund pays for at the limit sent: at least `floor`.
    let cap = (net - l1_fee_bound) / U256::from(limit);
    let cap = u128::try_from(cap).unwrap_or(u128::MAX);
    let tip = gas::tip(provider, config.tip_percentile).await?.min(cap);
    let tx = call
        .with_gas_limit(limit)
        .with_max_fee_per_gas(cap)
        .with_max_priority_fee_per_gas(tip)
        .with_chain_id(config.chain_id);
    simulate(provider, &tx, late).await?;
    Ok(Checked {
        tx,
        net,
        l1_fee_bound,
    })
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
    if deadline < late {
        return Err(Refusal::Expired);
    }
    Ok(late)
}

/// The relayer's call of `wallet`'s batch `data` with `value`, when the contracts
/// vouch for the wallet; a 7702 account only with no value.
pub(crate) async fn target<P: Provider>(
    provider: &P,
    config: &Config,
    wallet: Address,
    create: Option<Create>,
    data: &Bytes,
    value: U256,
) -> Result<TransactionRequest, Refusal> {
    let call = TransactionRequest::default()
        .with_from(config.relayer)
        .with_value(value);
    let data = data.clone();
    if let Some(c) = create {
        let factory = WalletFactory::new(config.factory, provider);
        let address = factory.walletAddress(c.platform_id, c.id_node, c.salt);
        if address.call().await.map_err(node)? != wallet {
            return Err(Refusal::UnknownWallet);
        }
        let input = (c.platform_id, c.id_node, c.salt, AUTHORIZED_MODE, data);
        let input = WalletFactory::createAndExecuteCall::new(input).abi_encode();
        return Ok(call.with_to(config.factory).with_input(input));
    }
    let vouched = check_target(provider, config.factory, config.forwarder, wallet);
    match vouched.await.map_err(node)? {
        Some(Target::FactoryWallet(_)) => {}
        Some(Target::Account(_)) if value.is_zero() => {}
        _ => return Err(Refusal::UnknownWallet),
    }
    let input = IWebWallet::executeCall::new((AUTHORIZED_MODE, data)).abi_encode();
    Ok(call.with_to(wallet).with_input(input))
}

/// `tx`, as it is sent, still runs before `deadline` until the inclusion window ends.
pub(crate) async fn rerun<P: Provider>(
    provider: &P,
    config: &Config,
    tx: &TransactionRequest,
    deadline: u64,
) -> Result<(), Refusal> {
    let late = unexpired(provider, deadline, config.inclusion_window).await?;
    simulate(provider, tx, late).await
}

/// `tx` runs without reverting on the latest block, and with its timestamp at `late`.
async fn simulate<P: Provider>(
    provider: &P,
    tx: &TransactionRequest,
    late: u64,
) -> Result<(), Refusal> {
    let at_late = BlockOverrides {
        time: Some(late),
        ..Default::default()
    };
    for overrides in [None, Some(at_late)] {
        let call = provider
            .call(tx.clone())
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
