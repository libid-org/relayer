//! What a send needs in gas and in L1 fee, from one simulation.

use alloy::{
    consensus::SignableTransaction,
    eips::BlockNumberOrTag,
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

/// `call` priced at `gas_limit` and the node's fees, the tip at most the fee cap; its
/// L1 fee bound; and the most it costs the relayer, `gas_limit × max fee + L1 bound`.
pub(crate) async fn price<P: Provider>(
    provider: &P,
    config: &Config,
    call: TransactionRequest,
    gas_limit: u64,
) -> Result<(TransactionRequest, U256, U256), Refusal> {
    let fees = provider.estimate_eip1559_fees().await.map_err(node)?;
    let max_fee_per_gas = fees.max_fee_per_gas;
    let tx = call
        .with_gas_limit(gas_limit)
        .with_max_fee_per_gas(max_fee_per_gas)
        .with_max_priority_fee_per_gas(fees.max_priority_fee_per_gas.min(max_fee_per_gas))
        .with_chain_id(config.chain_id);
    let l1_fee_bound = l1_fee_bound(provider, config, &tx).await?;
    let cost = U256::from(gas_limit)
        .saturating_mul(U256::from(max_fee_per_gas))
        .saturating_add(l1_fee_bound);
    Ok((tx, l1_fee_bound, cost))
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

/// `k × getL1FeeUpperBound` of `tx`'s unsigned size: the L1 fee a refund covers.
async fn l1_fee_bound<P: Provider>(
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
