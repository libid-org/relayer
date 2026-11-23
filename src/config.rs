//! What the relayer is configured with for one chain.

use alloy::primitives::Address;

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
}

impl Config {
    /// An anvil chain's configuration: no L1 fee, default margins.
    pub fn anvil(chain_id: u64, factory: Address, forwarder: Address, relayer: Address) -> Self {
        let _ = (chain_id, factory, forwarder, relayer);
        unimplemented!("S1")
    }
}
