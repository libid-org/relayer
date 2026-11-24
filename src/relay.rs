//! The sender: requests and their send attempts kept in SQLite, sent by the one
//! writer of the key's nonces, watched to a receipt, and cancelled at their deadline.

use std::{
    collections::BTreeSet,
    fs::File,
    path::{
        Path,
        PathBuf,
    },
    sync::{
        atomic::{
            AtomicBool,
            Ordering::Relaxed,
        },
        Mutex,
        MutexGuard,
        PoisonError,
    },
    time::Duration,
};

use alloy::{
    consensus::{
        SignableTransaction,
        Transaction,
        TxEnvelope,
    },
    eips::{
        BlockNumberOrTag,
        Decodable2718,
        Encodable2718,
    },
    network::TransactionBuilder,
    primitives::{
        keccak256,
        Address,
        Bytes,
        B256,
        U256,
    },
    providers::Provider,
    rpc::types::TransactionRequest,
    sol_types::SolValue,
};
use rusqlite::{
    params,
    Connection,
    OptionalExtension,
    Params,
    Row,
};

use crate::{
    check::{
        check,
        decode,
        node,
        target,
        Refusal,
        Request,
    },
    config::Config,
    gas,
};

/// Why the relayer did not start or tick.
pub type Error = Box<dyn std::error::Error + Send + Sync>;

/// A request's `R - value` and the L1 fee bound its refund covers.
type Cover = (U256, U256);

/// The fee-history percentile a cancel's tip is read at.
const CANCEL_PERCENTILE: f64 = 90.0;

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS requests (id BLOB PRIMARY KEY, wallet BLOB NOT NULL,
        tx TEXT NOT NULL, net BLOB NOT NULL, l1 BLOB NOT NULL, deadline INTEGER NOT NULL,
        status TEXT NOT NULL, block INTEGER, reverted INTEGER, detail BLOB);
    CREATE TABLE IF NOT EXISTS attempts (hash BLOB PRIMARY KEY, id BLOB NOT NULL,
        nonce INTEGER NOT NULL, cancel INTEGER NOT NULL, raw BLOB NOT NULL);
    CREATE TABLE IF NOT EXISTS nonce (next INTEGER NOT NULL);";

/// Where a request stands.
#[derive(Debug)]
pub enum Status {
    /// Accepted, not yet sent.
    Queued,
    /// Sent, with no receipt yet.
    Sent,
    /// Mined in `block`.
    Included {
        /// The block of its receipt.
        block: u64,
        /// The batch reverted.
        reverted: bool,
    },
    /// Included at or below the safe head.
    Confirmed {
        /// The block of its receipt.
        block: u64,
        /// The batch reverted.
        reverted: bool,
    },
    /// Replaced at its deadline by a cancel that landed; retriable.
    Cancelled,
    /// Refused before its first broadcast; retriable.
    Failed(Refusal),
}

/// A request's status and its transactions in the order they were signed.
#[derive(Debug)]
pub struct Report {
    /// Where the request stands.
    pub status: Status,
    /// Its send, then its cancel if any.
    pub tx_hashes: Vec<B256>,
}

/// A signed transaction, committed at its nonce before it is broadcast.
struct Attempt {
    hash: B256,
    nonce: u64,
    cancel: bool,
    raw: Bytes,
}

/// The relayer of one chain and one key, the only writer of the key's nonces
/// while it holds the lock on its database.
pub struct Relayer<P> {
    provider: P,
    config: Config,
    db: Mutex<Connection>,
    paused: AtomicBool,
    _lock: File,
}

