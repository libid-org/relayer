//! What a send needs in gas and in L1 fee, from one simulation.

use std::time::Duration;

use alloy::{
    consensus::SignableTransaction,
    eips::{
        BlockId,
        BlockNumberOrTag,
    },
    network::TransactionBuilder,
    primitives::{
        address,
        Address,
        B256,
        U256,
    },
    providers::Provider,
    rpc::types::{
        simulate::{
            SimBlock,
            SimCallResult,
            SimulatePayload,
        },
        BlockOverrides,
        FeeHistory,
        Header,
        TransactionRequest,
    },
};
use libid_contracts::{
    bindings::wallet::{
        ExecuteAuthorization,
        ExecuteMessage,
    },
    gas::CALL_OVERHEAD,
};

use crate::{
    check::{
        node,
        Refusal,
    },
    config::{
        Config,
        Eip1559,
        FeeParams,
        L1Fee,
    },
};

/// Calldata gas per byte above a zero byte's (EIP-2028).
pub(crate) const NONZERO_BYTE: u64 = 12;

/// The blocks of fee history a tip is read over.
const TIP_BLOCKS: u64 = 10;

/// The OP-stack `GasPriceOracle` predeploy.
const GAS_PRICE_ORACLE: Address = address!("420000000000000000000000000000000000000F");

alloy::sol! {
    #[sol(rpc)]
    interface GasPriceOracle {
        function getL1FeeUpperBound(uint256 unsignedTxSize) external view returns (uint256);
    }
}

/// `ceil(1.25 × gas) + (callGas + CALL_OVERHEAD) × (64/63)^d`. The 1.25 is the gas
/// a transaction needs while it runs above its gas used after the refund (EIP-3529).
/// Each 64/63 is a frame's kept 1/64: the wallet's check, and the delegatecall of the
/// wallet proxy or 7702 forwarder, so `d` is 2; the create path adds the factory
/// proxy's delegatecall and its call of the wallet, so 4.
pub(crate) fn limit(gas: u64, call_gas: u64, create: bool) -> u64 {
    let d = if create { 4 } else { 2 };
    let reserve = u128::from(call_gas) + u128::from(CALL_OVERHEAD);
    let reserve = (reserve * 64u128.pow(d)).div_ceil(63u128.pow(d));
    let run = u128::from(gas) * 5;
    u64::try_from(run.div_ceil(4) + reserve).unwrap_or(u64::MAX)
}

/// The gas of the zero bytes a client fills when it signs, priced as nonzero: the
/// signatures, `refunds`, and the six bytes of `at` and of the deadline.
pub(crate) fn fillable(
    message: &ExecuteMessage,
    authorization: &ExecuteAuthorization,
    refunds: impl Iterator<Item = U256>,
) -> u64 {
    let zeros = |b: &[u8]| b.iter().filter(|&&b| b == 0).count() as u64;
    let signed = authorization.signatures.iter().map(|s| zeros(&s.signature));
    let refunds = refunds.map(|r| zeros(&r.to_be_bytes::<32>()));
    let stamps = [message.at, message.deadline].map(|t| zeros(&t.to_be_bytes::<6>()));
    let stamps = stamps.iter().sum::<u64>();
    NONZERO_BYTE * (signed.sum::<u64>() + refunds.sum::<u64>() + stamps)
}

/// `call`'s L1 fee bound at `gas_limit`, and the most it costs the relayer at
/// `max_fee_per_gas`: `gas_limit × max fee + L1 bound`.
pub(crate) async fn price<P: Provider>(
    provider: &P,
    config: &Config,
    call: &TransactionRequest,
    gas_limit: u64,
    max_fee_per_gas: u128,
) -> Result<(U256, U256), Refusal> {
    let l1_fee_bound = l1_fee_bound(provider, config, call, gas_limit).await?;
    let cost = U256::from(gas_limit)
        .saturating_mul(U256::from(max_fee_per_gas))
        .saturating_add(l1_fee_bound);
    Ok((l1_fee_bound, cost))
}

/// The latest base fee, at least the chain's minimum, grown by its most over
/// `time`'s blocks.
pub(crate) async fn fee_floor<P: Provider>(
    provider: &P,
    config: &Config,
    time: Duration,
) -> Result<u128, Refusal> {
    grown(config, &latest(provider).await?, time)
}

/// `header`'s base fee, at least the chain's minimum, grown by its most over
/// `time`'s blocks.
#[allow(clippy::result_large_err)] // the `Refusal` that its callers return
fn grown(config: &Config, header: &Header, time: Duration) -> Result<u128, Refusal> {
    let extra = &header.extra_data;
    let unknown = || format!("extraData {extra} is no OP-stack fee rule; set fee_params");
    let rule = match config.fee_params {
        FeeParams::Fixed(rule) => rule,
        FeeParams::Header => {
            Eip1559::from_extra_data(extra).ok_or_else(|| node(unknown()))?
        }
    };
    let base_fee = header.base_fee_per_gas.unwrap_or_default().into();
    Ok(rule.grow(base_fee, config.blocks(time)))
}

/// The tip at `percentile` over the last [`TIP_BLOCKS`], from `eth_feeHistory`.
pub(crate) async fn tip<P: Provider>(
    provider: &P,
    percentile: f64,
) -> Result<u128, Refusal> {
    let percentiles = [percentile];
    let history =
        provider.get_fee_history(TIP_BLOCKS, BlockNumberOrTag::Latest, &percentiles);
    Ok(tip_of(&history.await.map_err(node)?, percentile))
}

