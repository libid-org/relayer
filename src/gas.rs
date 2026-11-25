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

/// Calldata gas per zero byte (EIP-2028), the least a byte costs.
pub(crate) const ZERO_BYTE: u64 = 4;

/// The most bytes of a signed transaction a node's pool accepts: geth's, op-geth's and
/// reth's 128 KiB.
pub(crate) const MAX_TX_BYTES: u64 = 128 * 1024;

/// The most bytes a signature adds to an unsigned EIP-1559 transaction: its y parity,
/// two 32-byte words with their RLP headers, and the list header's growth.
const SIGNATURE_BYTES: usize = 1 + 33 + 33 + 2;

/// The blocks of fee history a tip is read over.
const TIP_BLOCKS: u64 = 10;

/// The OP-stack `GasPriceOracle` predeploy.
const GAS_PRICE_ORACLE: Address = address!("420000000000000000000000000000000000000F");

/// The OP-stack `L1Block` predeploy.
const L1_BLOCK: Address = address!("4200000000000000000000000000000000000015");

alloy::sol! {
    #[sol(rpc)]
    interface GasPriceOracle {
        function getL1FeeUpperBound(uint256 unsignedTxSize) external view returns (uint256);
    }

    #[sol(rpc)]
    interface L1Block {
        function operatorFeeScalar() external view returns (uint32);
        function operatorFeeConstant() external view returns (uint64);
    }
}

/// The OP-stack operator fee, which no refund bounds.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OperatorFee {
    /// Off, or no L1 fee.
    Off,
    /// On, at its scalar and constant.
    On(u32, u64),
    /// No getters to read, as before Isthmus, whose `L1Block` reverts.
    Unread,
}

