//! The sender: requests and their send attempts kept in SQLite, sent one at a time
//! by the one writer of the key's nonces, each at a pinned block, watched to a
//! receipt, and cancelled when their inclusion window ends.

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
};

use alloy::{
    consensus::{
        Transaction,
        TxEnvelope,
    },
    eips::{
        Decodable2718,
        Encodable2718,
    },
    network::{
        AnyTransactionReceipt,
        ReceiptResponse,
        TransactionBuilder,
    },
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
        rerun,
        reverted,
        target,
        Checked,
        Refusal,
        Request,
    },
    config::Config,
    gas::{
        self,
        Head,
    },
};

/// Why the relayer did not start or tick.
pub type Error = Box<dyn std::error::Error + Send + Sync>;

/// A request's `R - value` and the L1 fee bound its refund covers.
type Cover = (U256, U256);

/// The fee-history percentile a cancel's tip is read at.
const CANCEL_PERCENTILE: f64 = 90.0;

/// The schema version, `PRAGMA user_version`, a database must have.
const VERSION: i32 = 1;

const SCHEMA: &str = "
    CREATE TABLE requests (id BLOB PRIMARY KEY, wallet BLOB NOT NULL,
        tx TEXT NOT NULL, net BLOB NOT NULL, l1 BLOB NOT NULL, deadline INTEGER NOT NULL,
        status TEXT NOT NULL, block INTEGER, reverted INTEGER, failure TEXT, detail BLOB,
        created_at INTEGER NOT NULL DEFAULT (unixepoch()), gas_used INTEGER, gas_price BLOB, l1_fee BLOB,
        included_at INTEGER);
    CREATE TABLE attempts (hash BLOB PRIMARY KEY, id BLOB NOT NULL,
        nonce INTEGER NOT NULL, cancel INTEGER NOT NULL, raw BLOB NOT NULL,
        block INTEGER NOT NULL, at INTEGER NOT NULL,
        sent_at INTEGER NOT NULL DEFAULT (unixepoch()));
    CREATE INDEX attempts_nonce ON attempts (nonce);
    CREATE TABLE nonce (next INTEGER NOT NULL);";

/// A request in flight, over `requests r JOIN attempts a` and the key's mined count
/// `?1`: sent, or ended at a nonce the count no longer covers.
const IN_FLIGHT: &str =
    "r.status = 'sent' OR (r.status IN ('included', 'cancelled') AND a.nonce >= ?1)";

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
    /// Replaced by a cancel that landed; not queued again.
    Cancelled,
    /// Refused before its first broadcast; queued again when relayed again.
    Failed(Refusal),
}

/// A request's status and its transactions in the order they were signed.
#[derive(Debug)]
pub struct Report {
    /// Where the request stands.
    pub status: Status,
    /// Its send, then its cancels.
    pub tx_hashes: Vec<B256>,
    /// When each of `tx_hashes` was committed, in unix seconds.
    pub sent_at: Vec<u64>,
    /// When it was last queued, in unix seconds.
    pub created_at: u64,
    /// What its attempt with a receipt cost.
    pub cost: Option<Cost>,
}

/// A receipt's cost, in wei: `gas_used × gas_price + l1_fee`.
#[derive(Debug)]
pub struct Cost {
    /// The receipt's gas used.
    pub gas_used: u64,
    /// The receipt's effective gas price.
    pub gas_price: U256,
    /// The receipt's `l1Fee`; 0 on a chain without one.
    pub l1_fee: U256,
    /// When the relayer first saw the receipt, in unix seconds.
    pub included_at: u64,
}