impl<P: Provider> Relayer<P> {
    /// Lock and open `db`, and check that the stored sends explain the key's mined
    /// nonce; a fresh database starts at it. `provider` signs with the relayer's key.
    pub async fn start(provider: P, config: Config, db: &Path) -> Result<Self, Error> {
        let mut lock = db.as_os_str().to_owned();
        lock.push(".lock");
        let lock = PathBuf::from(lock);
        let file = File::create(&lock)?;
        let held = |_| format!("{} is held by another relayer", lock.display());
        file.try_lock().map_err(held)?;
        let conn = Connection::open(db)?;
        conn.execute_batch(SCHEMA)?;
        let mined = provider.get_transaction_count(config.relayer).latest();
        let (mined, next) = (
            mined.await?,
            conn.query_row("SELECT next FROM nonce", [], first),
        );
        match next.optional()? {
            None => drop(conn.execute("INSERT INTO nonce (next) VALUES (?1)", [mined])?),
            Some(next) => explained(&provider, &conn, mined, next).await?,
        }
        Ok(Self {
            provider,
            config,
            db: Mutex::new(conn),
            paused: AtomicBool::new(false),
            _lock: file,
        })
    }

    /// Check `request` and queue it under its id. The same request again returns its
    /// id; one that failed or was cancelled is queued again under it.
    pub async fn relay(&self, request: &Request) -> Result<B256, Refusal> {
        let (message, ..) = decode(&request.data).ok_or(Refusal::NoncanonicalData)?;
        let (p, c, wallet) = (&self.provider, &self.config, request.wallet);
        let call = target(p, c, wallet, request.create, &request.data, message.value);
        let call = call.await?;
        let to = TransactionBuilder::to(&call).unwrap_or_default();
        let input = TransactionBuilder::input(&call)
            .cloned()
            .unwrap_or_default();
        let id = (U256::from(c.chain_id), to, input, message.value);
        let id = keccak256(id.abi_encode_params());
        if let Some(id) = admit(&self.db(), id, wallet)? {
            return Ok(id);
        }
        let checked = check(p, c, request).await?;
        let (tx, l1) = (checked.tx, checked.l1_fee_bound);
        let unsigned = tx.clone().nonce(u64::MAX).build_1559().map_err(node)?;
        gas::l1_gate(p, c, unsigned.encoded_for_signing().into(), l1).await?;
        let tx = serde_json::to_string(&tx).map_err(node)?;
        let (net, l1) = (checked.net.to_be_bytes::<32>(), l1.to_be_bytes::<32>());
        let db = self.db();
        if let Some(id) = admit(&db, id, wallet)? {
            return Ok(id);
        }
        let deadline = message.deadline.to::<u64>();
        let row = params![&id[..], &wallet[..], tx, net, l1, deadline];
        db.execute("DELETE FROM attempts WHERE id = ?1", [&id[..]])
            .and_then(|_| db.execute(
                "INSERT OR REPLACE INTO requests (id, wallet, tx, net, l1, deadline, status) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'queued')",
                row,
            ))
            .map_err(node)?;
        Ok(id)
    }

    /// Resolve or rebroadcast every sent request, then send the queued ones unless
    /// sending paused.
    pub async fn tick(&self) -> Result<(), Error> {
        let mined = self.provider.get_transaction_count(self.config.relayer);
        let mined = mined.latest().await?;
        let row = |r: &Row| Ok((b256(r, 0)?, r.get::<_, u64>(1)?, cover(r)?));
        let sent = "SELECT id, deadline, net, l1 FROM requests \
                    WHERE status IN ('sent', 'included')";
        let sent = rows(&self.db(), sent, [], row)?;
        for (id, deadline, cover) in sent {
            self.watch(id, deadline, cover, mined).await?;
        }
        let queued = "SELECT id, tx, net, l1 FROM requests WHERE status = 'queued' \
                      ORDER BY rowid";
        let row = |r: &Row| Ok((b256(r, 0)?, r.get::<_, String>(1)?, cover(r)?));
        let queued = rows(&self.db(), queued, [], row)?;
        for (id, tx, cover) in queued {
            if self.paused.load(Relaxed) {
                break;
            }
            self.send(id, serde_json::from_str(&tx)?, cover, mined)
                .await?;
        }
        Ok(())
    }