/// `percentile` of the tips at `percentile` of `history`'s blocks that are not
/// empty; at least 1 wei.
fn tip_of(history: &FeeHistory, percentile: f64) -> u128 {
    let blocks = history
        .gas_used_ratio
        .iter()
        .zip(history.reward.iter().flatten());
    let used = blocks.filter(|(ratio, _)| **ratio > 0.0);
    let mut tips: Vec<u128> = used.filter_map(|(_, t)| t.first().copied()).collect();
    tips.sort_unstable();
    let at = tips.len().saturating_sub(1) as f64 * percentile / 100.0;
    tips.get(at.round() as usize)
        .copied()
        .unwrap_or_default()
        .max(1)
}

/// The latest block's timestamp.
pub(crate) async fn latest_time<P: Provider>(provider: &P) -> Result<u64, Refusal> {
    Ok(latest(provider).await?.timestamp)
}

/// The block a send is pinned to: its nonce, balance and runs are read at `hash`.
pub(crate) struct Head {
    pub(crate) hash: B256,
    pub(crate) number: u64,
    pub(crate) time: u64,
    pub(crate) base_fee: u128,
    /// Its base fee grown over the inclusion window: the least cap a send runs at.
    pub(crate) floor: u128,
}

impl Head {
    /// Its block, which a read fails at once it leaves the canonical chain.
    pub(crate) fn at(&self) -> BlockId {
        BlockId::hash_canonical(self.hash)
    }

    /// M × its base fee: the most a send's fee cap is.
    pub(crate) fn fee_bound(&self, config: &Config) -> u128 {
        let m = config.max_fee_percent.into();
        self.base_fee.saturating_mul(m) / 100
    }
}

/// The latest block, to pin.
pub(crate) async fn head<P: Provider>(
    provider: &P,
    config: &Config,
) -> Result<Head, Refusal> {
    let latest = latest(provider).await?;
    Ok(Head {
        hash: latest.hash,
        number: latest.number,
        time: latest.timestamp,
        base_fee: latest.base_fee_per_gas.unwrap_or_default().into(),
        floor: grown(config, &latest, config.inclusion_window)?,
    })
}

async fn latest<P: Provider>(provider: &P) -> Result<Header, Refusal> {
    let latest = provider.get_block_by_number(BlockNumberOrTag::Latest);
    let latest = latest.await.map_err(node)?;
    Ok(latest.ok_or_else(|| node("no latest block"))?.header)
}

/// `tx` run once with `eth_simulateV1` after the latest block, in a block at `time`.
pub(crate) async fn simulate<P: Provider>(
    provider: &P,
    tx: &TransactionRequest,
    time: u64,
) -> Result<SimCallResult, Refusal> {
    let payload = SimulatePayload {
        block_state_calls: vec![SimBlock {
            block_overrides: Some(BlockOverrides {
                time: Some(time),
                ..Default::default()
            }),
            calls: vec![tx.clone()],
            ..Default::default()
        }],
        ..Default::default()
    };
    let blocks = provider.simulate(&payload).await.map_err(node)?;
    let call = blocks
        .into_iter()
        .next()
        .and_then(|b| b.calls.into_iter().next());
    let mut call = call.ok_or_else(|| node("eth_simulateV1 returned no call"))?;
    // Some nodes (anvil 1.8) return a revert's data only in `error.data`.
    if !call.status && call.return_data.is_empty() {
        if let Some(data) = call.error.as_ref().and_then(|e| e.data.clone()) {
            call.return_data = data;
        }
    }
    Ok(call)
}

/// `k × getL1FeeUpperBound` of `call`'s unsigned size at `gas_limit`: the L1 fee a
/// refund covers.
async fn l1_fee_bound<P: Provider>(
    provider: &P,
    config: &Config,
    call: &TransactionRequest,
    gas_limit: u64,
) -> Result<U256, Refusal> {
    if config.l1_fee == L1Fee::None {
        return Ok(U256::ZERO);
    }
    // The nonce and fees are set at send; the largest ones size the bound.
    let tx = call
        .clone()
        .with_gas_limit(gas_limit)
        .with_chain_id(config.chain_id)
        .nonce(u64::MAX)
        .max_fee_per_gas(u128::MAX)
        .max_priority_fee_per_gas(u128::MAX);
    let size = tx.build_1559().map_err(node)?.encoded_for_signing().len();
    let oracle = GasPriceOracle::new(GAS_PRICE_ORACLE, provider);
    let bound = oracle.getL1FeeUpperBound(U256::from(size));
    let bound = bound.call().await.map_err(node)?;
    Ok(bound.saturating_mul(U256::from(config.l1_margin)) / U256::from(100))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pinned_read_requires_the_canonical_block() {
        let head = Head {
            hash: B256::ZERO,
            number: 0,
            time: 0,
            base_fee: 0,
            floor: 0,
        };
        let id = serde_json::to_value(head.at()).unwrap();
        assert_eq!(id["requireCanonical"], true, "{id}");
    }

    #[test]
    fn a_tip_skips_empty_blocks_and_is_at_least_one_wei() {
        let history = |blocks: &[(f64, u128)]| FeeHistory {
            gas_used_ratio: blocks.iter().map(|b| b.0).collect(),
            reward: Some(blocks.iter().map(|b| vec![b.1]).collect()),
            ..Default::default()
        };
        let busy = history(&[(0.3, 10), (0.6, 30), (0.0, 99), (0.9, 20)]);
        assert_eq!(tip_of(&busy, 75.0), 30);
        assert_eq!(tip_of(&busy, 50.0), 20);
        assert_eq!(tip_of(&history(&[(0.0, 9), (0.5, 0)]), 75.0), 1);
        assert_eq!(tip_of(&history(&[]), 75.0), 1);
    }
}
