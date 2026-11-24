//! The relayer's own preconditions on a request, checked before it sends:
//! canonical data (D4), the refund (D5), the target (D9) and the deadline.

use std::fmt::Display;

use alloy::{
    eips::BlockNumberOrTag,
    network::TransactionBuilder,
    primitives::{
        Address,
        Bytes,
        B256,
        U256,
    },
    providers::Provider,
    rpc::types::TransactionRequest,
    sol_types::SolCall,
};
use libid_contracts::{
    bindings::wallet::{
        decode_executions,
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

use crate::config::{
    Config,
    L1Fee,
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
    /// The gas limit the client's refund was priced for.
    pub gas_limit: u64,
}

/// A request that passed every check: the only thing [`crate::chain::send`] sends.
#[derive(Clone, Debug)]
pub struct Checked {
    /// The transaction that was simulated, as it is sent.
    pub(crate) tx: TransactionRequest,
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
    /// The contracts do not vouch for the target.
    UnknownWallet,
    /// The batch's deadline is past.
    Expired,
    /// The simulation reverted, decoded on the wallet route where it can be.
    Reverted {
        /// The decoded error.
        error: Option<WalletRegistryError>,
        /// The revert data.
        data: Bytes,
    },
    /// The node could not answer.
    Node(String),
}

/// Check `request` against `config` on the chain `provider` reads.
pub async fn check<P: Provider>(
    provider: &P,
    config: &Config,
    request: &Request,
) -> Result<Checked, Refusal> {
    let (message, executions) = decode(&request.data).ok_or(Refusal::NoncanonicalData)?;
    let refund = refund(&executions, config.relayer).ok_or(Refusal::RefundMissing)?;
    let fees = provider.estimate_eip1559_fees().await.map_err(node)?;
    let gas_cost = U256::from(request.gas_limit)
        .saturating_mul(U256::from(fees.max_fee_per_gas))
        .saturating_add(l1_fee_bound(config).map_err(node)?);
    if refund
        .checked_sub(message.value)
        .is_none_or(|net| net < gas_cost)
    {
        let required = message.value.saturating_add(gas_cost);
        return Err(Refusal::RefundTooLow { refund, required });
    }
    let latest = provider.get_block_by_number(BlockNumberOrTag::Latest);
    let latest = latest.await.map_err(node)?;
    let now = latest
        .ok_or_else(|| node("no latest block"))?
        .header
        .timestamp;
    if now > message.deadline.to::<u64>() {
        return Err(Refusal::Expired);
    }
    let (to, input) = target(provider, config, request, message.value).await?;
    let tx = TransactionRequest::default()
        .with_from(config.relayer)
        .with_to(to)
        .with_value(message.value)
        .with_input(input)
        .with_gas_limit(request.gas_limit)
        .with_max_fee_per_gas(fees.max_fee_per_gas)
        .with_max_priority_fee_per_gas(
            fees.max_priority_fee_per_gas.min(fees.max_fee_per_gas),
        )
        .with_chain_id(config.chain_id);
    simulate(provider, &tx).await?;
    Ok(Checked { tx })
}

/// The message `data` carries and its batch, both canonical (D4).
fn decode(data: &Bytes) -> Option<(ExecuteMessage, Vec<Execution>)> {
    let (message, authorization) = ExecuteMessage::from_authorized_data(data).ok()?;
    if message.authorized_data(authorization) != *data {
        return None;
    }
    let executions = decode_executions(&message.execution_calldata).ok()?;
    Some((message, executions))
}

/// R: what the batch's top-level `Execution(relayer, R, "")` calls pay.
fn refund(executions: &[Execution], relayer: Address) -> Option<U256> {
    executions
        .iter()
        .filter(|e| e.target == relayer && e.callData.is_empty())
        .map(|e| e.value)
        .reduce(U256::saturating_add)
}

/// The L1 data fee bound a refund must cover (D5).
fn l1_fee_bound(config: &Config) -> Result<U256, &'static str> {
    match config.l1_fee {
        L1Fee::None => Ok(U256::ZERO),
        L1Fee::OpStack => Err("the OP-stack L1 fee bound is not implemented"),
    }
}

/// The address to send to and its calldata, when the contracts vouch for
/// the wallet (D9); a 7702 account only with no value (D5a).
async fn target<P: Provider>(
    provider: &P,
    config: &Config,
    request: &Request,
    value: U256,
) -> Result<(Address, Bytes), Refusal> {
    let data = request.data.clone();
    if let Some(c) = request.create {
        let factory = WalletFactory::new(config.factory, provider);
        let wallet = factory.walletAddress(c.platform_id, c.id_node, c.salt);
        if wallet.call().await.map_err(node)? != request.wallet {
            return Err(Refusal::UnknownWallet);
        }
        let call = (c.platform_id, c.id_node, c.salt, AUTHORIZED_MODE, data);
        let call = WalletFactory::createAndExecuteCall::new(call);
        return Ok((config.factory, call.abi_encode().into()));
    }
    let vouched =
        check_target(provider, config.factory, config.forwarder, request.wallet);
    match vouched.await.map_err(node)? {
        Some(Target::FactoryWallet(_)) => {}
        Some(Target::Account(_)) if value.is_zero() => {}
        _ => return Err(Refusal::UnknownWallet),
    }
    let call = IWebWallet::executeCall::new((AUTHORIZED_MODE, data));
    Ok((request.wallet, call.abi_encode().into()))
}

/// `tx` runs without reverting on the pending block.
async fn simulate<P: Provider>(
    provider: &P,
    tx: &TransactionRequest,
) -> Result<(), Refusal> {
    let Err(e) = provider.call(tx.clone()).await else {
        return Ok(());
    };
    match e.as_error_resp() {
        Some(resp) if resp.code == 3 => {
            let data = resp.as_revert_data().unwrap_or_default();
            let error = WalletRegistryError::decode(&data);
            Err(Refusal::Reverted { error, data })
        }
        _ => Err(node(e)),
    }
}

fn node(e: impl Display) -> Refusal {
    Refusal::Node(e.to_string())
}
