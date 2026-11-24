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
        IWebWallet,
    },
    gas::{
        KeyKind,
        WalletGas,
    },
};

use crate::{
    check::{
        decode,
        refunds,
        reverted,
        target,
        Create,
        Refusal,
    },
    config::Config,
    gas,
};

/// Calldata gas per byte above a zero byte's (EIP-2028).
const NONZERO_BYTE: u64 = 12;

/// A batch to quote: its data carries a well-formed dummy signature for every signer.
#[derive(Clone, Debug)]
pub struct Draft {
    /// The wallet that runs the batch.
    pub wallet: Address,
    /// Set when the wallet is created in the same transaction.
    pub create: Option<Create>,
    /// `execute(AUTHORIZED_MODE, data)`'s `data`, with dummy signatures.
    pub data: Bytes,
    /// The client's hint that its signers fall short of quorum: the rules check runs.
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
    /// The fee cap the refund is priced at.
    pub max_fee_per_gas: u128,
    /// The L1 fee bound with its margin.
    pub l1_fee_bound: U256,
    /// `gas_limit × max_fee_per_gas + l1_fee_bound`.
    pub min_refund: U256,
}

/// Quote `draft` against `config` on the chain `provider` reads.
pub async fn quote<P: Provider>(
    provider: &P,
    config: &Config,
    draft: &Draft,
) -> Result<Quote, Refusal> {
    let (message, authorization, executions) =
        decode(&draft.data).ok_or(Refusal::NoncanonicalData)?;
    let (wallet, create) = (draft.wallet, draft.create);
    let call = target(provider, config, wallet, create, &draft.data, message.value);
    let call = call.await?;
    // After the latest block: nodes refuse a simulated block that is not.
    let late = gas::latest_time(provider).await? + config.inclusion_window.as_secs();
    let sim = gas::simulate(provider, &call, late).await?;
    // Every call ran; only the dummy signatures failed.
    let unsigned = sim.return_data[..] == IWebWallet::ExecutionNotSigned::SELECTOR;
    if !sim.status && !unsigned {
        return Err(reverted(sim.return_data));
    }
    let call_gas = sim.gas_used;
    let below_quorum = draft.below_quorum.then_some(executions.len());
    let refunds = refunds(&executions, config.relayer);
    let unmeasured = unmeasured(&authorization, refunds, call_gas, below_quorum);
    let gas_limit = gas::limit(sim.gas_used + unmeasured, call_gas, create.is_some());
    let (tx, l1_fee_bound, min_refund) =
        gas::price(provider, config, call, gas_limit).await?;
    Ok(Quote {
        call_gas,
        gas_limit,
        max_fee_per_gas: tx.max_fee_per_gas.unwrap_or_default(),
        l1_fee_bound,
        min_refund,
    })
}

/// The gas the signed batch uses beyond its draft's run: the signatures the draft
/// did not check, the rest of a valid `_verify`, the signed `call_gas` word, the
/// zero bytes of its signatures and `refunds` as nonzero, and, below quorum, the
/// rules check of that many calls.
fn unmeasured(
    authorization: &ExecuteAuthorization,
    refunds: impl Iterator<Item = U256>,
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
    // The client fills the signatures and may change R after the quote.
    let zeros = |b: &[u8]| b.iter().filter(|&&b| b == 0).count() as u64;
    let signed = authorization.signatures.iter().map(|s| zeros(&s.signature));
    let filled =
        signed.sum::<u64>() + refunds.map(|r| zeros(&r.to_be_bytes::<32>())).sum::<u64>();
    let rules = below_quorum.map_or(0, |n| costs.rules(n as u64));
    let bytes = call_gas_bytes + filled;
    costs.verify_tail() + NONZERO_BYTE * bytes + unchecked.sum::<u64>() + rules
}
