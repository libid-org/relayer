//! Sending a checked request.

use std::fmt::Display;

use alloy::{
    primitives::TxHash,
    providers::{
        PendingTransactionError,
        Provider,
        WatchTxError,
    },
    rpc::types::TransactionReceipt,
};

use crate::check::Checked;

/// A send that did not land.
#[derive(Debug)]
pub enum Error {
    /// The node refused the transaction or its receipt.
    Rpc(String),
    /// No receipt within the inclusion window; the transaction may still land.
    Timeout {
        /// The sent transaction.
        hash: TxHash,
    },
    /// The transaction was mined and reverted.
    Reverted {
        /// Its receipt.
        receipt: Box<TransactionReceipt>,
    },
}

/// Sign and send `checked` with `provider`'s relayer key, and wait for its
/// receipt for at most the inclusion window.
pub async fn send<P: Provider>(
    provider: &P,
    checked: Checked,
) -> Result<TransactionReceipt, Error> {
    let pending = provider.send_transaction(checked.tx).await.map_err(rpc)?;
    let hash = *pending.tx_hash();
    let pending = pending.with_timeout(Some(checked.timeout));
    match pending.get_receipt().await {
        Ok(receipt) if receipt.status() => Ok(receipt),
        Ok(receipt) => Err(Error::Reverted {
            receipt: Box::new(receipt),
        }),
        Err(PendingTransactionError::TxWatcher(WatchTxError::Timeout)) => {
            Err(Error::Timeout { hash })
        }
        Err(e) => Err(rpc(e)),
    }
}

fn rpc(e: impl Display) -> Error {
    Error::Rpc(e.to_string())
}
