//! What the ledger holds now: rows by status, the identities a loss holds, the loss the
//! global budget counts, and the losses not yet charged. A paid row is dropped
//! `receipt_lag_blocks` after its receipt, so there is no history to sum.

use std::{
    collections::BTreeMap,
    fmt,
    path::Path,
};

use alloy::primitives::U256;
use rusqlite::{
    Connection,
    OpenFlags,
};
use serde_json::{
    json,
    Value,
};

use crate::{
    config::Config,
    relay::{
        counts,
        lost,
        Error,
    },
};

/// The ledger's numbers.
#[derive(Debug, Default)]
pub struct Stats {
    /// Rows by status; an included one whose batch reverted counts as `reverted`.
    pub requests: BTreeMap<String, u64>,
    /// Identities a loss holds.
    pub held: u64,
    /// What the global budget counts as lost over its window, in wei.
    pub lost: U256,
    /// Included rows whose loss is not yet charged.
    pub pending: u64,
}

impl Stats {
    /// The numbers as JSON, wei amounts as decimal strings.
    pub fn json(&self) -> Value {
        json!({
            "requests": self.requests,
            "held": self.held,
            "lost": self.lost.to_string(),
            "pending": self.pending,
        })
    }
}

impl fmt::Display for Stats {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let requests = self.requests.iter().map(|(s, n)| format!("{s} {n}"));
        let requests = requests.collect::<Vec<_>>().join(", ");
        let requests = if requests.is_empty() {
            "none"
        } else {
            &requests
        };
        writeln!(f, "requests      {requests}")?;
        writeln!(f, "held          {}", self.held)?;
        writeln!(f, "lost          {} wei", self.lost)?;
        writeln!(f, "pending       {}", self.pending)
    }
}

/// The numbers of the ledger at `db`, opened read-only, under `config`'s budget window
/// and loss hold.
pub fn stats(db: &Path, config: &Config) -> Result<Stats, Error> {
    let db = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let (requests, held, pending) = counts(&db, config.loss_hold)?;
    Ok(Stats {
        requests,
        held,
        lost: lost(&db, config.global_budget.window)?,
        pending,
    })
}
