//! What the relayer is configured with for one chain.

use std::time::Duration;

use alloy::primitives::Address;

/// The send-to-inclusion window a production chain is configured with.
pub const INCLUSION_WINDOW: Duration = Duration::from_secs(120);

/// The L1 margin `k` of a production chain, in percent: the L1 fee bound a
/// refund covers is `k × getL1FeeUpperBound`, sized to the inclusion window.
pub const L1_MARGIN: u64 = 130;

/// How long a quote stays valid on a production chain.
pub const QUOTE_VALIDITY: Duration = Duration::from_secs(30);

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
    /// The chain's block time, over which the base fee grows at most 0.4% a block.
    pub block_time: Duration,
    /// The fee-history percentile a send's tip is read at.
    pub tip_percentile: f64,
}

impl Config {
    /// An anvil chain's configuration: no L1 fee, and a 5 s inclusion window,
    /// as anvil mines each send at once; 1 s blocks.
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
            tip_percentile: TIP_PERCENTILE,
        }
    }
}
