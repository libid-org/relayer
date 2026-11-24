//! The relayer's own preconditions on a request, checked before it sends:
//! canonical data (D4), the refund (D5), the target (D9) and the deadline.

use alloy::{
    primitives::{
        Address,
        Bytes,
        B256,
        U256,
    },
    providers::Provider,
};
use libid_contracts::WalletRegistryError;

use crate::config::Config;

/// The create path's wallet: `WalletFactory.createAndExecute(platformId, idNode, salt, …)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Create {
    /// The genesis identity's platform.
    pub platform_id: B256,
    /// The genesis identity's id node.
    pub id_node: B256,
    /// The wallet's salt.
    pub salt: U256,
}

/// A signed batch to relay. Its `value` is the one `data` carries.
#[derive(Clone, Debug)]
pub struct Request {
    /// The wallet that runs the batch.
    pub wallet: Address,
    /// Set when the wallet is created in the same transaction.
    pub create: Option<Create>,
    /// `execute(AUTHORIZED_MODE, data)`'s `data`, signed.
    pub data: Bytes,
    /// The gas limit the client's refund was priced for.
    pub gas_limit: u64,
}

/// A request that passed every check: the only thing [`crate::chain::send`] sends.
#[derive(Clone, Debug)]
pub struct Checked {
    _private: (),
}

/// Why a request is not sent.
#[derive(Debug)]
pub enum Refusal {
    /// `data` does not decode, or does not re-encode to itself byte for byte.
    NoncanonicalData,
    /// No top-level `Execution(relayer, R, "")` in the batch.
    RefundMissing,
    /// `R - value` does not cover the gas and L1 fee bound.
    RefundTooLow {
        /// The batch's refund.
        refund: U256,
        /// The least refund the relayer accepts.
        required: U256,
    },
    /// The contracts do not vouch for the target.
    UnknownWallet,
    /// The batch's deadline is past.
    Expired,
    /// The simulation reverted, decoded on the wallet route where it can be.
    Reverted {
        /// The decoded error.
        error: Option<WalletRegistryError>,
        /// The revert data.
        data: Bytes,
    },
    /// The node could not answer.
    Node(String),
}

/// Check `request` against `config` on the chain `provider` reads.
pub async fn check<P: Provider>(
    provider: &P,
    config: &Config,
    request: &Request,
) -> Result<Checked, Refusal> {
    let _ = (provider, config, request);
    unimplemented!("S1")
}