/// A signed transaction, committed at its nonce before it is broadcast.
struct Attempt {
    hash: B256,
    nonce: u64,
    cancel: bool,
    raw: Bytes,
    /// The number and time of the block it was signed at.
    block: u64,
    at: u64,
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
    /// Lock and open `db`, and check its schema version and that the stored sends
    /// explain the key's mined nonce; a fresh database starts at it. `provider` signs
    /// with the relayer's key.
    pub async fn start(provider: P, config: Config, db: &Path) -> Result<Self, Error> {
        config.validate()?;
        let mut lock = db.as_os_str().to_owned();
        lock.push(".lock");
        let lock = PathBuf::from(lock);
        let file = File::create(&lock)?;
        let held = |_| format!("{} is held by another relayer", lock.display());
        file.try_lock().map_err(held)?;
        let mut conn = Connection::open(db)?;
        let tables: u64 =
            conn.query_row("SELECT count(*) FROM sqlite_master", [], first)?;
        let version: i32 = conn.query_row("PRAGMA user_version", [], first)?;
        if tables == 0 {
            let tx = conn.transaction()?;
            tx.execute_batch(SCHEMA)?;
            tx.pragma_update(None, "user_version", VERSION)?;
            tx.commit()?;
        } else if version != VERSION {
            let old = format!(
                "{} has schema version {version}, not {VERSION}",
                db.display()
            );
            return Err(format!("{old}: start on a fresh database file").into());
        }
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
    /// id; one that failed is queued again under it.
    pub async fn relay(&self, request: &Request) -> Result<B256, Refusal> {
        let (message, ..) = decode(&request.data).ok_or(Refusal::NoncanonicalData)?;
        let (p, c, wallet) = (&self.provider, &self.config, request.wallet);
        let call = target(p, c, wallet, request.create, &request.data, message.value);
        let id = request_id(c.chain_id, &call.await?, message.value);
        if let Some(id) = admit(&self.db(), id, wallet)? {
            return Ok(id);
        }
        let checked = check(p, c, request).await?;
        self.queue(id, wallet, &checked, message.deadline.to())
    }

    /// Store `checked` as `id`, queued, unless [`admit`] now answers for it.
    #[allow(clippy::result_large_err)] // the `Refusal` that `relay` returns
    fn queue(
        &self,
        id: B256,
        wallet: Address,
        checked: &Checked,
        deadline: u64,
    ) -> Result<B256, Refusal> {
        let tx = serde_json::to_string(&checked.tx).map_err(node)?;
        let (net, l1) = (checked.net, checked.l1_fee_bound);
        let (net, l1) = (net.to_be_bytes::<32>(), l1.to_be_bytes::<32>());
        let db = self.db();
        if let Some(id) = admit(&db, id, wallet)? {
            return Ok(id);
        }
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

    /// Resolve or rebroadcast each request in flight, then send the oldest queued one
    /// when none is in flight and the key's count at the pinned block is the stored
    /// next nonce. Above it a foreign nonce pauses sending; below it ours is unmined.
    pub async fn tick(&self) -> Result<(), Error> {
        let head = gas::head(&self.provider).await.map_err(refused)?;
        let mined = self.provider.get_transaction_count(self.config.relayer);
        let mined = mined.block_id(head.at()).await?;
        for (id, deadline, cover) in self.in_flight(mined)? {
            self.watch(id, deadline, cover, &head, mined).await?;
        }
        let next = self.db().query_row("SELECT next FROM nonce", [], first)?;
        if mined > next {
            self.paused.store(true, Relaxed);
        }
        if self.paused.load(Relaxed) || mined < next || !self.in_flight(mined)?.is_empty()
        {
            return Ok(());
        }
        let Some((id, tx, cover, deadline)) = self.queued()? else {
            return Ok(());
        };
        let tx = serde_json::from_str(&tx)?;
        self.send(id, tx, cover, deadline, &head, next).await
    }

    /// The requests in flight at the mined count: id, deadline, cover.
    fn in_flight(&self, mined: u64) -> Result<Vec<(B256, u64, Cover)>, Error> {
        let sql = format!(
            "SELECT DISTINCT r.id, r.deadline, r.net, r.l1 FROM requests r \
             JOIN attempts a ON a.id = r.id WHERE {IN_FLIGHT}"
        );
        let row = |r: &Row| Ok((b256(r, 0)?, r.get(1)?, cover(r)?));
        rows(&self.db(), &sql, [mined], row)
    }

    /// The oldest queued request: id, transaction, cover, deadline.
    fn queued(&self) -> Result<Option<(B256, String, Cover, u64)>, Error> {
        let sql = "SELECT id, tx, net, l1, deadline FROM requests \
                   WHERE status = 'queued' ORDER BY rowid LIMIT 1";
        let row = |r: &Row| Ok((b256(r, 0)?, r.get(1)?, cover(r)?, r.get(4)?));
        Ok(self.db().query_row(sql, [], row).optional()?)
    }

    /// `id`'s status and its transactions.
    pub async fn status(&self, id: B256) -> Result<Report, Error> {
        let sql = "SELECT status, block, reverted, failure, detail, created_at, \
                   gas_used, gas_price, l1_fee, included_at FROM requests WHERE id = ?1";
        let row = |r: &Row| {
            let (block, reverted) =
                (r.get::<_, Option<_>>(1)?, r.get::<_, Option<_>>(2)?);
            let failure = (r.get::<_, Option<String>>(3)?, r.get(4)?);
            let cost = match r.get::<_, Option<u64>>(6)? {
                None => None,
                Some(gas_used) => Some(Cost {
                    gas_used,
                    gas_price: u256(r, 7)?,
                    l1_fee: u256(r, 8)?,
                    included_at: r.get(9)?,
                }),
            };
            Ok((
                first::<String>(r)?,
                block.unwrap_or(0),
                reverted.unwrap_or(false),
                failure,
                r.get(5)?,
                cost,
            ))
        };
        let (status, block, reverted, (failure, detail), created_at, cost) =
            self.db().query_row(sql, [&id[..]], row)?;
        let status = match &status[..] {
            "queued" => Status::Queued,
            "sent" => Status::Sent,
            "included" => Status::Included { block, reverted },
            "cancelled" => Status::Cancelled,
            _ => Status::Failed(decode_failure(&failure.unwrap_or_default(), detail)),
        };
        let sql = "SELECT hash, sent_at FROM attempts WHERE id = ?1 ORDER BY rowid";
        let sent = rows(&self.db(), sql, [&id[..]], |r| {
            Ok((b256(r, 0)?, r.get::<_, u64>(1)?))
        })?;
        let (tx_hashes, sent_at) = sent.into_iter().unzip();
        Ok(Report {
            status,
            tx_hashes,
            sent_at,
            created_at,
            cost,
        })
    }

    /// Whether sending paused: a foreign nonce, a low balance, a refused first send or
    /// a cancel above the max cancel fee.
    pub async fn paused(&self) -> bool {
        self.paused.load(Relaxed)
    }

    /// Resolve `id` from a receipt of any of its attempts. With none, broadcast its send
    /// again until its window ends, then cancel it, raising the cancel each new block.
    async fn watch(
        &self,
        id: B256,
        deadline: u64,
        cover: Cover,
        head: &Head,
        mined: u64,
    ) -> Result<(), Error> {
        let attempts = self.attempts(id)?;
        for a in &attempts {
            // Read as any receipt, which keeps an OP-stack `l1Fee`.
            let get = self
                .provider
                .raw_request("eth_getTransactionReceipt".into(), (a.hash,));
            let Some(receipt): Option<AnyTransactionReceipt> = get.await? else {
                continue;
            };
            let block = receipt.block_number.unwrap_or_default();
            let status = if a.cancel { "cancelled" } else { "included" };
            return self.set_status(id, status, Some((block, &receipt)), None);
        }
        self.set_status(id, "sent", None, None)?; // a receipt a reorg removed
        let (Some(send), Some(last)) = (attempts.first(), attempts.last()) else {
            return Ok(());
        };
        let late = self.late(send.at);
        if !last.cancel && head.time < late {
            let run = Some((deadline, late));
            return self.broadcast(last, cover, run, head, mined).await;
        }
        if last.cancel && head.number <= last.block {
            return self.broadcast(last, cover, None, head, mined).await;
        }
        if mined > last.nonce {
            self.paused.store(true, Relaxed); // another sender's at our nonce
            return Ok(());
        }
        self.cancel(id, last, head, mined).await
    }

    /// Sign `tx` at `nonce` with its fee cap at most M × the pinned base fee and the
    /// tip read now, run it again at the pinned block, then commit and broadcast it.
    async fn send(
        &self,
        id: B256,
        tx: TransactionRequest,
        cover: Cover,
        deadline: u64,
        head: &Head,
        nonce: u64,
    ) -> Result<(), Error> {
        let cap = tx.max_fee_per_gas.unwrap_or_default();
        let cap = cap.min(head.fee_bound(&self.config));
        let tip = gas::tip(&self.provider, self.config.tip_percentile).await;
        let tx = tx
            .from(self.config.relayer)
            .nonce(nonce)
            .max_fee_per_gas(cap)
            .max_priority_fee_per_gas(tip.map_err(refused)?.min(cap));
        let attempt = self.sign(tx, false, head).await?;
        let worst = affordable(&attempt.raw, cover)?;
        let balance = self.provider.get_balance(self.config.relayer);
        let balance = balance.block_id(head.at()).await?;
        if balance < worst {
            self.paused.store(true, Relaxed);
            let short = format!("balance {balance} below a send's cost {worst}");
            return Err(format!("{short}: sending paused; fund it and restart").into());
        }
        let run = self.runs(&attempt.raw, deadline, head, self.late(head.time));
        match run.await {
            Err(Refusal::Node(e)) => return Err(e.into()),
            Err(refusal) => return self.set_status(id, "failed", None, Some(&refusal)),
            Ok(()) => {}
        }
        self.commit(id, &attempt)?;
        self.broadcast(&attempt, cover, None, head, nonce).await
    }

    /// A 0-value self-send at `last`'s nonce, both fees at least 1.1× `last`'s and its
    /// cap twice the pinned base fee, up to the max cancel fee. Above it sending pauses
    /// and the last cancel is sent again.
    async fn cancel(
        &self,
        id: B256,
        last: &Attempt,
        head: &Head,
        mined: u64,
    ) -> Result<(), Error> {
        let tx = TxEnvelope::decode_2718(&mut &last.raw[..])?;
        let more = |fee: u128| fee.saturating_mul(11).div_ceil(10);
        let (least, max) = (more(tx.max_fee_per_gas()), self.config.max_cancel_fee);
        if least > max {
            self.paused.store(true, Relaxed);
            if last.cancel {
                self.broadcast(last, Default::default(), None, head, mined)
                    .await?;
            }
            let n = last.nonce;
            let above = format!("the cancel at nonce {n} needs a fee cap above {max}");
            return Err(format!("{above}: sending paused; raise max_cancel_fee").into());
        }
        let cap = least.max(head.base_fee.saturating_mul(2)).min(max);
        let tip = gas::tip(&self.provider, CANCEL_PERCENTILE)
            .await
            .map_err(refused)?;
        let tip = more(tx.max_priority_fee_per_gas().unwrap_or_default()).max(tip);
        let relayer = self.config.relayer;
        let cancel = TransactionRequest::default()
            .from(relayer)
            .to(relayer)
            .nonce(last.nonce)
            .gas_limit(21_000)
            .max_fee_per_gas(cap)
            .max_priority_fee_per_gas(tip.min(cap))
            .with_chain_id(self.config.chain_id);
        let attempt = self.sign(cancel, true, head).await?;
        self.commit(id, &attempt)?;
        self.broadcast(&attempt, Default::default(), None, head, mined)
            .await
    }

    /// The end of the inclusion window of a send signed at `at`.
    fn late(&self, at: u64) -> u64 {
        at + self.config.inclusion_window.as_secs()
    }

    async fn sign(
        &self,
        tx: TransactionRequest,
        cancel: bool,
        head: &Head,
    ) -> Result<Attempt, Error> {
        let nonce = tx.nonce.unwrap_or_default();
        let signed = self.provider.fill_and_sign_transaction(tx).await?;
        let (hash, raw) = (*signed.tx_hash(), signed.encoded_2718().into());
        Ok(Attempt {
            hash,
            nonce,
            cancel,
            raw,
            block: head.number,
            at: head.time,
        })
    }

    /// `raw`, as it is sent but at any nonce, still runs at `head` and at `late`.
    async fn runs(
        &self,
        raw: &[u8],
        deadline: u64,
        head: &Head,
        late: u64,
    ) -> Result<(), Refusal> {
        let tx = TxEnvelope::decode_2718(&mut &raw[..]).map_err(node)?;
        let mut tx =
            TransactionRequest::from_transaction_with_sender(tx, self.config.relayer);
        tx.nonce = None;
        rerun(&self.provider, &tx, deadline, head.at(), late).await
    }

    /// Broadcast `a` unless the node has it. Its nonce mined with no receipt of ours
    /// is another sender's, and sending pauses. With `run`, a send's deadline and
    /// window end, a send that no longer runs at `head` is left to its cancel.
    async fn broadcast(
        &self,
        a: &Attempt,
        cover: Cover,
        run: Option<(u64, u64)>,
        head: &Head,
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
        if let Some((deadline, late)) = run {
            match self.runs(&a.raw, deadline, head, late).await {
                Err(Refusal::Node(e)) => return Err(e.into()),
                Err(_) => return Ok(()),
                Ok(()) => {}
            }
        }
        let Err(e) = self.provider.send_raw_transaction(&a.raw).await else {
            return Ok(());
        };
        // The node may hold `a` unless it answered with an error and has no such hash.
        let first = run.is_none() && !a.cancel;
        let rejected = e.as_error_resp().is_some()
            && self
                .provider
                .get_transaction_by_hash(a.hash)
                .await?
                .is_none();
        if !first || !rejected {
            eprintln!("broadcast of {} refused, retried next tick: {e}", a.hash);
            return Ok(());
        }
        self.uncommit(a)?;
        if e.as_error_resp()
            .is_some_and(|r| r.message.contains("nonce too low"))
        {
            return Ok(()); // checked again from the next tick's block
        }
        self.paused.store(true, Relaxed);
        Err(format!("the node refused send {}: {e}; sending paused", a.hash).into())
    }

    /// Undo [`Self::commit`] of a first send: drop it, free its nonce, queue it again.
    fn uncommit(&self, a: &Attempt) -> Result<(), Error> {
        let mut db = self.db();
        let tx = db.transaction()?;
        tx.execute(
            "UPDATE requests SET status = 'queued' \
             WHERE id = (SELECT id FROM attempts WHERE hash = ?1)",
            [&a.hash[..]],
        )?;
        tx.execute("DELETE FROM attempts WHERE hash = ?1", [&a.hash[..]])?;
        tx.execute("UPDATE nonce SET next = ?1 WHERE next = ?1 + 1", [a.nonce])?;
        Ok(tx.commit()?)
    }

    /// Store `a` and the next nonce past it, and mark `id` sent, in one transaction.
    fn commit(&self, id: B256, a: &Attempt) -> Result<(), Error> {
        let mut db = self.db();
        let tx = db.transaction()?;
        tx.execute(
            "INSERT INTO attempts (hash, id, nonce, cancel, raw, block, at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                &a.hash[..],
                &id[..],
                a.nonce,
                a.cancel,
                &a.raw[..],
                a.block,
                a.at
            ],
        )?;
        tx.execute("UPDATE nonce SET next = max(next, ?1)", [a.nonce + 1])?;
        tx.execute(
            "UPDATE requests SET status = 'sent' WHERE id = ?1",
            [&id[..]],
        )?;
        Ok(tx.commit()?)
    }

    fn set_status(
        &self,
        id: B256,
        status: &str,
        receipt: Option<(u64, &AnyTransactionReceipt)>,
        failed: Option<&Refusal>,
    ) -> Result<(), Error> {
        let (block, receipt) = receipt.unzip();
        let (failure, detail) = failed.map(encode_failure).unzip();
        let l1 = receipt.and_then(|r| r.other.get_deserialized::<U256>("l1Fee"));
        let l1 = l1.transpose()?.unwrap_or_default().to_be_bytes::<32>();
        let wei = |r: &AnyTransactionReceipt| U256::from(r.effective_gas_price);
        let (gas, price) = (receipt.map(|r| r.gas_used), receipt.map(wei));
        let (reverted, l1) = (receipt.map(|r| !r.status()), receipt.map(|_| l1));
        let price = price.map(|p| p.to_be_bytes::<32>());
        self.db().execute(
            "UPDATE requests SET status = ?2, block = ?3, reverted = ?4, failure = ?5, \
             detail = ?6, gas_used = ?7, gas_price = ?8, l1_fee = ?9, included_at = \
             CASE WHEN ?3 IS NULL THEN NULL ELSE coalesce(included_at, unixepoch()) END \
             WHERE id = ?1",
            params![
                &id[..],
                status,
                block,
                reverted,
                failure,
                detail,
                gas,
                price,
                l1
            ],
        )?;
        Ok(())
    }

    fn attempts(&self, id: B256) -> Result<Vec<Attempt>, Error> {
        let sql =
            "SELECT hash, nonce, cancel, raw, block, at FROM attempts WHERE id = ?1 \
                   ORDER BY rowid";
        rows(&self.db(), sql, [&id[..]], |r| {
            let (hash, nonce, cancel) = (b256(r, 0)?, r.get(1)?, r.get(2)?);
            let raw = r.get::<_, Vec<u8>>(3)?.into();
            Ok(Attempt {
                hash,
                nonce,
                cancel,
                raw,
                block: r.get(4)?,
                at: r.get(5)?,
            })
        })
    }

    fn db(&self) -> MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A request's id: its chain, target, calldata and value.
fn request_id(chain_id: u64, call: &TransactionRequest, value: U256) -> B256 {
    let to = TransactionBuilder::to(call).unwrap_or_default();
    let input = TransactionBuilder::input(call).cloned().unwrap_or_default();
    keccak256((U256::from(chain_id), to, input, value).abi_encode_params())
}

/// `mined` lies between the lowest nonce in flight and the next nonce, and each nonce
/// in flight below it has a receipt for one of its attempts.
async fn explained<P: Provider>(
    provider: &P,
    db: &Connection,
    mined: u64,
    next: u64,
) -> Result<(), Error> {
    let sql = format!(
        "SELECT a.nonce, a.hash FROM attempts a JOIN requests r ON a.id = r.id \
         WHERE {IN_FLIGHT}"
    );
    let in_flight = rows(db, &sql, [mined], |r| {
        Ok((r.get::<_, u64>(0)?, b256(r, 1)?))
    })?;
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
        let fix =
            "wait for or inspect the key's nonce, then start on a fresh database file";
        return Err(format!("{drift}: {fix}").into());
    }
    Ok(())
}

/// `id` again when it is stored and has not failed; `WalletBusy` while another batch
/// of `wallet` is queued or sent.
#[allow(clippy::result_large_err)] // the `Refusal` that `relay` returns
fn admit(db: &Connection, id: B256, wallet: Address) -> Result<Option<B256>, Refusal> {
    let status = "SELECT status FROM requests WHERE id = ?1";
    let status = db.query_row(status, [&id[..]], first::<String>).optional();
    if status.map_err(node)?.is_some_and(|s| s != "failed") {
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
/// Returns that cost plus its value, the most it takes from the balance.
fn affordable(raw: &[u8], (net, l1): Cover) -> Result<U256, Error> {
    let tx = TxEnvelope::decode_2718(&mut &raw[..])?;
    let worst = U256::from(tx.gas_limit()) * U256::from(tx.max_fee_per_gas()) + l1;
    if worst > net {
        return Err(format!("a send costs up to {worst}, above R - value {net}").into());
    }
    Ok(worst + tx.value())
}

/// A failure as stored: its name and its fields.
fn encode_failure(refusal: &Refusal) -> (String, Vec<u8>) {
    match refusal {
        Refusal::Reverted { data, .. } => ("Reverted".into(), data.to_vec()),
        other => (format!("{other:?}"), vec![]),
    }
}

/// The failure [`encode_failure`] stored: what a send's run refuses.
fn decode_failure(name: &str, detail: Option<Vec<u8>>) -> Refusal {
    let detail = detail.unwrap_or_default();
    match name {
        "Reverted" => reverted(detail.into()),
        _ => Refusal::Expired,
    }
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