/// The OP-stack operator fee at `block`; off on a chain without an L1 fee.
pub(crate) async fn operator_fee<P: Provider>(
    provider: &P,
    config: &Config,
    block: BlockId,
) -> Result<OperatorFee, Refusal> {
    if config.l1_fee != L1Fee::OpStack {
        return Ok(OperatorFee::Off);
    }
    let l1_block = L1Block::new(L1_BLOCK, provider);
    let (scalar, constant) = (
        l1_block.operatorFeeScalar().call().block(block).await,
        l1_block.operatorFeeConstant().call().block(block).await,
    );
    match (scalar, constant) {
        (Ok(0), Ok(0)) => Ok(OperatorFee::Off),
        (Ok(s), Ok(c)) => Ok(OperatorFee::On(s, c)),
        (Err(e), _) | (_, Err(e)) if reverted(&e) => Ok(OperatorFee::Unread),
        (Err(e), _) | (_, Err(e)) => Err(node(e)),
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

/// The least gas a run is sent with above its peak: two storage slots that turn fresh
/// between the run and the block.
const PEAK_MARGIN: u128 = 50_000;

/// The gas limit of the run `sim` with `extra` gas more:
/// `max(ceil(1.05 × peak), peak + 50 000) + (callGas + CALL_OVERHEAD) × (64/63)^d`, the
/// peak being `maxUsedGas`, what the run needs before its refund (EIP-3529); from a
/// node that gives none, `ceil(1.25 × gasUsed)` instead. Each 64/63 is a frame's kept
/// 1/64: the wallet's check, and the delegatecall of the wallet proxy, so `d` is 2;
/// the create path adds the factory proxy's delegatecall and its call of the wallet,
/// so 4.
pub(crate) fn limit(sim: &SimCallResult, extra: u64, call_gas: u64, create: bool) -> u64 {
    let d = if create { 4 } else { 2 };
    let reserve = u128::from(call_gas) + u128::from(CALL_OVERHEAD);
    let reserve = (reserve * 64u128.pow(d)).div_ceil(63u128.pow(d));
    let run = match sim.max_used_gas {
        Some(peak) => {
            let peak = u128::from(peak) + u128::from(extra);
            (peak * 105).div_ceil(100).max(peak + PEAK_MARGIN)
        }
        None => (u128::from(sim.gas_used + extra) * 5).div_ceil(4),
    };
    u64::try_from(run + reserve).unwrap_or(u64::MAX)
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
    let latest = BlockId::latest();
    let l1_fee_bound = l1_fee_bound(provider, config, call, gas_limit, latest).await?;
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
    let base_fee = header.base_fee_per_gas.unwrap_or_default().into();
    Ok(rule(config, header)?.grow(base_fee, config.blocks(time)))
}

/// The latest block's base fee rule's most growth over the inclusion window, in
/// percent.
pub(crate) async fn window_growth<P: Provider>(
    provider: &P,
    config: &Config,
) -> Result<u64, Refusal> {
    let rule = rule(config, &latest(provider).await?)?;
    Ok(rule.growth_percent(config.blocks(config.inclusion_window)))
}

/// The base fee rule at `header`.
#[allow(clippy::result_large_err)] // the `Refusal` that its callers return
fn rule(config: &Config, header: &Header) -> Result<Eip1559, Refusal> {
    let extra = &header.extra_data;
    let unknown = || format!("extraData {extra} is no OP-stack fee rule; set fee_params");
    match config.fee_params {
        FeeParams::Fixed(rule) => Ok(rule),
        FeeParams::Header => {
            Eip1559::from_extra_data(extra).ok_or_else(|| node(unknown()))
        }
    }
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
/// empty, and not 0 as in an OP-stack block of only its deposit; at least 1 wei.
fn tip_of(history: &FeeHistory, percentile: f64) -> u128 {
    let blocks = history
        .gas_used_ratio
        .iter()
        .zip(history.reward.iter().flatten());
    let used = blocks.filter(|(ratio, _)| **ratio > 0.0);
    let tips = used.filter_map(|(_, t)| t.first().copied().filter(|t| *t > 0));
    let mut tips: Vec<u128> = tips.collect();
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

    /// M × its base fee, at least its floor: the most a send's fee cap is.
    pub(crate) fn fee_bound(&self, config: &Config) -> u128 {
        let m = config.max_fee_percent.into();
        (self.base_fee.saturating_mul(m) / 100).max(self.floor)
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
    let call = simulate_all(provider, vec![tx.clone()], time).await?;
    call.into_iter()
        .next()
        .ok_or_else(|| node("eth_simulateV1 returned no call"))
}

/// `txs` run in order with `eth_simulateV1` in one block after the latest, at `time`.
pub(crate) async fn simulate_all<P: Provider>(
    provider: &P,
    txs: Vec<TransactionRequest>,
    time: u64,
) -> Result<Vec<SimCallResult>, Refusal> {
    let payload = SimulatePayload {
        block_state_calls: vec![SimBlock {
            block_overrides: Some(BlockOverrides {
                time: Some(time),
                ..Default::default()
            }),
            calls: txs,
            ..Default::default()
        }],
        ..Default::default()
    };
    let blocks = provider.simulate(&payload).await.map_err(node)?;
    let calls = blocks
        .into_iter()
        .next()
        .map(|b| b.calls)
        .unwrap_or_default();
    Ok(calls
        .into_iter()
        .map(|mut call| {
            // Some nodes (anvil 1.8) return a revert's data only in `error.data`.
            if !call.status && call.return_data.is_empty() {
                if let Some(data) = call.error.as_ref().and_then(|e| e.data.clone()) {
                    call.return_data = data;
                }
            }
            call
        })
        .collect())
}

/// `k × getL1FeeUpperBound` of `call`'s unsigned size at `gas_limit`, read at
/// `block`: the L1 fee a refund covers.
pub(crate) async fn l1_fee_bound<P: Provider>(
    provider: &P,
    config: &Config,
    call: &TransactionRequest,
    gas_limit: u64,
    block: BlockId,
) -> Result<U256, Refusal> {
    if config.l1_fee == L1Fee::None {
        return Ok(U256::ZERO);
    }
    let size = unsigned_size(config, call, gas_limit)?;
    l1_bound(provider, config, U256::from(size), block).await
}

/// `call`'s unsigned EIP-1559 size at `gas_limit`, at the largest nonce and fees: at
/// least its size as sent.
#[allow(clippy::result_large_err)] // the `Refusal` that its callers return
pub(crate) fn unsigned_size(
    config: &Config,
    call: &TransactionRequest,
    gas_limit: u64,
) -> Result<usize, Refusal> {
    let tx = call
        .clone()
        .with_gas_limit(gas_limit)
        .with_chain_id(config.chain_id)
        .nonce(u64::MAX)
        .max_fee_per_gas(u128::MAX)
        .max_priority_fee_per_gas(u128::MAX);
    Ok(tx.build_1559().map_err(node)?.encoded_for_signing().len())
}

/// `call` signed at any gas limit, nonce and fees is at most [`MAX_TX_BYTES`].
#[allow(clippy::result_large_err)] // the `Refusal` that its callers return
pub(crate) fn under_max_size(
    config: &Config,
    call: &TransactionRequest,
) -> Result<(), Refusal> {
    let size = unsigned_size(config, call, u64::MAX)? + SIGNATURE_BYTES;
    let max = MAX_TX_BYTES;
    (size as u64 <= max)
        .then_some(())
        .ok_or(Refusal::TooLarge { max })
}

/// `k × getL1FeeUpperBound(size)` read at `block`; 0 on a chain without an L1 fee.
pub(crate) async fn l1_bound<P: Provider>(
    provider: &P,
    config: &Config,
    size: U256,
    block: BlockId,
) -> Result<U256, Refusal> {
    if config.l1_fee == L1Fee::None {
        return Ok(U256::ZERO);
    }
    let oracle = GasPriceOracle::new(GAS_PRICE_ORACLE, provider);
    let bound = oracle.getL1FeeUpperBound(size);
    let bound = bound.call().block(block).await.map_err(node)?;
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
    fn a_signature_adds_at_most_its_bytes() {
        use alloy::{
            consensus::TxEnvelope,
            eips::Encodable2718,
            primitives::Signature,
        };
        let config = Config::anvil(1, Address::ZERO, Address::ZERO);
        for len in [0, 55, 300, 70_000] {
            let call = TransactionRequest::default()
                .with_to(Address::ZERO)
                .with_input(vec![0xff; len]);
            let tx = call
                .clone()
                .with_gas_limit(u64::MAX)
                .with_chain_id(1)
                .nonce(u64::MAX)
                .max_fee_per_gas(u128::MAX)
                .max_priority_fee_per_gas(u128::MAX);
            let tx = tx.build_1559().unwrap();
            let word = U256::MAX;
            let signed =
                TxEnvelope::from(tx.into_signed(Signature::new(word, word, true)));
            let unsigned = unsigned_size(&config, &call, u64::MAX).unwrap();
            assert!(
                signed.encoded_2718().len() <= unsigned + SIGNATURE_BYTES,
                "{len}"
            );
        }
    }

    #[test]
    fn a_tip_skips_empty_blocks_and_is_at_least_one_wei() {
        let history = |blocks: &[(f64, u128)]| FeeHistory {
            gas_used_ratio: blocks.iter().map(|b| b.0).collect(),
            reward: Some(blocks.iter().map(|b| vec![b.1]).collect()),
            ..Default::default()
        };
        let busy = history(&[(0.3, 10), (0.6, 30), (0.0, 99), (0.9, 20), (0.1, 0)]);
        assert_eq!(tip_of(&busy, 75.0), 30);
        assert_eq!(tip_of(&busy, 50.0), 20);
        assert_eq!(tip_of(&history(&[(0.0, 9), (0.5, 0)]), 75.0), 1);
        assert_eq!(tip_of(&history(&[]), 75.0), 1);
    }
}
