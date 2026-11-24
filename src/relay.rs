//! The sender: requests and their send attempts kept in SQLite, sent one at a time
//! by the one writer of the key's nonces, each at a pinned block, watched to a
//! receipt, and cancelled when their inclusion window ends.

use std::{
    collections::{
        BTreeMap,
        BTreeSet,
    },
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

/// A queued request: id, transaction, cover, deadline.
type Queued = (B256, String, Cover, u64);

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

/// What a tick leaves for the next, behind the lock that runs one tick at a time.
#[derive(Default)]
struct Ticking {
    /// The balance at the last pinned block is below `min_balance`.
    low: bool,
    /// The last request whose run met a node error, and the ticks in a row it did.
    errors: (B256, u32),
    /// Our nonces mined with no receipt of ours, and the block each was first seen at.
    missing: BTreeMap<u64, u64>,
    /// One of them still had none `receipt_lag_blocks` later.
    unexplained: bool,
}

impl Ticking {
    /// Count a tick of node errors on `id`'s run: the ticks in a row.
    fn strike(&mut self, id: B256) -> u32 {
        let n = if self.errors.0 == id {
            self.errors.1
        } else {
            0
        };
        self.errors = (id, n + 1);
        n + 1
    }
}

/// The relayer of one chain and one key, the only writer of the key's nonces
/// while it holds the lock on its database.
pub struct Relayer<P> {
    provider: P,
    config: Config,
    db: Mutex<Connection>,
    paused: AtomicBool,
    ticking: tokio::sync::Mutex<Ticking>,
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
        let growth = gas::window_growth(&provider, &config)
            .await
            .map_err(refused)?;
        let m = config.max_fee_percent;
        if m < growth {
            let under = format!("max_fee_percent {m} is under the {growth}% fee growth");
            eprintln!(
                "{under} over inclusion_window; sends cap at that growth: raise it"
            );
        }
        Ok(Self {
            provider,
            config,
            db: Mutex::new(conn),
            paused: AtomicBool::new(false),
            ticking: Default::default(),
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
        let checked = request.check(p, c).await?;
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
    /// A balance below `min_balance` pauses sending until a tick finds it funded.
    pub async fn tick(&self) -> Result<(), Error> {
        let mut ticking = self.ticking.lock().await;
        let head = gas::head(&self.provider, &self.config)
            .await
            .map_err(refused)?;
        let relayer = self.config.relayer;
        let mined = self.provider.get_transaction_count(relayer);
        let mined = mined.block_id(head.at()).await?;
        let mut missing = BTreeMap::new();
        for (id, deadline, cover) in self.in_flight(mined)? {
            if let Some(n) = self.watch(id, deadline, cover, &head, mined).await? {
                missing.insert(n, *ticking.missing.get(&n).unwrap_or(&head.number));
            }
        }
        let lag = self.config.receipt_lag_blocks;
        ticking.unexplained = missing.values().any(|at| head.number >= at + lag);
        ticking.missing = missing;
        let next = self.db().query_row("SELECT next FROM nonce", [], first)?;
        if mined > next {
            self.paused.store(true, Relaxed);
        }
        let balance = self
            .provider
            .get_balance(relayer)
            .block_id(head.at())
            .await?;
        let floor = self.config.min_balance;
        ticking.low = balance < floor;
        if self.paused.load(Relaxed) || mined < next || !self.in_flight(mined)?.is_empty()
        {
            return Ok(());
        }
        if ticking.low {
            let low = format!("balance {balance} below min_balance {floor}");
            return Err(format!("{low}: sending paused until it is funded").into());
        }
        while let Some(queued) = self.queued()? {
            if !self
                .send(&mut ticking, queued, &head, next, balance)
                .await?
            {
                break;
            }
        }
        Ok(())
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

    /// The oldest queued request.
    fn queued(&self) -> Result<Option<Queued>, Error> {
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

    /// Whether sending paused: a foreign nonce, a balance below `min_balance`, a
    /// refused first send, or our nonce mined with no receipt of ours for
    /// `receipt_lag_blocks`.
    pub async fn paused(&self) -> bool {
        let ticking = self.ticking.lock().await;
        self.paused.load(Relaxed) || ticking.low || ticking.unexplained
    }

    /// Resolve `id` from a receipt of any of its attempts. With none, broadcast its send
    /// again until its window ends or it no longer runs, then cancel it. Its nonce
    /// when mined with no receipt of ours: another sender's, or a lagging node's.
    async fn watch(
        &self,
        id: B256,
        deadline: u64,
        cover: Cover,
        head: &Head,
        mined: u64,
    ) -> Result<Option<u64>, Error> {
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
            self.set_status(id, status, Some((block, &receipt)), None)?;
            return Ok(None);
        }
        self.set_status(id, "sent", None, None)?; // a receipt a reorg removed
        let (Some(send), Some(last)) = (attempts.first(), attempts.last()) else {
            return Ok(None);
        };
        if mined > last.nonce {
            return Ok(Some(last.nonce));
        }
        let late = self.late(send.at);
        let run = Some((deadline, late));
        if !last.cancel
            && head.time < late
            && self.broadcast(last, cover, run, head).await?
        {
            return Ok(None);
        }
        self.cancel(id, last, head).await.map(|()| None)
    }

    /// Sign the queued transaction at `nonce` with its fee cap at most the pinned
    /// block's fee bound and the tip read now, run it again at the pinned block, then
    /// commit and broadcast it. True when it failed instead, and the next may be sent:
    /// its refund no longer covering the floor at the L1 bound read now, its run
    /// failing for good, or a node error on its run for `node_error_ticks` ticks in a
    /// row.
    async fn send(
        &self,
        ticking: &mut Ticking,
        (id, tx, (net, l1), deadline): Queued,
        head: &Head,
        nonce: u64,
        balance: U256,
    ) -> Result<bool, Error> {
        let tx: TransactionRequest = serde_json::from_str(&tx)?;
        let (limit, value) = (tx.gas.unwrap_or_default(), tx.value.unwrap_or_default());
        let refund = net + value;
        // A node error here is not the request's, so it counts no strike.
        let now = gas::l1_fee_bound(&self.provider, &self.config, &tx, limit, head.at());
        let now = now.await.map_err(refused)?;
        // The cap the refund covers at the bound read now and, as stored, at the bound
        // a rebroadcast is checked against.
        let covered = net.saturating_sub(now) / U256::from(limit);
        let covered = u128::try_from(covered).unwrap_or(u128::MAX);
        let cap = tx.max_fee_per_gas.unwrap_or_default().min(covered);
        let cap = cap.min(head.fee_bound(&self.config));
        if cap < head.floor {
            let gas = U256::from(limit) * U256::from(head.floor);
            let required = value + gas + now.max(l1);
            return self.fail(id, &Refusal::RefundTooLow { refund, required });
        }
        let tip = gas::tip(&self.provider, self.config.tip_percentile).await;
        let tx = tx
            .from(self.config.relayer)
            .nonce(nonce)
            .max_fee_per_gas(cap)
            .max_priority_fee_per_gas(tip.map_err(refused)?.min(cap));
        let attempt = self.sign(tx, false, head).await?;
        let required = affordable(&attempt.raw, (net, now))?;
        if balance < required {
            return self.fail(id, &Refusal::RelayerUnderfunded { balance, required });
        }
        let run = self.runs(&attempt.raw, deadline, head, self.late(head.time));
        let n = self.config.node_error_ticks;
        match run.await {
            Err(Refusal::Node(e)) if !deterministic(&e) && ticking.strike(id) < n => {
                return Err(e.into());
            }
            Err(refusal) => return self.fail(id, &refusal),
            Ok(()) => {}
        }
        self.commit(id, &attempt)?;
        self.broadcast(&attempt, (net, l1), None, head).await?;
        Ok(false)
    }

    /// Mark `id` failed for `refusal`; true, as the next may be sent.
    fn fail(&self, id: B256, refusal: &Refusal) -> Result<bool, Error> {
        self.set_status(id, "failed", None, Some(refusal))?;
        Ok(true)
    }

    /// A 0-value self-send at `last`'s nonce. The last cancel is sent again while the
    /// pinned base fee is at most its cap; past it, a new one is signed with its cap
    /// twice the base fee up to the max cancel fee, and its tip the market's; both at
    /// least 1.1× `last`'s.
    async fn cancel(&self, id: B256, last: &Attempt, head: &Head) -> Result<(), Error> {
        let tx = TxEnvelope::decode_2718(&mut &last.raw[..])?;
        if last.cancel && head.base_fee <= tx.max_fee_per_gas() {
            let again = self.broadcast(last, Default::default(), None, head);
            return again.await.map(drop);
        }
        let more = |fee: u128| fee.saturating_mul(11).div_ceil(10);
        let percent = self.config.max_cancel_fee_percent.into();
        let max = head.base_fee.saturating_mul(percent) / 100;
        // Above `max` only after the base fee fell: within it at the send's base fee.
        let cap = head.base_fee.saturating_mul(2).min(max);
        let cap = cap.max(more(tx.max_fee_per_gas()));
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
        self.broadcast(&attempt, Default::default(), None, head)
            .await
            .map(drop)
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

    /// Broadcast `a` unless the node has it. With `run`, a send's deadline and window
    /// end, false when a send no longer runs at `head`, to cancel at once.
    async fn broadcast(
        &self,
        a: &Attempt,
        cover: Cover,
        run: Option<(u64, u64)>,
        head: &Head,
    ) -> Result<bool, Error> {
        if !a.cancel {
            affordable(&a.raw, cover)?;
        }
        let known = self.provider.get_transaction_by_hash(a.hash).await?;
        if known.is_some() {
            return Ok(true);
        }
        if let Some((deadline, late)) = run {
            match self.runs(&a.raw, deadline, head, late).await {
                Err(Refusal::Node(e)) => return Err(e.into()),
                Err(_) => return Ok(false),
                Ok(()) => {}
            }
        }
        let Err(e) = self.provider.send_raw_transaction(&a.raw).await else {
            return Ok(true);
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
            return Ok(true);
        }
        self.uncommit(a)?;
        if e.as_error_resp()
            .is_some_and(|r| r.message.contains("nonce too low"))
        {
            return Ok(true); // checked again from the next tick's block
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

/// A node error that the same run meets again: one naming fees, gas or funds.
fn deterministic(e: &str) -> bool {
    let e = e.to_lowercase();
    ["fee", "gas", "funds"].iter().any(|w| e.contains(w))
}

/// A failure as stored: its name and its fields.
fn encode_failure(refusal: &Refusal) -> (String, Vec<u8>) {
    let words = |a: &U256, b: &U256| [a.to_be_bytes::<32>(), b.to_be_bytes()].concat();
    match refusal {
        Refusal::Reverted { data, .. } => ("Reverted".into(), data.to_vec()),
        Refusal::RefundTooLow { refund, required } => {
            ("RefundTooLow".into(), words(refund, required))
        }
        Refusal::RelayerUnderfunded { balance, required } => {
            ("RelayerUnderfunded".into(), words(balance, required))
        }
        Refusal::Node(e) => ("Node".into(), e.as_bytes().to_vec()),
        other => (format!("{other:?}"), vec![]),
    }
}

/// The failure [`encode_failure`] stored: what a send refuses.
fn decode_failure(name: &str, detail: Option<Vec<u8>>) -> Refusal {
    let detail = detail.unwrap_or_default();
    let word =
        |i: usize| U256::from_be_slice(detail.get(i * 32..i * 32 + 32).unwrap_or(&[]));
    let (first, second) = (word(0), word(1));
    match name {
        "Reverted" => reverted(detail.into()),
        "RefundTooLow" => Refusal::RefundTooLow {
            refund: first,
            required: second,
        },
        "RelayerUnderfunded" => Refusal::RelayerUnderfunded {
            balance: first,
            required: second,
        },
        "Node" => Refusal::Node(String::from_utf8_lossy(&detail).into()),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_send_costing_exactly_its_cover_is_affordable() {
        let mut tx = alloy::consensus::TxEip1559::default();
        (tx.gas_limit, tx.max_fee_per_gas, tx.value) = (100, 7, U256::from(5));
        let signature = alloy::primitives::Signature::test_signature();
        let raw = TxEnvelope::new_unhashed(tx.into(), signature).encoded_2718();
        let (net, l1) = (U256::from(703), U256::from(3));
        assert_eq!(affordable(&raw, (net, l1)).unwrap(), U256::from(708));
        assert!(affordable(&raw, (net - U256::ONE, l1)).is_err());
    }
}
