//! What the relayer is configured with for one chain.

use std::time::Duration;

use alloy::primitives::Address;

/// The send-to-inclusion window a production chain is configured with. The fee
/// covers the base fee's growth over it and the quote's validity: ×1.49 on Base.
pub const INCLUSION_WINDOW: Duration = Duration::from_secs(10);

/// The L1 margin `k` of a production chain, in percent: the L1 fee bound a
/// refund covers is `k × getL1FeeUpperBound`, sized to the inclusion window.
pub const L1_MARGIN: u64 = 130;

/// How long a quote stays valid on a production chain.
pub const QUOTE_VALIDITY: Duration = Duration::from_secs(10);

/// The fee-history percentile a send's tip is read at, between 50 and 90.
pub const TIP_PERCENTILE: f64 = 75.0;

/// How the relayer bounds the L1 data fee of a send.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum L1Fee {
    /// An OP-stack chain: `getL1FeeUpperBound` of the gas price oracle.
    OpStack,
    /// No L1 fee, as on anvil.
    None,
}

/// A chain's EIP-1559 base fee rule: each block's base fee grows at most
/// `(elasticity - 1) / denominator`, and is never below `min_base_fee`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Eip1559 {
    /// The base fee change denominator.
    pub denominator: u32,
    /// The elasticity multiplier.
    pub elasticity: u32,
    /// The minimum base fee, in wei.
    pub min_base_fee: u64,
}

impl Eip1559 {
    /// Ethereum L1's rule: 12.5% a block, no minimum.
    pub const L1: Self = Self {
        denominator: 8,
        elasticity: 2,
        min_base_fee: 0,
    };

    /// The rule in an OP-stack header's `extraData`: version 0 (Holocene),
    /// `0x00 ‖ denominator u32 ‖ elasticity u32`, or version 1 (Jovian), which
    /// appends `min_base_fee u64`; big-endian.
    pub fn from_extra_data(extra: &[u8]) -> Option<Self> {
        let u32_at =
            |i: usize| Some(u32::from_be_bytes(extra.get(i..i + 4)?.try_into().ok()?));
        let min_base_fee = match (extra.first(), extra.len()) {
            (Some(0), 9) => 0,
            (Some(1), 17) => u64::from_be_bytes(extra[9..17].try_into().ok()?),
            _ => return None,
        };
        let (denominator, elasticity) = (u32_at(1)?, u32_at(5)?);
        (denominator != 0 && elasticity != 0).then_some(Self {
            denominator,
            elasticity,
            min_base_fee,
        })
    }

    /// `max(base_fee, min_base_fee)` grown by its most over `blocks`, rounded up
    /// each block.
    pub fn grow(&self, base_fee: u128, blocks: u128) -> u128 {
        let d = u128::from(self.denominator.max(1));
        let n = d + u128::from(self.elasticity.max(1)) - 1;
        let base = base_fee.max(u128::from(self.min_base_fee));
        (0..blocks).fold(base, |fee, _| fee.saturating_mul(n).div_ceil(d))
    }
}

/// Where the relayer reads a chain's base fee rule, at each fee it computes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeeParams {
    /// The latest OP-stack header's `extraData`.
    Header,
    /// A fixed rule.
    Fixed(Eip1559),
}

/// One chain's relayer configuration.
#[derive(Clone, Debug)]
pub struct Config {
    /// The chain every request is for.
    pub chain_id: u64,
    /// `WalletFactory`, the create path's target.
    pub factory: Address,
    /// `WalletForwarder`, which EIP-7702 accounts delegate to.
    pub forwarder: Address,
    /// The sending key's address, which every refund must pay.
    pub relayer: Address,
    /// The L1 fee model.
    pub l1_fee: L1Fee,
    /// The L1 margin `k`, in percent.
    pub l1_margin: u64,
    /// How long a send may take to be included: a batch must still run that
    /// long after the latest block.
    pub inclusion_window: Duration,
    /// How long a quote stays valid; its fee covers the base fee's growth meanwhile.
    pub quote_validity: Duration,
    /// The chain's block time, the step of the base fee's growth.
    pub block_time: Duration,
    /// The chain's base fee rule.
    pub fee_params: FeeParams,
    /// The fee-history percentile a send's tip is read at.
    pub tip_percentile: f64,
}

impl Config {
    /// An anvil chain's configuration: no L1 fee, and a 5 s inclusion window,
    /// as anvil mines each send at once; 1 s blocks and L1's base fee rule.
    pub fn anvil(
        chain_id: u64,
        factory: Address,
        forwarder: Address,
        relayer: Address,
    ) -> Self {
        Self {
            chain_id,
            factory,
            forwarder,
            relayer,
            l1_fee: L1Fee::None,
            l1_margin: L1_MARGIN,
            inclusion_window: Duration::from_secs(5),
            quote_validity: QUOTE_VALIDITY,
            block_time: Duration::from_secs(1),
            fee_params: FeeParams::Fixed(Eip1559::L1),
            tip_percentile: TIP_PERCENTILE,
        }
    }
}
