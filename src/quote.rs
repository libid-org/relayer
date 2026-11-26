//! The quote: what a client signs, from one simulation of its draft.

use alloy::{
    eips::BlockId,
    network::TransactionBuilder,
    primitives::{
        Address,
        Bytes,
        U256,
    },
    providers::Provider,
    rpc::types::TransactionRequest,
    sol_types::{
        SolError,
        SolValue,
    },
};
use libid_contracts::{
    bindings::wallet::{
        encode_executions,
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

/// The most calls a batch may have for each call's gas to be measured.
const PREFIXES: usize = 8;

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
    /// The gas the wallet reserves for each call: what its dearest call used and a
    /// quarter, at most the whole run's gas used.
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
        let (sim, cut) = tokio::join!(
            gas::simulate(provider, &call, late),
            self.call_gas(provider, config, parts, &executions, late),
        );
        let sim = sim?;
        // Every call ran; only the dummy signatures failed.
        let unsigned = sim.return_data[..] == IWebWallet::ExecutionNotSigned::SELECTOR;
        if !sim.status && !unsigned {
            return Err(reverted(sim.return_data));
        }
        let call_gas = cut.unwrap_or(sim.gas_used).min(sim.gas_used);
        let below_quorum = self.below_quorum.then_some(executions.len());
        let refunds = refunds(&executions, config.relayer);
        if refunds.clone().next().is_none() {
            return Err(Refusal::RefundMissing);
        }
        let unmeasured =
            unmeasured(&message, &authorization, refunds, call_gas, below_quorum);
        let gas_limit = gas::limit(&sim, unmeasured, call_gas, create.is_some());
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

    /// 1.25 × the gas of the batch's [`dearest`] call. Each cut is run in one block at
    /// `time`. `None` with over [`PREFIXES`] calls, or a cut that does not run to the
    /// dummy signatures.
    async fn call_gas<P: Provider>(
        &self,
        provider: &P,
        config: &Config,
        (message, authorization): (&ExecuteMessage, &ExecuteAuthorization),
        executions: &[Execution],
        time: u64,
    ) -> Option<u64> {
        if executions.len() > PREFIXES {
            return None;
        }
        let cut = |k: usize| {
            let message = ExecuteMessage {
                execution_calldata: encode_executions(&executions[..k]),
                ..message.clone()
            };
            let data = message.authorized_data(authorization.clone());
            outer(config, self.wallet, self.create, &data, message.value)
        };
        let cuts: Vec<_> = (0..=executions.len()).map(cut).collect();
        let input =
            |c: &TransactionRequest| c.input().map(|i| floor(i)).unwrap_or_default();
        let floors: Vec<_> = cuts.iter().map(input).collect();
        let runs = gas::simulate_all(provider, cuts, time).await.ok()?;
        let signed = IWebWallet::ExecutionNotSigned::SELECTOR;
        let ran = |r: &alloy::rpc::types::simulate::SimCallResult| {
            !r.status && r.return_data[..] == signed
        };
        if runs.len() != executions.len() + 1 || !runs.iter().all(ran) {
            return None;
        }
        let used: Vec<_> = runs.iter().map(|r| r.gas_used).collect();
        let calls = executions.iter().map(|e| calldata_gas(&e.abi_encode()));
        Some((dearest(&used, &floors, calls)? * 5).div_ceil(4))
    }
}

/// The gas of the dearest call: the cut after it `used` less the cut before it and its
/// calldata gas `calls`. `None` when a cut used at most its EIP-7623 floor in `floors`,
/// which hides its execution.
fn dearest(
    used: &[u64],
    floors: &[u64],
    calls: impl Iterator<Item = u64>,
) -> Option<u64> {
    if used.iter().zip(floors).any(|(used, floor)| used <= floor) {
        return None;
    }
    let each = used.windows(2).zip(calls);
    let each =
        each.map(|(used, call)| used[1].saturating_sub(used[0]).saturating_sub(call));
    Some(each.max().unwrap_or_default())
}

/// The EIP-7623 floor of a transaction with `input`: 21000 and 10 a token, a zero byte
/// one token and any other four.
fn floor(input: &[u8]) -> u64 {
    let zeros = input.iter().filter(|&&b| b == 0).count() as u64;
    21_000 + 10 * (zeros + 4 * (input.len() as u64 - zeros))
}

/// The calldata gas of `bytes` (EIP-2028).
fn calldata_gas(bytes: &[u8]) -> u64 {
    let zeros = bytes.iter().filter(|&&b| b == 0).count() as u64;
    zeros * gas::ZERO_BYTE
        + (bytes.len() as u64 - zeros) * (gas::ZERO_BYTE + NONZERO_BYTE)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cut_at_its_calldata_floor_falls_back_to_the_whole_run() {
        // 21000, and 40 a nonzero byte, 10 a zero one.
        assert_eq!(floor(&[1; 100]), 25_000);
        assert_eq!(floor(&[0; 1000]), 31_000);
        let calls = || [10, 10].into_iter();
        assert_eq!(
            dearest(&[50_000, 80_000, 90_000], &[40_000; 3], calls()),
            Some(29_990)
        );
        // The last cut billed at its floor: call 2's 10_000 shows, not its execution.
        let floors = [40_000, 40_000, 90_000];
        assert_eq!(dearest(&[50_000, 80_000, 90_000], &floors, calls()), None);
    }
}
