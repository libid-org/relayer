//! The quote: what a client signs, from one simulation of its draft.

use alloy::{
    primitives::{
        Address,
        Bytes,
        U256,
    },
    providers::Provider,
    sol_types::SolError,
};
use libid_contracts::{
    bindings::wallet::{
        ExecuteAuthorization,
        ExecuteMessage,
        IWebWallet,
    },
    gas::{
        KeyKind,
        WalletGas,
    },
};
use serde::Deserialize;

use crate::{
    check::{
        decode,
        outer,
        refunds,
        reverted,
        target,
        under_max,
        under_max_value,
        Create,
        Refusal,
    },
    config::Config,
    gas::{
        self,
        NONZERO_BYTE,
    },
};

/// A call's extra gas for sending value (`CALL`'s value transfer).
const CALL_VALUE: u64 = 9_000;

/// A batch to quote: its data carries a well-formed dummy signature for every signer.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Draft {
    /// The wallet that runs the batch.
    pub wallet: Address,
    /// Set when the wallet is created in the same transaction.
    #[serde(default)]
    pub create: Option<Create>,
    /// `execute(AUTHORIZED_MODE, data)`'s `data`, with dummy signatures.
    pub data: Bytes,
    /// The client's hint that its signers fall short of quorum: the rules check runs.
    #[serde(default)]
    pub below_quorum: bool,
}

/// What the client signs: `call_gas`, and a refund of at least `value + min_refund`
/// for a send at `gas_limit`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quote {
    /// The gas the wallet reserves for each call: the simulation's gas used.
    pub call_gas: u64,
    /// The gas limit the refund is priced for.
    pub gas_limit: u64,
    /// The fee cap the refund is priced at: the floor, the base fee grown by its most
    /// until `valid_until` and over the inclusion window, plus the tip, but no more than
    /// a send's cap, M × the base fee grown until `valid_until`, unless that is under
    /// the floor.
    pub max_fee_per_gas: u128,
    /// The L1 fee bound with its margin.
    pub l1_fee_bound: U256,
    /// `gas_limit × max_fee_per_gas + l1_fee_bound`.
    pub min_refund: U256,
    /// The latest block time the quote is priced for, in Unix seconds.
    pub valid_until: u64,
}

impl Draft {
    /// Quote this draft against `config` on the chain `provider` reads.
    pub async fn quote<P: Provider>(
        &self,
        provider: &P,
        config: &Config,
    ) -> Result<Quote, Refusal> {
        let (message, authorization, executions) =
            decode(&self.data).ok_or(Refusal::NoncanonicalData)?;
        under_max_value(message.value, config)?;
        let (wallet, create) = (self.wallet, self.create);
        gas::under_max_size(
            config,
            &outer(config, wallet, create, &self.data, message.value),
        )?;
        let call = target(provider, config, wallet, create, &self.data, message.value);
        let (call, _) = call.await?;
        // After the latest block: nodes refuse a simulated block that is not.
        let now = gas::latest_time(provider).await?;
        let late = now + config.inclusion_window.as_secs();
        let sim = gas::simulate(provider, &call, late).await?;
        // Every call ran; only the dummy signatures failed.
        let unsigned = sim.return_data[..] == IWebWallet::ExecutionNotSigned::SELECTOR;
        if !sim.status && !unsigned {
            return Err(reverted(sim.return_data));
        }
        let call_gas = sim.gas_used;
        let below_quorum = self.below_quorum.then_some(executions.len());
        let refunds = refunds(&executions, config.relayer);
        if refunds.clone().next().is_none() {
            return Err(Refusal::RefundMissing);
        }
        let unmeasured =
            unmeasured(&message, &authorization, refunds, call_gas, below_quorum);
        let gas_limit = gas::limit(sim.gas_used + unmeasured, call_gas, create.is_some());
        under_max(gas_limit, config)?;
        let growth = config.quote_validity + config.inclusion_window;
        let floor = gas::fee_floor(provider, config, growth).await?;
        let grown = gas::fee_floor(provider, config, config.quote_validity).await?;
        let cap = grown.saturating_mul(config.max_fee_percent.into()) / 100;
        let tip = gas::tip(provider, config.tip_percentile).await?;
        let max_fee_per_gas = (floor + tip).min(cap).max(floor);
        let priced = gas::price(provider, config, &call, gas_limit, max_fee_per_gas);
        let (l1_fee_bound, min_refund) = priced.await?;
        Ok(Quote {
            call_gas,
            gas_limit,
            max_fee_per_gas,
            l1_fee_bound,
            min_refund,
            valid_until: now + config.quote_validity.as_secs(),
        })
    }
}

/// The gas the signed batch uses beyond its draft's run: the signatures the draft
/// did not check, the rest of a valid `_verify`, the signed `call_gas` word, the
/// zero bytes the client fills as nonzero ([`gas::fillable`]), a value transfer for
/// a refund drafted at 0, and, below quorum, the rules check of that many calls.
fn unmeasured(
    message: &ExecuteMessage,
    authorization: &ExecuteAuthorization,
    refunds: impl Iterator<Item = U256> + Clone,
    call_gas: u64,
    below_quorum: Option<usize>,
) -> u64 {
    let costs = WalletGas::vendored();
    // `_verify` stops at the first failing signature, so it checked only the
    // first, and in full only an ECDSA one. A longer key is priced as WebAuthn,
    // the dearer kind.
    let signatures = authorization.signatures.iter().enumerate();
    let unchecked = signatures.map(|(i, s)| match (i, s.key.len()) {
        (0, 20) => 0,
        (_, 20) => costs.verify(KeyKind::Ecdsa),
        _ => costs.verify(KeyKind::WebAuthn),
    });
    // The draft signs `callGas = 0`; the signed data carries `call_gas`.
    let call_gas_bytes = u64::from(u64::BITS - call_gas.leading_zeros()).div_ceil(8);
    let transfers = refunds.clone().filter(|r| r.is_zero()).count() as u64 * CALL_VALUE;
    let rules = below_quorum.map_or(0, |n| costs.rules(n as u64));
    costs.verify_tail()
        + NONZERO_BYTE * call_gas_bytes
        + gas::fillable(message, authorization, refunds)
        + unchecked.sum::<u64>()
        + rules
        + transfers
}
