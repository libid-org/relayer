//! What the relayer is configured with for one chain.

use std::time::Duration;

use alloy::primitives::{
    Address,
    U256,
};
use serde::Deserialize;

/// The send-to-inclusion window a production chain is configured with. The fee
/// covers the base fee's growth over it and the quote's validity: ×1.49 on Base.
pub const INCLUSION_WINDOW: Duration = Duration::from_secs(10);

/// The L1 margin `k` of a production chain, in percent: the L1 fee bound a
/// refund covers is `k × getL1FeeUpperBound`, sized to the inclusion window.
pub const L1_MARGIN: u64 = 130;

/// How long a quote stays valid on a production chain.
pub const QUOTE_VALIDITY: Duration = Duration::from_secs(10);

/// The most gas a transaction may have (EIP-7825): the highest `max_gas_limit`.
pub const MAX_TX_GAS: u64 = 1 << 24;

/// The most a tick and each node request take on a production chain.
pub const TICK_TIMEOUT: Duration = Duration::from_secs(30);

/// The fee-history percentile a send's tip is read at, between 50 and 90.
pub const TIP_PERCENTILE: f64 = 75.0;

/// How long a loss refuses its identity on a production chain.
pub const LOSS_HOLD: Duration = Duration::from_secs(24 * 3600);

const UNLIMITED: Budget = Budget {
    wei: U256::MAX,
    window: Duration::from_secs(3600),
};

/// How the relayer bounds the L1 data fee of a send.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum L1Fee {
    /// An OP-stack chain: `getL1FeeUpperBound` of the gas price oracle.
    OpStack,
    /// No L1 fee, as on anvil.
    None,
}

/// A chain's EIP-1559 base fee rule: each block's base fee grows at most
/// `(elasticity - 1) / denominator`, and is never below `min_base_fee`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
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

    /// The most the base fee grows over `blocks` without its minimum, in percent
    /// rounded up.
    pub fn growth_percent(&self, blocks: u128) -> u64 {
        let rule = Self {
            min_base_fee: 0,
            ..*self
        };
        u64::try_from(rule.grow(10_000, blocks).div_ceil(100)).unwrap_or(u64::MAX)
    }
}

/// Where the relayer reads a chain's base fee rule, at each fee it computes:
/// `"header"`, or `{ fixed = { denominator, elasticity, min_base_fee } }`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FeeParams {
    /// The latest OP-stack header's `extraData`.
    Header,
    /// A fixed rule.
    Fixed(Eip1559),
}

/// At most `wei` lost over any `window`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    /// The loss that trips the budget, in wei.
    pub wei: U256,
    /// How far back losses count.
    #[serde(with = "humantime_serde")]
    pub window: Duration,
}

/// One chain's relayer configuration.
#[derive(Clone, Debug)]
pub struct Config {
    /// The chain every request is for.
    pub chain_id: u64,
    /// `WalletFactory`, the create path's target and the voucher of its wallets.
    pub factory: Address,
    /// The release versions a target may run: the ones whose AUTHORIZED data layout
    /// the relayer decodes.
    pub supported_releases: Vec<u64>,
    /// The highest gas limit a request may be priced for; above it, `GasLimitTooHigh`.
    pub max_gas_limit: u64,
    /// The most value a batch may ask the relayer to front; above it,
    /// `ValueTooHigh`. `None` is no cap.
    pub max_value: Option<U256>,
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
    /// M: a send's fee cap is at most M percent of the pinned block's base fee, at
    /// least the base fee rule's growth over the inclusion window.
    pub max_fee_percent: u64,
    /// The highest fee cap a cancel is raised to, in percent of the pinned block's
    /// base fee, or of the send's when the base fee fell: at least 1.1 × M, the
    /// least cap that replaces a send.
    pub max_cancel_fee_percent: u64,
    /// The balance below which sending pauses, in wei.
    pub min_balance: U256,
    /// The ticks in a row a node error on the oldest queued request's run holds the
    /// queue before the request fails as `Node`; and a receipt in flight that does not
    /// parse, before it is charged as an unrefunded loss at its worst cost.
    pub node_error_ticks: u32,
    /// The blocks our nonce may stay mined with no receipt of ours, as from a lagging
    /// node, before sending pauses; and a receipt's loss stays pending for the key's
    /// balances before it is charged as unrefunded.
    pub receipt_lag_blocks: u64,
    /// The loss budget of all identities, each loss at its worst until its receipt:
    /// past it, sending pauses and requests are `Paused`.
    pub global_budget: Budget,
    /// How long a loss refuses its identity as `IdentityHeld`.
    pub loss_hold: Duration,
    /// The most a tick may take: past it, the tick is dropped where it stands.
    pub tick_timeout: Duration,
}

