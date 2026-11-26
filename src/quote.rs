//! The quote: what a client signs, from one simulation of its draft.

use alloy::{
    eips::BlockId,
    primitives::{
        Address,
        Bytes,
        U256,
    },
    providers::Provider,
    sol_types::{
        SolError,
        SolValue,
    },
};
use libid_contracts::{
    bindings::wallet::{
        ExecuteAuthorization,
        ExecuteMessage,
        Execution,
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
        self,
        before,
        cuts,
        decode,
        outer,
        refunds,
        reverted,
        signed_in,
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Quote {
    /// The least gas the wallet gives each call, one floor per call: what it used and a
    /// quarter, at most the whole run's gas used; each the whole run's when the cuts
    /// show no call's own gas.
    pub call_gas: Vec<u64>,
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
        // Only the target, then the sign-in, refuse for the draft; the rest fail as the
        // node.
        let target = async {
            let value = message.value;
            let target = target(provider, config, wallet, create, &self.data, value);
            let (call, signed_in) =
                tokio::join!(target, signed_in(provider, wallet, &executions));
            let call = call?;
            signed_in.map(|()| call)
        };
        let (call, header, tip) = tokio::try_join!(
            target,
            gas::latest(provider),
            gas::tip(provider, config.tip_percentile),
        )?;
        // After the latest block: nodes refuse a simulated block that is not.
        let now = header.timestamp;
        let late = now + config.inclusion_window.as_secs();
        let parts = (&message, &authorization);
        let (sim, cuts) = tokio::join!(
            gas::simulate(provider, &call, late),
            cuts(provider, config, (wallet, create), parts, &executions, late),
        );
        let sim = sim?;
        // Every call ran; only the dummy signatures failed.
        let unsigned = sim.return_data[..] == IWebWallet::ExecutionNotSigned::SELECTOR;
        if !sim.status && !unsigned {
            return Err(reverted(sim.return_data));
        }
        let each = cuts.as_deref().and_then(|cuts| each(cuts, &executions));
        let each = each.unwrap_or_else(|| vec![sim.gas_used; executions.len()]);
        let call_gas: Vec<_> = each.into_iter().map(|g| g.min(sim.gas_used)).collect();
        let before = cuts.map(|cuts| before(&cuts, &executions));
        let below_quorum = self.below_quorum.then_some(executions.len());
        let refunds = refunds(&executions, config.relayer);
        if refunds.clone().next().is_none() {
            return Err(Refusal::RefundMissing);
        }
        let unmeasured =
            unmeasured(&message, &authorization, refunds, &call_gas, below_quorum);
        let (floors, create) = ((&call_gas[..], before.as_deref()), create.is_some());
        let gas_limit = gas::limit(&sim, unmeasured, floors.0, floors.1, create);
        under_max(gas_limit, config)?;
        let growth = config.quote_validity + config.inclusion_window;
        let floor = gas::grown(config, &header, growth)?;
        let grown = gas::grown(config, &header, config.quote_validity)?;
        let cap = grown.saturating_mul(config.max_fee_percent.into()) / 100;
        let max_fee_per_gas = (floor + tip).min(cap).max(floor);
        let latest = BlockId::latest();
        let l1 = gas::l1_fee_bound(provider, config, &call, gas_limit, latest);
        let l1_fee_bound = l1.await?;
        let min_refund = gas::cost(gas_limit, max_fee_per_gas, l1_fee_bound);
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

/// 1.25 × the gas of each call: the cut after it less the cut before it and its
/// calldata gas. `None` when a cut used at most its EIP-7623 floor, which hides its
/// execution.
fn each(cuts: &[check::Cut], executions: &[Execution]) -> Option<Vec<u64>> {
    if cuts.iter().any(|(used, floor)| used <= floor) {
        return None;
    }
    let calls = executions
        .iter()
        .map(|e| gas::calldata_gas(&e.abi_encode()));
    let each = cuts.windows(2).zip(calls);
    let each = each.map(|(c, call)| c[1].0.saturating_sub(c[0].0).saturating_sub(call));
    Some(each.map(|g| (g * 5).div_ceil(4)).collect())
}

/// The gas the signed batch uses beyond its draft's run: the signatures the draft
/// did not check, the rest of a valid `_verify`, the signed `call_gas` floors, the
/// zero bytes the client fills as nonzero ([`gas::fillable`]), a value transfer for
/// a refund drafted at 0, and, below quorum, the rules check of that many calls.
fn unmeasured(
    message: &ExecuteMessage,
    authorization: &ExecuteAuthorization,
    refunds: impl Iterator<Item = U256> + Clone,
    call_gas: &[u64],
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
    // The draft signs a zero floor per call; the signed data carries `call_gas`.
    let bytes = |g: &u64| u64::from(u64::BITS - g.leading_zeros()).div_ceil(8);
    let call_gas_bytes = call_gas.iter().map(bytes).sum::<u64>();
    let transfers = refunds.clone().filter(|r| r.is_zero()).count() as u64 * CALL_VALUE;
    let rules = below_quorum.map_or(0, |n| costs.rules(n as u64));
    costs.verify_tail()
        + NONZERO_BYTE * call_gas_bytes
        + gas::fillable(message, authorization, refunds)
        + unchecked.sum::<u64>()
        + rules
        + transfers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cut_at_its_calldata_floor_falls_back_to_the_whole_run() {
        // 21000, and 40 a nonzero byte, 10 a zero one.
        assert_eq!(gas::floor(&[1; 100]), 25_000);
        assert_eq!(gas::floor(&[0; 1000]), 31_000);
        let calls = [Execution::default(), Execution::default()];
        let call = gas::calldata_gas(&calls[0].abi_encode());
        let cuts = [(50_000, 40_000), (80_000, 40_000), (90_000, 40_000)];
        let quarter = |g: u64| (g * 5).div_ceil(4);
        let each_call = vec![quarter(30_000 - call), quarter(10_000 - call)];
        assert_eq!(each(&cuts, &calls), Some(each_call));
        // The last cut billed at its floor: call 2's 10_000 shows, not its execution.
        let cuts = [(50_000, 40_000), (80_000, 40_000), (90_000, 90_000)];
        assert_eq!(each(&cuts, &calls), None);
    }
}
