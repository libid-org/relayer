//! What the ledger says the relayer earned: requests by status, profit, cancel rate,
//! inclusion times and margin. Wei amounts are summed in Rust: they are 32-byte blobs.

use std::{
    collections::BTreeMap,
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

use crate::relay::{
    first,
    rows,
    u256,
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
    /// Seconds from first send to first seen receipt: p50, p90, p99.
    pub inclusion_seconds: Option<[u64; 3]>,
    /// The cost of included requests whose refund arrived per their `net`.
    pub margin: Option<f64>,
}

/// The numbers of the ledger at `db`, opened read-only.
pub fn stats(db: &Path) -> Result<Stats, Error> {
    let db = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let sql = "SELECT CASE WHEN r.reverted THEN 'reverted' ELSE r.status END, r.net, \
               r.gas_used, r.gas_price, r.l1_fee, \
               r.included_at - (SELECT min(sent_at) FROM attempts a WHERE a.id = r.id), \
               r.loss FROM requests r";
    let row = |r: &rusqlite::Row| {
        let cost = match r.get::<_, Option<u64>>(2)? {
            Some(gas) => Some((U256::from(gas) * u256(r, 3)? + u256(r, 4)?, u256(r, 6)?)),
            None => None,
        };
        Ok((
            first::<String>(r)?,
            u256(r, 1)?,
            cost,
            r.get::<_, Option<u64>>(5)?,
        ))
    };
    let mut stats = Stats::default();
    let (mut receipts, mut cancelled) = (0u64, 0u64);
    let (mut costs, mut nets) = (U256::ZERO, U256::ZERO);
    let mut times = vec![];
    let signed = |w: U256| I256::try_from(w).unwrap_or(I256::MAX);
    for (status, net, cost, inclusion) in rows(&db, sql, [], row)? {
        *stats.requests.entry(status.clone()).or_default() += 1;
        times.extend(inclusion.filter(|_| status != "cancelled"));
        let Some((cost, loss)) = cost else { continue };
        receipts += 1;
        cancelled += u64::from(status == "cancelled");
        if status == "included" && loss.is_zero() {
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
    stats.margin = (!nets.is_zero()).then(|| f64::from(costs) / f64::from(nets));
    Ok(stats)
}