impl Config {
    /// An anvil chain's configuration: no L1 fee, and a 5 s inclusion window,
    /// as anvil mines each send at once; 1 s blocks and L1's base fee rule, M its
    /// growth over the window, cancels up to 10× the base fee, no balance floor, 5
    /// ticks of node errors on a request, receipts up to 10 blocks behind, no loss
    /// budget or value cap, a day's hold on a loss, gas limits up to [`MAX_TX_GAS`], and release 1, the one the
    /// contracts' test stack publishes.
    pub fn anvil(chain_id: u64, factory: Address, relayer: Address) -> Self {
        let mut config = Self {
            chain_id,
            factory,
            supported_releases: vec![1],
            max_gas_limit: MAX_TX_GAS,
            max_value: None,
            relayer,
            l1_fee: L1Fee::None,
            l1_margin: L1_MARGIN,
            inclusion_window: Duration::from_secs(5),
            quote_validity: QUOTE_VALIDITY,
            block_time: Duration::from_secs(1),
            fee_params: FeeParams::Fixed(Eip1559::L1),
            tip_percentile: TIP_PERCENTILE,
            max_fee_percent: 0,
            max_cancel_fee_percent: 1000,
            min_balance: U256::ZERO,
            node_error_ticks: 5,
            receipt_lag_blocks: 10,
            global_budget: UNLIMITED,
            loss_hold: LOSS_HOLD,
            tick_timeout: TICK_TIMEOUT,
        };
        let window = config.blocks(config.inclusion_window);
        config.max_fee_percent = Eip1559::L1.growth_percent(window);
        config
    }

    /// Refuses settings the sender cannot keep: no supported release, a max gas limit
    /// above [`MAX_TX_GAS`], an inclusion window shorter than two blocks, a max cancel
    /// fee below 1.1 × M, a tip percentile outside 50 to 90, an L1 margin under 100%,
    /// a budget window of 0, or no receipt lag.
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.supported_releases.is_empty() {
            return Err("supported_releases is empty; list a release".into());
        }
        if self.l1_margin < 100 {
            let k = self.l1_margin;
            return Err(format!("l1_margin_percent {k} is under 100; raise it"));
        }
        if self.global_budget.window.is_zero() {
            return Err("global_budget's window is 0; set it above 0".into());
        }
        if self.receipt_lag_blocks == 0 {
            return Err("receipt_lag_blocks is 0; set it to 1 or more".into());
        }
        if self.max_gas_limit > MAX_TX_GAS {
            let max = self.max_gas_limit;
            let most = format!("a transaction's {MAX_TX_GAS} (EIP-7825)");
            return Err(format!("max_gas_limit {max} is above {most}; lower it"));
        }
        if self.inclusion_window < 2 * self.block_time {
            let (w, b) = (self.inclusion_window, self.block_time);
            return Err(format!(
                "inclusion_window {w:?} is under twice block_time {b:?}; raise it"
            ));
        }
        let (cancel, m) = (self.max_cancel_fee_percent, self.max_fee_percent);
        if u128::from(cancel) * 10 < u128::from(m) * 11 {
            return Err(format!(
                "max_cancel_fee_percent {cancel} is under 1.1 × max_fee_percent {m}; raise it"
            ));
        }
        let tip = self.tip_percentile;
        if !(50.0..=90.0).contains(&tip) {
            return Err(format!(
                "tip_percentile {tip} is outside 50 to 90; set it within"
            ));
        }
        Ok(())
    }

    /// The blocks in `time`, at the chain's block time.
    pub(crate) fn blocks(&self, time: Duration) -> u128 {
        time.as_millis() / self.block_time.as_millis().max(1)
    }
}
