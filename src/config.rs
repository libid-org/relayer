//! What the relayer is configured with for one chain.

use std::time::Duration;

use alloy::primitives::Address;

/// The send-to-inclusion window a production chain is configured with.
pub const INCLUSION_WINDOW: Duration = Duration::from_secs(120);

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
    /// How long a send may take to be included: a batch must still run that
    /// long after the latest block, and a send waits that long for its receipt.
    pub inclusion_window: Duration,
}

impl Config {
    /// An anvil chain's configuration: no L1 fee, and a 5 s inclusion window,
    /// as anvil mines each send at once.
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
            inclusion_window: Duration::from_secs(5),
        }
    }
}
