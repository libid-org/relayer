//! Sending a checked request.

use alloy::{
    providers::Provider,
    rpc::types::TransactionReceipt,
};

use crate::check::Checked;

/// A send that did not land.
#[derive(Debug)]
pub enum Error {
    /// The node refused the transaction or its receipt.
    Rpc(String),
}

/// Sign and send `checked` with `provider`'s relayer key, and wait for its receipt.
pub async fn send<P: Provider>(
    provider: &P,
    checked: Checked,
) -> Result<TransactionReceipt, Error> {
    let _ = (provider, checked);
    unimplemented!("S1")
}
