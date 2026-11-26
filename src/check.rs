//! The relayer's own preconditions on a request, checked before it sends:
//! canonical data, the refund, the target, the deadline and the gas limit.

use std::fmt::Display;

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

/// The block times a deadline may run past `validUntil` plus the inclusion window: a
/// relay read at a node behind the quote's.
const DEADLINE_SLACK: u32 = 2;

/// A wallet's genesis identity, `(platformId, idNode)`: what a loss is charged to.
pub type Identity = (B256, B256);

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
    /// The wallet's genesis identity.
    pub(crate) identity: Identity,
    /// What a send costs at most at the head's cap, its value included.
    pub(crate) worst: U256,
}

/// Why a request is not sent.
#[derive(Debug)]
pub enum Refusal {
    /// `data` does not decode, or does not re-encode to itself byte for byte.
    NoncanonicalData,
    /// No top-level `Execution(relayer, R, "")` in the batch.
    RefundMissing,
    /// The batch carries no sign-in: a top-level `addSessionByLogin` of the wallet, or
    /// its `linkIdentity` while it never had a session.
    SignInMissing,
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
    /// The signed transaction would be over the bytes a node's pool accepts.
    TooLarge {
        /// The most bytes of a signed transaction.
        max: u64,
    },
    /// The batch's value is above the configured `max_value`.
    ValueTooHigh {
        /// The most value relayed, in wei.
        max: U256,
    },
    /// The contracts do not vouch for the target as a factory wallet.
    UnknownWallet,
    /// The target runs a release outside the configured supported set.
    UnsupportedRelease {
        /// The target's release version.
        version: u64,
    },
    /// The batch's deadline falls before the inclusion window ends.
    Expired,
    /// The batch's deadline is past the latest a quote signed now and `queue_wait`
    /// give.
    DeadlineTooFar {
        /// The latest deadline relayed, in unix seconds.
        max: u64,
    },
    /// The simulation reverted, decoded on the wallet route where it can be.
    Reverted {
        /// The decoded error.
        error: Option<WalletRegistryError>,
        /// The revert data.
        data: Bytes,
    },
    /// The request's worst loss, its value in full, is above what is left of the global
    /// loss budget.
    OverBudget {
        /// The request's worst loss.
        worst: U256,
        /// What is left of the global loss budget.
        left: U256,
    },
    /// The relayer's balance at its send does not cover its worst cost and value.
    RelayerUnderfunded {
        /// The balance at the pinned block.
        balance: U256,
        /// The send's worst cost plus its value.
        required: U256,
    },
    /// Another batch of the wallet's genesis identity is in flight.
    IdentityBusy {
        /// The request in flight.
        id: B256,
    },
    /// A batch of the wallet's genesis identity cost the relayer a loss.
    IdentityHeld {
        /// When the identity is served again, in unix seconds.
        until: u64,
    },
    /// Sending paused, for any reason `Relayer::paused` names.
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
            Self::SignInMissing => "SignInMissing",
            Self::RefundTooLow { .. } => "RefundTooLow",
            Self::GasLimitTooLow { .. } => "GasLimitTooLow",
            Self::GasLimitTooHigh { .. } => "GasLimitTooHigh",
            Self::TooLarge { .. } => "TooLarge",
            Self::ValueTooHigh { .. } => "ValueTooHigh",
            Self::UnknownWallet => "UnknownWallet",
            Self::UnsupportedRelease { .. } => "UnsupportedRelease",
            Self::Expired => "Expired",
            Self::DeadlineTooFar { .. } => "DeadlineTooFar",
            Self::Reverted { .. } => "Reverted",
            Self::OverBudget { .. } => "OverBudget",
            Self::RelayerUnderfunded { .. } => "RelayerUnderfunded",
            Self::IdentityBusy { .. } => "IdentityBusy",
            Self::IdentityHeld { .. } => "IdentityHeld",
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
        let (wallet, create, value) = (self.wallet, self.create, message.value);
        let unchecked = outer(config, wallet, create, &self.data, value);
        gas::under_max_size(config, &unchecked)?;
        let refund = refund(&executions, config.relayer).ok_or(Refusal::RefundMissing)?;
        let call = target(provider, config, wallet, create, &self.data, value);
        let identity = genesis(provider, wallet, create);
        let signed_in = signed_in(provider, wallet, &executions);
        // The target refuses before the sign-in.
        let target = async {
            let (call, identity, signed_in) = tokio::join!(call, identity, signed_in);
            let call = call?;
            signed_in?;
            Ok((call, identity?))
        };
        // Only the target and the sign-in refuse for the request; the rest fail as the node.
        let (header, (call, identity), tip, l1_fee_bound) = tokio::try_join!(
            gas::latest(provider),
            target,
            gas::tip(provider, config.tip_percentile),
            gas::l1_fee_bound(
                provider,
                config,
                &unchecked,
                self.gas_limit,
                BlockId::latest()
            ),
        )?;
        let (window, deadline) = (config.inclusion_window, message.deadline.to());
        let late = covered(deadline, header.timestamp.saturating_add(window.as_secs()))?;
        let slack = DEADLINE_SLACK * config.block_time;
        let wait = config.quote_validity + config.queue_wait + slack;
        let max = late + wait.as_secs_f64().ceil() as u64;
        if deadline > max {
            return Err(Refusal::DeadlineTooFar { max });
        }
        let head = gas::Head::of(config, &header)?;
        let cost = gas::cost(self.gas_limit, head.floor, l1_fee_bound);
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
        let at_cap = |cap: u128| {
            call.clone()
                .with_gas_limit(limit)
                .with_max_fee_per_gas(cap)
                .with_max_priority_fee_per_gas(tip.min(cap))
                .with_chain_id(config.chain_id)
        };
        // Run at the cap a send uses; queued at the cap from R.
        let sent = cap.min(head.fee_bound(config));
        simulate(provider, &at_cap(sent), None, late).await?;
        let worst = gas::cost(limit, sent, l1_fee_bound) + value;
        let tx = at_cap(cap);
        Ok(Checked {
            tx,
            net,
            l1_fee_bound,
            identity,
            worst,
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

/// `SignInMissing` unless the batch signs its wallet in: a top-level call of the wallet,
/// itself or `address(0)`, to `addSessionByLogin`, or to `linkIdentity` while the
/// wallet has no code or never had a session. A login runs only on a fresh proof.
pub(crate) async fn signed_in<P: Provider>(
    provider: &P,
    wallet: Address,
    executions: &[Execution],
) -> Result<(), Refusal> {
    let own = executions
        .iter()
        .filter(|e| e.target == wallet || e.target == Address::ZERO)
        .filter_map(|e| e.callData.get(..4));
    let selectors: Vec<&[u8]> = own.collect();
    let calls = |selector: [u8; 4]| selectors.contains(&&selector[..]);
    if calls(IWebWallet::addSessionByLoginCall::SELECTOR) {
        return Ok(());
    }
    if !calls(IWebWallet::linkIdentityCall::SELECTOR) {
        return Err(Refusal::SignInMissing);
    }
    if provider.get_code_at(wallet).await.map_err(node)?.is_empty() {
        return Ok(());
    }
    let wallet = IWebWallet::new(wallet, provider);
    match wallet.hadSession().call().await.map_err(node)? {
        false => Ok(()),
        true => Err(Refusal::SignInMissing),
    }
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
    let required = gas::limit(&sim, 0, call_gas, create);
    if required > priced_for {
        let required = gas::limit(&sim, fillable, call_gas, create);
        return Err(Refusal::GasLimitTooLow { required });
    }
    Ok(required)
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
/// vouch for the wallet on a supported release.
pub(crate) async fn target<P: Provider>(
    provider: &P,
    config: &Config,
    wallet: Address,
    create: Option<Create>,
    data: &Bytes,
    value: U256,
) -> Result<TransactionRequest, Refusal> {
    let created = async {
        let Some(c) = create else {
            return Ok(());
        };
        let factory = WalletFactory::new(config.factory, provider);
        let address = factory.walletAddress(c.platform_id, c.id_node, c.salt);
        match address.call().await.map_err(node)? == wallet {
            true => Ok(()),
            false => Err(Refusal::UnknownWallet),
        }
    };
    let vouched = vouched(provider, config, wallet, create.is_some());
    tokio::try_join!(created, vouched)?;
    Ok(outer(config, wallet, create, data, value))
}

/// The genesis identity of a wallet [`target`] vouched for: `create`'s, or read from
/// the wallet.
async fn genesis<P: Provider>(
    provider: &P,
    wallet: Address,
    create: Option<Create>,
) -> Result<Identity, Refusal> {
    if let Some(c) = create {
        return Ok((c.platform_id, c.id_node));
    }
    let wallet = IWebWallet::new(wallet, provider);
    let genesis = wallet.genesisIdentity().call().await.map_err(node)?;
    Ok((genesis.platformId, genesis.idNode))
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

/// The contracts vouch for `wallet` as a factory wallet on a supported release: on the
/// create path with no code yet, the factory's latest. An EIP-7702 account is not one.
pub(crate) async fn vouched<P: Provider>(
    provider: &P,
    config: &Config,
    wallet: Address,
    create: bool,
) -> Result<(), Refusal> {
    let new = create && provider.get_code_at(wallet).await.map_err(node)?.is_empty();
    let version = if new {
        let factory = WalletFactory::new(config.factory, provider);
        factory.latest().call().await.map_err(node)?.version
    } else {
        // No forwarder: a delegation to the zero address leaves no code to match.
        let vouched = check_target(provider, config.factory, Address::ZERO, wallet);
        match vouched.await.map_err(node)? {
            Some(Target::FactoryWallet(release)) => release.version,
            Some(Target::Account(_)) | None => return Err(Refusal::UnknownWallet),
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
    let call = |overrides| async move {
        let call = provider.call(tx.clone()).block_opt(block);
        call.with_block_overrides_opt(overrides)
            .await
            .map_err(refusal)
    };
    tokio::try_join!(call(None), call(Some(at_late)))?;
    Ok(())
}

/// What a `Node` refusal of a node's error response starts with.
const ANSWERED: &str = "the node answered: ";

/// A revert, a node's error response, or another node error.
pub(crate) fn refusal(e: TransportError) -> Refusal {
    match e.as_error_resp() {
        Some(resp) if resp.code == 3 => {
            reverted(resp.as_revert_data().unwrap_or_default())
        }
        Some(resp) => {
            Refusal::Node(format!("{ANSWERED}{} ({})", resp.message, resp.code))
        }
        _ => node(e),
    }
}

/// The message of the node's error response a `Node` refusal's text carries, if any.
pub(crate) fn answered(e: &str) -> Option<&str> {
    e.strip_prefix(ANSWERED)
}

/// A revert with `data`, decoded on the wallet route where it can be.
pub(crate) fn reverted(data: Bytes) -> Refusal {
    let error = WalletRegistryError::decode(&data);
    Refusal::Reverted { error, data }
}

pub(crate) fn node(e: impl Display) -> Refusal {
    Refusal::Node(e.to_string())
}
