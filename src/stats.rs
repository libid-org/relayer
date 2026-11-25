//! What the ledger says the relayer earned: requests by status, profit, cancel rate,
//! inclusion times and cost ratio. Wei amounts are summed in Rust: they are 32-byte blobs.

use std::{
    collections::BTreeMap,
    fmt,
    path::Path,
};

use alloy::primitives::{
    I256,
    U256,
};
use rusqlite::{
    Connection,
    OpenFlags,
};
use serde_json::{
    json,
    Value,
};

use crate::relay::{
    entries,
    Entry,
    Error,
};

/// The ledger's numbers.
#[derive(Debug, Default)]
pub struct Stats {
    /// Requests by status; an included request that reverted counts as `reverted`.
    pub requests: BTreeMap<String, u64>,
    /// `net - cost` of each included request whose refund arrived, less the loss
    /// charged for every other receipt, in wei.
    pub profit: I256,
    /// `profit` per request with a receipt.
    pub profit_per_request: I256,
    /// Cancelled requests per request with a receipt.
    pub cancel_rate: f64,
    /// Seconds from first send to first seen receipt, at least 0: p50, p90, p99.
    pub inclusion_seconds: Option<[u64; 3]>,
    /// The cost of included requests whose refund arrived per their `net`; lower is
    /// better.
    pub cost_ratio: Option<f64>,
    /// Included requests whose loss is not yet charged, outside profit, cancel rate
    /// and cost ratio.
    pub pending: u64,
}

impl Stats {
    /// The numbers as JSON, wei amounts as decimal strings.
    pub fn json(&self) -> Value {
        json!({
            "requests": self.requests,
            "profit": self.profit.to_string(),
            "profitPerRequest": self.profit_per_request.to_string(),
            "cancelRate": self.cancel_rate,
            "inclusionSeconds": self.inclusion_seconds,
            "costRatio": self.cost_ratio,
            "pending": self.pending,
        })
    }
}

impl fmt::Display for Stats {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let requests = self.requests.iter().map(|(s, n)| format!("{s} {n}"));
        let requests = requests.collect::<Vec<_>>().join(", ");
        writeln!(
            f,
            "requests      {}",
            if requests.is_empty() {
                "none"
            } else {
                &requests
            }
        )?;
        let each = self.profit_per_request;
        writeln!(f, "profit        {} wei, {each} per request", self.profit)?;
        writeln!(f, "cancel rate   {:.3}", self.cancel_rate)?;
        match self.inclusion_seconds {
            Some([p50, p90, p99]) => {
                writeln!(f, "inclusion     p50 {p50}s, p90 {p90}s, p99 {p99}s")?
            }
            None => writeln!(f, "inclusion     none")?,
        }
        match self.cost_ratio {
            Some(ratio) => writeln!(f, "cost ratio    {ratio}")?,
            None => writeln!(f, "cost ratio    none")?,
        }
        writeln!(f, "pending       {}", self.pending)
    }
}

/// The numbers of the ledger at `db`, opened read-only.
pub fn stats(db: &Path) -> Result<Stats, Error> {
    let db = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stats = Stats::default();
    let (mut receipts, mut cancelled) = (0u64, 0u64);
    let (mut costs, mut nets) = (U256::ZERO, U256::ZERO);
    let mut times = vec![];
    let signed = |w: U256| I256::try_from(w).unwrap_or(I256::MAX);
    for Entry {
        status,
        net,
        cost,
        loss,
        inclusion,
    } in entries(&db)?
    {
        *stats.requests.entry(status.clone()).or_default() += 1;
        times.extend(inclusion.filter(|_| status != "cancelled"));
        // A loss charged with no receipt read has no cost.
        let Some(loss) = loss else {
            stats.pending += u64::from(cost.is_some());
            continue;
        };
        receipts += 1;
        cancelled += u64::from(status == "cancelled");
        if let (Some(cost), true) = (cost, status == "included" && loss.is_zero()) {
            stats.profit += signed(net) - signed(cost);
            (costs, nets) = (costs + cost, nets + net);
        } else {
            stats.profit -= signed(loss);
        }
    }
    if receipts > 0 {
        stats.profit_per_request = stats.profit / I256::try_from(receipts)?;
        stats.cancel_rate = cancelled as f64 / receipts as f64;
    }
    times.sort_unstable();
    let at = |p: usize| times[(times.len() * p).div_ceil(100).max(1) - 1];
    stats.inclusion_seconds = (!times.is_empty()).then(|| [at(50), at(90), at(99)]);
    stats.cost_ratio = (!nets.is_zero()).then(|| f64::from(costs) / f64::from(nets));
    Ok(stats)
}
