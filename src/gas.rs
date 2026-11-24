//! What a send needs in gas (D7, D8) and in L1 fee (D5), from one simulation.

use alloy::{
    consensus::SignableTransaction,
    eips::{
        eip1559::Eip1559Estimation,
        BlockNumberOrTag,
    },
    network::TransactionBuilder,
    primitives::{
        address,
        Address,
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
        TransactionRequest,
    },
};
use libid_contracts::gas::CALL_OVERHEAD;

use crate::{
    check::{
        node,
        Refusal,
    },
    config::{
        Config,
        L1Fee,
    },
};

/// The OP-stack `GasPriceOracle` predeploy.
const GAS_PRICE_ORACLE: Address = address!("420000000000000000000000000000000000000F");

alloy::sol! {
    #[sol(rpc)]
    interface GasPriceOracle {
        function getL1FeeUpperBound(uint256 unsignedTxSize) external view returns (uint256);
    }
}

/// `ceil(1.25 × gas) + (callGas + CALL_OVERHEAD) × (64/63)^d`, `d` 2 on the
/// create path and 1 otherwise. The 1.25 is the gas a transaction needs while it
/// runs above its gas used after the refund (EIP-3529).
pub(crate) fn limit(gas: u64, call_gas: u64, create: bool) -> u64 {
    let d = if create { 2 } else { 1 };
    let reserve = u128::from(call_gas) + u128::from(CALL_OVERHEAD);
    let reserve = (reserve * 64u128.pow(d)).div_ceil(63u128.pow(d));
    let run = u128::from(gas) * 5;
    u64::try_from(run.div_ceil(4) + reserve).unwrap_or(u64::MAX)
}

/// `call` sent at `gas_limit` and `fees`: the tip at most the fee cap.
pub(crate) fn priced(
    call: TransactionRequest,
    gas_limit: u64,
    fees: &Eip1559Estimation,
    chain_id: u64,
) -> TransactionRequest {
    call.with_gas_limit(gas_limit)
        .with_max_fee_per_gas(fees.max_fee_per_gas)
        .with_max_priority_fee_per_gas(
            fees.max_priority_fee_per_gas.min(fees.max_fee_per_gas),
        )
        .with_chain_id(chain_id)
}

/// The latest block's timestamp.
pub(crate) async fn latest_time<P: Provider>(provider: &P) -> Result<u64, Refusal> {
    let latest = provider.get_block_by_number(BlockNumberOrTag::Latest);
    let latest = latest.await.map_err(node)?;
    Ok(latest
        .ok_or_else(|| node("no latest block"))?
        .header
        .timestamp)
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

/// `k × getL1FeeUpperBound` of `tx`'s unsigned size: the L1 fee a refund covers (D5).
pub(crate) async fn l1_fee_bound<P: Provider>(
    provider: &P,
    config: &Config,
    tx: &TransactionRequest,
) -> Result<U256, Refusal> {
    if config.l1_fee == L1Fee::None {
        return Ok(U256::ZERO);
    }
    // The provider fills the nonce at send; the largest one sizes the bound.
    let mut tx = tx.clone();
    tx.nonce.get_or_insert(u64::MAX);
    let size = tx.build_1559().map_err(node)?.encoded_for_signing().len();
    let oracle = GasPriceOracle::new(GAS_PRICE_ORACLE, provider);
    let bound = oracle.getL1FeeUpperBound(U256::from(size));
    let bound = bound.call().await.map_err(node)?;
    Ok(bound.saturating_mul(U256::from(config.l1_margin)) / U256::from(100))
}