    /// `id`'s status and its transactions.
    pub async fn status(&self, id: B256) -> Result<Report, Error> {
        let sql = "SELECT status, block, reverted, detail FROM requests WHERE id = ?1";
        let row = |r: &Row| {
            let (block, reverted) =
                (r.get::<_, Option<_>>(1)?, r.get::<_, Option<_>>(2)?);
            let detail = r.get::<_, Option<Vec<u8>>>(3)?.unwrap_or(vec![0; 64]);
            Ok((
                first::<String>(r)?,
                block.unwrap_or(0),
                reverted.unwrap_or(false),
                detail,
            ))
        };
        let (status, block, reverted, detail) =
            self.db().query_row(sql, [&id[..]], row)?;
        let status = match &status[..] {
            "queued" => Status::Queued,
            "sent" => Status::Sent,
            "included" => Status::Included { block, reverted },
            "confirmed" => Status::Confirmed { block, reverted },
            "cancelled" => Status::Cancelled,
            _ => Status::Failed(Refusal::L1FeeAboveBound {
                fee: U256::from_be_slice(&detail[..32]),
                bound: U256::from_be_slice(&detail[32..]),
            }),
        };
        let tx_hashes = self.attempts(id)?.iter().map(|a| a.hash).collect();
        Ok(Report { status, tx_hashes })
    }

    /// Whether sending paused on a nonce another sender took.
    pub async fn paused(&self) -> bool {
        self.paused.load(Relaxed)
    }

    /// Resolve `id` from a receipt of any of its attempts; with none, cancel it at
    /// its deadline or broadcast its last attempt again.
    async fn watch(
        &self,
        id: B256,
        deadline: u64,
        cover: Cover,
        mined: u64,
    ) -> Result<(), Error> {
        let attempts = self.attempts(id)?;
        for a in &attempts {
            let Some(receipt) = self.provider.get_transaction_receipt(a.hash).await?
            else {
                continue;
            };
            let block = receipt.block_number.unwrap_or_default();
            let safe = self.provider.get_block_by_number(BlockNumberOrTag::Safe);
            let safe = safe.await?.is_some_and(|s| s.header.number >= block);
            let status = match (a.cancel, safe) {
                (true, _) => "cancelled",
                (false, true) => "confirmed",
                (false, false) => "included",
            };
            return self.set(id, status, Some((block, !receipt.status())), None);
        }
        self.set(id, "sent", None, None)?; // a receipt a reorg removed
        let Some(last) = attempts.last() else {
            return Ok(());
        };
        let now = gas::latest_time(&self.provider).await.map_err(refused)?;
        if last.cancel || now < deadline {
            return self.broadcast(last, cover, mined).await;
        }
        self.cancel(id, last, mined).await
    }

    /// Sign `tx` at the next nonce with the tip read now, gate its L1 fee, commit it
    /// and broadcast it.
    async fn send(
        &self,
        id: B256,
        tx: TransactionRequest,
        cover: Cover,
        mined: u64,
    ) -> Result<(), Error> {
        let next = self.db().query_row("SELECT next FROM nonce", [], first)?;
        if mined > next {
            self.paused.store(true, Relaxed);
            return Ok(());
        }
        let tip = gas::tip(&self.provider, self.config.tip_percentile).await;
        let tip = tip.map_err(refused)?.min(tx.max_fee_per_gas.unwrap_or(0));
        let tx = tx
            .from(self.config.relayer)
            .nonce(next)
            .max_priority_fee_per_gas(tip);
        let attempt = self.sign(tx, false).await?;
        affordable(&attempt.raw, cover)?;
        let gate =
            gas::l1_gate(&self.provider, &self.config, attempt.raw.clone(), cover.1);
        if let Err(Refusal::L1FeeAboveBound { fee, bound }) = gate.await {
            let detail = [fee.to_be_bytes::<32>(), bound.to_be_bytes::<32>()].concat();
            return self.set(id, "failed", None, Some(detail));
        }
        self.commit(id, &attempt)?;
        self.broadcast(&attempt, cover, mined).await
    }

