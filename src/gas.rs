//! What a send needs in gas and in L1 fee, from one simulation.

use std::time::Duration;

use alloy::{
    consensus::SignableTransaction,
    eips::BlockNumberOrTag,
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
    let latest = latest(provider).await?;
    let extra = &latest.extra_data;
    let unknown = || format!("extraData {extra} is no OP-stack fee rule; set fee_params");
    let rule = match config.fee_params {
        FeeParams::Fixed(rule) => rule,
        FeeParams::Header => {
            Eip1559::from_extra_data(extra).ok_or_else(|| node(unknown()))?
        }
    };
    let base_fee = latest.base_fee_per_gas.unwrap_or_default().into();
    Ok(rule.grow(base_fee, config.blocks(time)))
}

/// The tip at `percentile` of the latest block's, from `eth_feeHistory`.
pub(crate) async fn tip<P: Provider>(
    provider: &P,
    percentile: f64,
) -> Result<u128, Refusal> {
    let percentiles = [percentile];
    let history = provider.get_fee_history(1, BlockNumberOrTag::Latest, &percentiles);
    let reward = history.await.map_err(node)?.reward;
    Ok(reward
        .and_then(|r| r.last()?.first().copied())
        .unwrap_or_default())
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
}

impl Head {
    /// M × its base fee: the most a send's fee cap is.
    pub(crate) fn fee_bound(&self, config: &Config) -> u128 {
        let m = config.max_fee_percent.into();
        self.base_fee.saturating_mul(m) / 100
    }
}

/// The latest block, to pin.
pub(crate) async fn head<P: Provider>(provider: &P) -> Result<Head, Refusal> {
    let latest = latest(provider).await?;
    Ok(Head {
        hash: latest.hash,
        number: latest.number,
        time: latest.timestamp,
        base_fee: latest.base_fee_per_gas.unwrap_or_default().into(),
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