    /// A 0-value self-send at `sent`'s nonce, both fees at least 1.1× `sent`'s and
    /// its cap at least twice the base fee.
    async fn cancel(&self, id: B256, sent: &Attempt, mined: u64) -> Result<(), Error> {
        let tx = TxEnvelope::decode_2718(&mut &sent.raw[..])?;
        let more = |fee: u128| fee.saturating_mul(11).div_ceil(10);
        let base = gas::fee_floor(&self.provider, &self.config, Duration::ZERO).await;
        let cap = more(tx.max_fee_per_gas()).max(base.map_err(refused)? * 2);
        let tip = gas::tip(&self.provider, CANCEL_PERCENTILE)
            .await
            .map_err(refused)?;
        let tip = more(tx.max_priority_fee_per_gas().unwrap_or_default()).max(tip);
        let relayer = self.config.relayer;
        let cancel = TransactionRequest::default()
            .from(relayer)
            .to(relayer)
            .nonce(sent.nonce)
            .gas_limit(21_000)
            .max_fee_per_gas(cap)
            .max_priority_fee_per_gas(tip.min(cap))
            .with_chain_id(self.config.chain_id);
        let attempt = self.sign(cancel, true).await?;
        self.commit(id, &attempt)?;
        self.broadcast(&attempt, Default::default(), mined).await
    }

    async fn sign(&self, tx: TransactionRequest, cancel: bool) -> Result<Attempt, Error> {
        let nonce = tx.nonce.unwrap_or_default();
        let signed = self.provider.fill_and_sign_transaction(tx).await?;
        let (hash, raw) = (*signed.tx_hash(), signed.encoded_2718().into());
        Ok(Attempt {
            hash,
            nonce,
            cancel,
            raw,
        })
    }

    /// Broadcast `a` unless the node has it. Its nonce mined with no receipt of ours
    /// is another sender's, and sending pauses.
    async fn broadcast(
        &self,
        a: &Attempt,
        cover: Cover,
        mined: u64,
    ) -> Result<(), Error> {
        if !a.cancel {
            affordable(&a.raw, cover)?;
        }
        let known = self.provider.get_transaction_by_hash(a.hash).await?;
        if known.is_some() {
            return Ok(());
        }
        if mined > a.nonce {
            self.paused.store(true, Relaxed);
            return Ok(());
        }
        // A refused broadcast is tried again at the next tick.
        let _ = self.provider.send_raw_transaction(&a.raw).await;
        Ok(())
    }

    /// Store `a` and the next nonce past it, and mark `id` sent, in one transaction.
    fn commit(&self, id: B256, a: &Attempt) -> Result<(), Error> {
        let mut db = self.db();
        let tx = db.transaction()?;
        tx.execute(
            "INSERT INTO attempts (hash, id, nonce, cancel, raw) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![&a.hash[..], &id[..], a.nonce, a.cancel, &a.raw[..]],
        )?;
        tx.execute("UPDATE nonce SET next = max(next, ?1)", [a.nonce + 1])?;
        tx.execute(
            "UPDATE requests SET status = 'sent' WHERE id = ?1",
            [&id[..]],
        )?;
        Ok(tx.commit()?)
    }

    fn set(
        &self,
        id: B256,
        status: &str,
        receipt: Option<(u64, bool)>,
        detail: Option<Vec<u8>>,
    ) -> Result<(), Error> {
        let (block, reverted) = receipt.unzip();
        self.db().execute(
            "UPDATE requests SET status = ?2, block = ?3, reverted = ?4, detail = ?5 \
             WHERE id = ?1",
            params![&id[..], status, block, reverted, detail],
        )?;
        Ok(())
    }

    fn attempts(&self, id: B256) -> Result<Vec<Attempt>, Error> {
        let sql =
            "SELECT hash, nonce, cancel, raw FROM attempts WHERE id = ?1 ORDER BY rowid";
        rows(&self.db(), sql, [&id[..]], |r| {
            let (hash, nonce, cancel) = (b256(r, 0)?, r.get(1)?, r.get(2)?);
            let raw = r.get::<_, Vec<u8>>(3)?.into();
            Ok(Attempt {
                hash,
                nonce,
                cancel,
                raw,
            })
        })
    }

    fn db(&self) -> MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// `mined` lies between the lowest nonce of a sent request and the next nonce, and
/// each stored nonce below it has a receipt for one of its attempts.
async fn explained<P: Provider>(
    provider: &P,
    db: &Connection,
    mined: u64,
    next: u64,
) -> Result<(), Error> {
    let sql = "SELECT a.nonce, a.hash FROM attempts a JOIN requests r ON a.id = r.id \
               WHERE r.status = 'sent'";
    let in_flight = rows(db, sql, [], |r| Ok((r.get::<_, u64>(0)?, b256(r, 1)?)))?;
    let below = in_flight.iter().filter(|a| a.0 < mined);
    let mut unexplained: BTreeSet<_> = below.clone().map(|a| a.0).collect();
    for (nonce, hash) in below {
        if provider.get_transaction_receipt(*hash).await?.is_some() {
            unexplained.remove(nonce);
        }
    }
    let lowest = in_flight.iter().map(|a| a.0).min().unwrap_or(next);
    if mined < lowest || mined > next || !unexplained.is_empty() {
        let drift = format!("nonce drift: mined {mined}, stored next {next}");
        return Err(format!("{drift}: run `relayer reconcile`").into());
    }
    Ok(())
}

/// `id` again when it is stored and not retriable; `WalletBusy` while another batch
/// of `wallet` is queued or sent.
#[allow(clippy::result_large_err)] // the `Refusal` that `relay` returns
fn admit(db: &Connection, id: B256, wallet: Address) -> Result<Option<B256>, Refusal> {
    let status = "SELECT status FROM requests WHERE id = ?1";
    let status = db.query_row(status, [&id[..]], first::<String>).optional();
    if status
        .map_err(node)?
        .is_some_and(|s| s != "failed" && s != "cancelled")
    {
        return Ok(Some(id));
    }
    let busy =
        "SELECT id FROM requests WHERE wallet = ?1 AND status IN ('queued', 'sent')";
    let busy = db.query_row(busy, [&wallet[..]], |r| b256(r, 0));
    match busy.optional().map_err(node)? {
        Some(id) => Err(Refusal::WalletBusy { id }),
        None => Ok(None),
    }
}

/// The send `raw` costs at most `R - value`: its gas at its cap plus the L1 bound.
fn affordable(raw: &[u8], (net, l1): Cover) -> Result<(), Error> {
    let tx = TxEnvelope::decode_2718(&mut &raw[..])?;
    let worst = U256::from(tx.gas_limit()) * U256::from(tx.max_fee_per_gas()) + l1;
    if worst > net {
        return Err(format!("a send costs up to {worst}, above R - value {net}").into());
    }
    Ok(())
}

fn rows<T>(
    db: &Connection,
    sql: &str,
    params: impl Params,
    row: impl FnMut(&Row) -> rusqlite::Result<T>,
) -> Result<Vec<T>, Error> {
    let mut statement = db.prepare(sql)?;
    let rows = statement.query_map(params, row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn first<T: rusqlite::types::FromSql>(row: &Row) -> rusqlite::Result<T> {
    row.get(0)
}

fn b256(row: &Row, i: usize) -> rusqlite::Result<B256> {
    Ok(B256::from_slice(&row.get::<_, Vec<u8>>(i)?))
}

fn u256(row: &Row, i: usize) -> rusqlite::Result<U256> {
    Ok(U256::from_be_slice(&row.get::<_, Vec<u8>>(i)?))
}

/// The `net` and `l1` columns at 2 and 3.
fn cover(row: &Row) -> rusqlite::Result<Cover> {
    Ok((u256(row, 2)?, u256(row, 3)?))
}

fn refused(refusal: Refusal) -> Error {
    format!("{refusal:?}").into()
}
