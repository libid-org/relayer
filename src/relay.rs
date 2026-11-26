//! The sender: one SQLite row per genesis identity, holding its request, its signed
//! send and cancels, and its loss; sent one at a time by the one writer of the key's
//! nonces, each at a pinned block, watched to a receipt, and cancelled when its
//! inclusion window ends. A paid row goes `receipt_lag_blocks` after its receipt, a
//! row with a loss after `loss_hold`, refusing its identity until then.

use std::{
    collections::{
        BTreeMap,
        BTreeSet,
    },
    fmt,
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
        Transaction,
        TxEnvelope,
    },
    eips::{
        BlockId,
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
        answered,
        decode,
        node,
        outer,
        refusal,
        rerun,
        reverted,
        vouched,
        Checked,
        Identity,
        Refusal,
        Request,
    },
    config::Config,
    gas::{
        self,
        Head,
        OperatorFee,
    },
    logs,
};

/// Why the relayer did not start or tick.
pub type Error = Box<dyn std::error::Error + Send + Sync>;

/// An [`Error`] a caller may tell apart.
#[derive(Debug)]
pub enum Stop {
    /// No request has the id.
    Unknown,
    /// A tick found the balance below `min_balance`, and sending paused.
    LowBalance(String),
    /// A tick found the OP-stack operator fee on, and sending paused.
    OperatorFee(String),
}

impl fmt::Display for Stop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unknown => "no such request",
            Self::LowBalance(why) | Self::OperatorFee(why) => why,
        })
    }
}

impl std::error::Error for Stop {}

/// A request's `R - value` and the L1 fee bound its refund covers.
type Cover = (U256, U256);

/// A queued request: id, transaction, cover, deadline, wallet.
type Queued = (B256, String, Cover, u64, Address);

/// The fee-history percentile a cancel's tip is read at.
const CANCEL_PERCENTILE: f64 = 90.0;

/// How long a tick goes on sending the queue once a send in it failed; the rest wait
/// for the next tick.
const FAILING_TICK: Duration = Duration::from_secs(5);

/// The schema version, `PRAGMA user_version`, a database must have.
const VERSION: i32 = 6;

/// The tables, created in a new database.
const SCHEMA: &str = include_str!("schema.sql");

/// A row in flight at the key's mined count `?1`: sent, or ended at a nonce the count
/// no longer covers.
const IN_FLIGHT: &str =
    "status = 'sent' OR (status IN ('included', 'cancelled') AND nonce >= ?1)";

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
    /// Refused before or at its first broadcast, or charged at its worst as `Node` when
    /// its receipt did not parse.
    Failed(Refusal),
}

/// A request's status and its transactions in the order they were signed.
#[derive(Debug)]
pub struct Report {
    /// Where the request stands.
    pub status: Status,
    /// Its send, then its cancels.
    pub tx_hashes: Vec<B256>,
}

/// A signed transaction, committed at its nonce before it is broadcast.
struct Attempt {
    hash: B256,
    nonce: u64,
    cancel: bool,
    raw: Bytes,
}

impl Attempt {
    /// The stored `raw`, a cancel when `cancel`.
    fn read(raw: Bytes, cancel: bool) -> Result<Self, Error> {
        let tx = TxEnvelope::decode_2718(&mut &raw[..])?;
        Ok(Self {
            hash: *tx.tx_hash(),
            nonce: tx.nonce(),
            cancel,
            raw,
        })
    }
}

/// A row's signed transactions.
struct Sent {
    nonce: u64,
    /// The send's, then each cancel's.
    hashes: Vec<B256>,
    send: Bytes,
    /// The last cancel signed.
    cancel: Option<Bytes>,
    /// The time of the block the send was signed at.
    signed_at: u64,
}

impl Sent {
    /// The last transaction signed: the last cancel, or the send.
    fn last(&self) -> Result<Attempt, Error> {
        match &self.cancel {
            Some(cancel) => Attempt::read(cancel.clone(), true),
            None => Attempt::read(self.send.clone(), false),
        }
    }
}

/// What a tick leaves for the next, behind the lock that runs one tick at a time.
#[derive(Default)]
struct Ticking {
    /// Node errors on a queued request's run.
    send: Strikes,
    /// Receipts of a request that did not parse.
    receipt: Strikes,
    /// Our nonces mined with no receipt of ours, and the block each was first seen at.
    missing: BTreeMap<u64, u64>,
    /// The block time a send in flight at start, or one a reorg removed, was first
    /// watched at, while it stays in flight: its window begins there.
    since: BTreeMap<B256, u64>,
}

/// Each request's ticks in a row with an error.
#[derive(Default)]
struct Strikes(BTreeMap<B256, u32>);

impl Strikes {
    /// Count a tick with an error on `id`: its ticks in a row.
    fn strike(&mut self, id: B256) -> u32 {
        let n = self.0.entry(id).or_default();
        *n += 1;
        *n
    }

    fn clear(&mut self, id: B256) {
        self.0.remove(&id);
    }
}

/// The relayer of one chain and one key, the only writer of the key's nonces
/// while it holds the lock on its database.
pub struct Relayer<P> {
    pub(crate) provider: P,
    config: Config,
    db: Mutex<Connection>,
    /// The sends in flight at start, by their hash: sent again until their deadline.
    resumed: BTreeSet<B256>,
    paused: AtomicBool,
    /// The balance at the last pinned block is below `min_balance`.
    low: AtomicBool,
    /// One of our nonces mined with no receipt of ours `receipt_lag_blocks` later.
    unexplained: AtomicBool,
    /// The OP-stack operator fee was on at the last read.
    operator_fee: AtomicBool,
    /// The balance at the last pinned block.
    balance: Mutex<Option<U256>>,
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
        let held = |_| {
            let held = format!("{} is held by another relayer", lock.display());
            format!("{held}: stop it, or give this one another state.db")
        };
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
        let sql = format!("SELECT send FROM identities WHERE {IN_FLIGHT}");
        let resumed = rows(&conn, &sql, [mined], |r| r.get::<_, Vec<u8>>(0))?;
        let resumed = resumed
            .iter()
            .map(|raw| Ok(Attempt::read(raw.clone().into(), false)?.hash));
        let resumed = resumed.collect::<Result<_, Error>>()?;
        let growth = gas::window_growth(&provider, &config)
            .await
            .map_err(refused)?;
        let m = config.max_fee_percent;
        if m < growth {
            let under = format!("max_fee_percent {m} is under the {growth}% fee growth");
            tracing::warn!(
                "{under} over inclusion_window; sends cap at that growth: raise it"
            );
        }
        Ok(Self {
            provider,
            config,
            db: Mutex::new(conn),
            resumed,
            paused: AtomicBool::new(false),
            low: AtomicBool::new(false),
            unexplained: AtomicBool::new(false),
            operator_fee: AtomicBool::new(false),
            balance: Mutex::new(None),
            ticking: Default::default(),
            _lock: file,
        })
    }

    /// [`Self::submit`]'s id.
    pub async fn relay(&self, request: &Request) -> Result<B256, Refusal> {
        self.submit(request).await.map(|(id, _)| id)
    }

    /// Check `request` and queue it under its id while sending is not paused: its id
    /// and status. The same request again returns its id and stored status unchecked;
    /// one that failed is queued again under it.
    pub(crate) async fn submit(
        &self,
        request: &Request,
    ) -> Result<(B256, String), Refusal> {
        let (message, ..) = decode(&request.data).ok_or(Refusal::NoncanonicalData)?;
        let (p, c, wallet) = (&self.provider, &self.config, request.wallet);
        let call = outer(c, wallet, request.create, &request.data, message.value);
        let id = request_id(c.chain_id, &call, message.value);
        if let Some(stored) = stored(&self.db(), id)? {
            return Ok(stored);
        }
        if self.paused() {
            return Err(Refusal::Paused);
        }
        let checked = request.check(p, c).await?;
        self.queue(id, wallet, &checked, message.deadline.to())
    }

    /// Store `checked` as `id`, queued in its identity's row, unless [`admit`] now
    /// answers for it, or its worst loss at the head's cap, value included, is over
    /// what the charged losses leave of the global budget.
    #[allow(clippy::result_large_err)] // the `Refusal` that `relay` returns
    fn queue(
        &self,
        id: B256,
        wallet: Address,
        checked: &Checked,
        deadline: u64,
    ) -> Result<(B256, String), Refusal> {
        let tx = serde_json::to_string(&checked.tx).map_err(node)?;
        let (net, l1) = (checked.net, checked.l1_fee_bound);
        let (net, l1) = (net.to_be_bytes::<32>(), l1.to_be_bytes::<32>());
        let ((platform, id_node), db) = (checked.identity, self.db());
        let (_, hold, window) = self.holds();
        if let Some(stored) = admit(&db, id, checked.identity, (hold, window))? {
            return Ok(stored);
        }
        let worst = checked.worst;
        let budget = self.config.global_budget;
        let lost = lost(&db, budget.window, false).map_err(node)?;
        let left = budget.wei.saturating_sub(lost);
        if worst > left {
            return Err(Refusal::OverBudget { worst, left });
        }
        let row = params![
            &platform[..],
            &id_node[..],
            &id[..],
            &wallet[..],
            tx,
            net,
            l1,
            deadline
        ];
        db.execute(
            "INSERT OR REPLACE INTO identities (platform, id_node, id, wallet, tx, net, l1, \
             deadline, status) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'queued')",
            row,
        )
        .map_err(node)?;
        Ok((id, "queued".into()))
    }

    /// [`Self::step`] within `tick_timeout`. Past it the tick is dropped where it
    /// stands: each send is committed before its broadcast, so the next tick resumes it.
    pub async fn tick(&self) -> Result<(), Error> {
        let limit = self.config.tick_timeout;
        let step = tokio::time::timeout(limit, self.step()).await;
        step.map_err(|_| format!("a tick ran over tick_timeout {limit:?}: dropped"))?
    }

    /// Resolve or rebroadcast each request in flight, drop the rows that are done,
    /// then send the oldest queued one when none is in flight and the key's count at the
    /// pinned block is the stored next nonce, the next after a failed one until the tick
    /// has run [`FAILING_TICK`]. Above it a foreign nonce pauses sending;
    /// below it ours is unmined. A balance below `min_balance` pauses sending until a
    /// tick finds it funded, an OP-stack operator fee until a tick finds it off, and a
    /// global loss budget its charged losses spent until its window passes.
    async fn step(&self) -> Result<(), Error> {
        let mut ticking = self.ticking.lock().await;
        let started = tokio::time::Instant::now();
        let head = gas::head(&self.provider, &self.config)
            .await
            .map_err(refused)?;
        let relayer = self.config.relayer;
        let mined = self.provider.get_transaction_count(relayer);
        let mined = mined.block_id(head.at()).await?;
        let mut missing = BTreeMap::new();
        let in_flight = self.in_flight(mined)?;
        ticking
            .since
            .retain(|id, _| in_flight.iter().any(|(i, ..)| i == id));
        for (id, deadline, cover) in in_flight {
            let watched = self
                .watch(&mut ticking.since, (id, deadline, cover), &head, mined)
                .await;
            if let Some(Some(n)) = self.read(&mut ticking.receipt, id, watched, mined)? {
                missing.insert(n, *ticking.missing.get(&n).unwrap_or(&head.number));
            }
        }
        for (id, net) in self.uncharged()? {
            let resolved = self.resolve(id, net, &head).await;
            self.read(&mut ticking.receipt, id, resolved, mined)?;
        }
        self.prune(&head)?;
        let lag = self.config.receipt_lag_blocks;
        let unexplained = missing.values().any(|at| head.number >= at + lag);
        self.unexplained.store(unexplained, Relaxed);
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
        *self.balance.lock().unwrap_or_else(PoisonError::into_inner) = Some(balance);
        self.low.store(balance < floor, Relaxed);
        if self.paused.load(Relaxed)
            || mined < next
            || !self.in_flight(mined)?.is_empty()
            || self.charged_left()?.is_zero()
        {
            return Ok(());
        }
        if self.low.load(Relaxed) {
            let low = format!("balance {balance} below min_balance {floor}");
            return Err(Stop::LowBalance(format!(
                "{low}: sending paused until it is funded"
            ))
            .into());
        }
        if self.queued()?.is_some() || self.operator_fee.load(Relaxed) {
            let fee = gas::operator_fee(&self.provider, &self.config, head.at()).await;
            let fee = fee.map_err(refused)?;
            self.operator_fee
                .store(matches!(fee, OperatorFee::On(..)), Relaxed);
            if let OperatorFee::On(s, c) = fee {
                let on = format!("the operator fee is on (scalar {s}, constant {c})");
                return Err(Stop::OperatorFee(format!(
                    "{on}, which no refund bounds: sending paused until it is off"
                ))
                .into());
            }
        }
        while let Some(queued) = self.queued()? {
            let id = queued.0;
            let failed = self
                .send(&mut ticking, queued, &head, next, balance)
                .await?;
            ticking.send.clear(id); // out of the queue
            if !failed || started.elapsed() >= FAILING_TICK {
                break;
            }
        }
        Ok(())
    }

    /// The rows in flight at the mined count: id, deadline, cover.
    fn in_flight(&self, mined: u64) -> Result<Vec<(B256, u64, Cover)>, Error> {
        let sql =
            format!("SELECT id, deadline, net, l1 FROM identities WHERE {IN_FLIGHT}");
        let row = |r: &Row| Ok((b256(r, 0)?, r.get(1)?, cover(r)?));
        rows(&self.db(), &sql, [mined], row)
    }

    /// The oldest queued request.
    fn queued(&self) -> Result<Option<Queued>, Error> {
        let sql = "SELECT id, tx, net, l1, deadline, wallet FROM identities \
                   WHERE status = 'queued' ORDER BY rowid LIMIT 1";
        let row =
            |r: &Row| Ok((b256(r, 0)?, r.get(1)?, cover(r)?, r.get(4)?, address(r, 5)?));
        Ok(self.db().query_row(sql, [], row).optional()?)
    }

    /// The included requests whose loss is not yet charged: id, `R - value`.
    fn uncharged(&self) -> Result<Vec<(B256, U256)>, Error> {
        let sql =
            "SELECT id, net FROM identities WHERE status = 'included' AND loss IS NULL";
        rows(&self.db(), sql, [], |r| Ok((b256(r, 0)?, u256(r, 1)?)))
    }

    /// Drop the rows that are done: a paid one `receipt_lag_blocks` after its block, a
    /// failed one or one with a loss [`held`] after it was last moved.
    fn prune(&self, head: &Head) -> Result<(), Error> {
        let (lag, hold, window) = self.holds();
        self.db().execute(
            "DELETE FROM identities WHERE \
             (status = 'included' AND loss = zeroblob(32) AND block + ?1 <= ?2) \
             OR ((status = 'failed' OR loss != zeroblob(32)) \
                 AND at <= unixepoch() - iif(holds, ?3, ?4))",
            params![lag, head.number, hold, window],
        )?;
        Ok(())
    }

    /// `receipt_lag_blocks`, and in seconds `loss_hold` and the budget's window.
    fn holds(&self) -> (u64, u64, u64) {
        let c = &self.config;
        let window = c.global_budget.window.as_secs();
        (c.receipt_lag_blocks, c.loss_hold.as_secs(), window)
    }

    /// `id`'s status and its transactions.
    pub async fn status(&self, id: B256) -> Result<Report, Error> {
        let sql = "SELECT status, block, reverted, failure, detail, hashes \
                   FROM identities WHERE id = ?1";
        let row = |r: &Row| {
            let (block, reverted) =
                (r.get::<_, Option<_>>(1)?, r.get::<_, Option<_>>(2)?);
            let failure = (r.get::<_, Option<String>>(3)?, r.get(4)?);
            let hashes = r.get::<_, Option<Vec<u8>>>(5)?.unwrap_or_default();
            Ok((
                first::<String>(r)?,
                block.unwrap_or(0),
                reverted.unwrap_or(false),
                failure,
                hashes.chunks(32).map(B256::from_slice).collect(),
            ))
        };
        let found = self.db().query_row(sql, [&id[..]], row).optional()?;
        let (status, block, reverted, (failure, detail), tx_hashes) =
            found.ok_or(Stop::Unknown)?;
        let status = match &status[..] {
            "queued" => Status::Queued,
            "sent" => Status::Sent,
            "included" => Status::Included { block, reverted },
            "cancelled" => Status::Cancelled,
            _ => Status::Failed(decode_failure(&failure.unwrap_or_default(), detail)),
        };
        Ok(Report { status, tx_hashes })
    }

    /// Whether sending paused: a foreign nonce, a balance below `min_balance`, a first
    /// send refused for a pending transaction of the key, our nonce mined with no
    /// receipt of ours for `receipt_lag_blocks`, an OP-stack operator fee, or a global
    /// loss budget its charged losses spent.
    pub fn paused(&self) -> bool {
        let spent = self.charged_left().map_or(true, |left| left.is_zero());
        let (low, unexplained) = (self.low.load(Relaxed), self.unexplained.load(Relaxed));
        let fee = self.operator_fee.load(Relaxed);
        self.paused.load(Relaxed) || low || unexplained || fee || spent
    }

    /// The configuration it runs with.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The key's balance at the last tick's pinned block.
    pub(crate) fn balance(&self) -> Option<U256> {
        *self.balance.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// How long the oldest queued or sent request has stood so, in seconds.
    pub fn oldest_in_flight(&self) -> Result<Option<u64>, Error> {
        let sql = "SELECT unixepoch() - min(at) FROM identities \
                   WHERE status IN ('queued', 'sent')";
        Ok(self.db().query_row(sql, [], first)?)
    }

    /// What is left of the global loss budget, each send not yet charged at its worst.
    pub fn budget_left(&self) -> Result<U256, Error> {
        self.left(true)
    }

    /// What the charged losses leave of the global loss budget.
    fn charged_left(&self) -> Result<U256, Error> {
        self.left(false)
    }

    fn left(&self, pending: bool) -> Result<U256, Error> {
        let budget = self.config.global_budget;
        let lost = lost(&self.db(), budget.window, pending)?;
        Ok(budget.wei.saturating_sub(lost))
    }

    /// What `receipt` of `id`'s send `send`, or of a cancel, cost the relayer: its gas
    /// and L1 fee, plus its value unless it reverted, less R when the key's balance over
    /// its block rose by at least `R - value - cost`. `None` while the balances are
    /// unread, until `receipt_lag_blocks` past its block, then charged its gas and L1
    /// fee. With whether it holds the identity for `loss_hold`: a reverted batch or a
    /// skipped refund do; a cancel, a paid refund or unread balances do not.
    async fn loss(
        &self,
        cancel: bool,
        send: &[u8],
        receipt: &AnyTransactionReceipt,
        net: U256,
        head: &Head,
    ) -> Result<Option<(U256, bool)>, Error> {
        let gas = U256::from(receipt.gas_used) * U256::from(receipt.effective_gas_price);
        let cost = gas + l1_fee(receipt).map_err(unreadable)?;
        if cancel || !receipt.status() {
            return Ok(Some((cost, !cancel)));
        }
        let value = TxEnvelope::decode_2718(&mut &send[..])
            .map_err(unreadable)?
            .value();
        let block = receipt.block_number.unwrap_or_default();
        Ok(Some(match self.balances(receipt).await {
            Ok((parent, at)) if at + cost >= parent + net => {
                (cost.saturating_sub(net), false)
            }
            Ok(_) => (cost + value, true),
            Err(_) if head.number < block + self.config.receipt_lag_blocks => {
                return Ok(None)
            }
            Err(e) => {
                let hash = receipt.transaction_hash;
                tracing::warn!("{hash} charged as unrefunded: {e}");
                (cost, false)
            }
        }))
    }

    /// The key's balance before and after `receipt`'s block, read by block hash.
    async fn balances(
        &self,
        receipt: &AnyTransactionReceipt,
    ) -> Result<(U256, U256), Error> {
        let hash = receipt.block_hash.ok_or("a receipt with no block hash")?;
        let block = self.provider.get_block_by_hash(hash).await?;
        let parent = block.ok_or("no receipt block")?.header.parent_hash;
        let balance = |h| {
            self.provider
                .get_balance(self.config.relayer)
                .block_id(BlockId::hash(h))
        };
        Ok((balance(parent).await?, balance(hash).await?))
    }

    /// Record `id`'s receipt from any of its transactions with its loss, unless charged
    /// at its block already. False with none.
    async fn resolve(&self, id: B256, net: U256, head: &Head) -> Result<bool, Error> {
        let Some(sent) = self.sent(id)? else {
            return Ok(false);
        };
        for (i, hash) in sent.hashes.iter().enumerate() {
            // Read as any receipt, which keeps an OP-stack `l1Fee`.
            let get = self
                .provider
                .raw_request("eth_getTransactionReceipt".into(), (hash,));
            let get: Result<Option<AnyTransactionReceipt>, _> = get.await;
            let read = |e: alloy::transports::TransportError| match e.is_deser_error() {
                true => unreadable(e),
                false => e.into(),
            };
            let Some(receipt) = get.map_err(read)? else {
                continue;
            };
            let block = receipt.block_hash.unwrap_or_default();
            let sql = "SELECT count(*) FROM identities \
                       WHERE id = ?1 AND block_hash = ?2 AND loss IS NOT NULL";
            let charged =
                self.db()
                    .query_row(sql, params![&id[..], &block[..]], first::<u64>);
            if charged? == 1 {
                return Ok(true);
            }
            let cancel = i > 0;
            let status = if cancel { "cancelled" } else { "included" };
            let loss = self.loss(cancel, &sent.send, &receipt, net, head).await?;
            self.set_status(id, status, Some((&receipt, loss)), None)?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Resolve `id` from a receipt of any of its transactions. With none, broadcast its
    /// send again until its window ends, never past its deadline, or it no longer runs,
    /// then cancel it. Its nonce when mined with no receipt of ours: another sender's,
    /// or a lagging node's.
    async fn watch(
        &self,
        since: &mut BTreeMap<B256, u64>,
        (id, deadline, cover): (B256, u64, Cover),
        head: &Head,
        mined: u64,
    ) -> Result<Option<u64>, Error> {
        // A receipt that does not parse is returned, to be struck, after the watch.
        let unread = match self.resolve(id, cover.0, head).await {
            Ok(true) => return Ok(None),
            Ok(false) => None,
            Err(e) if e.is::<Unreadable>() => Some(e),
            Err(e) => return Err(e),
        };
        let Some(sent) = self.sent(id)? else {
            return Ok(None);
        };
        if unread.is_none() && self.set_status(id, "sent", None, None)? {
            since.insert(id, head.time); // a receipt a reorg removed
        }
        if mined > sent.nonce {
            return unread.map_or(Ok(Some(sent.nonce)), Err);
        }
        if self.resumed.contains(&sent.hashes[0]) {
            since.entry(id).or_insert(head.time);
        }
        let from = since.get(&id).copied().unwrap_or(sent.signed_at);
        let late = self.late(from).min(deadline);
        let last = sent.last()?;
        let run = Some((deadline, late));
        if last.cancel
            || head.time >= late
            || !self.broadcast(&last, cover, run, head).await?
        {
            self.cancel(id, &last, cover.1, head).await?;
        }
        unread.map_or(Ok(None), Err)
    }

    /// Sign the queued transaction at `nonce` with its fee cap at most the pinned
    /// block's fee bound and the tip read now, check its target and run it again at the
    /// pinned block, then commit and broadcast it. True when it failed instead, and the
    /// next may be sent: its refund no longer covering the floor at the L1 bound read
    /// now, its target or run failing for good, or the node refusing it for good; a node
    /// error on them or a refusal for now does so after `node_error_ticks` ticks in a
    /// row.
    async fn send(
        &self,
        ticking: &mut Ticking,
        (id, tx, (net, l1), deadline, wallet): Queued,
        head: &Head,
        nonce: u64,
        balance: U256,
    ) -> Result<bool, Error> {
        let tx: TransactionRequest = serde_json::from_str(&tx)?;
        let (limit, value) = (tx.gas.unwrap_or_default(), tx.value.unwrap_or_default());
        let (refund, create) = (net + value, tx.to == Some(self.config.factory.into()));
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
        let attempt = self.sign(tx, false).await?;
        let worst = affordable(&attempt.raw, (net, now))? + value;
        if balance < worst {
            let required = worst;
            return self.fail(id, &Refusal::RelayerUnderfunded { balance, required });
        }
        let left = self.budget_left()?;
        if worst > left {
            return self.fail(id, &Refusal::OverBudget { worst, left });
        }
        let vouched = vouched(&self.provider, &self.config, wallet, create);
        let run = self.runs(&attempt.raw, deadline, head, self.late(head.time));
        if let Err(refusal) = async { vouched.await.and(run.await) }.await {
            return self.strike(&mut ticking.send, id, refusal);
        }
        self.commit(id, &attempt, worst, head.time)?;
        match self.broadcast(&attempt, (net, l1), None, head).await {
            Err(e) => match e.downcast::<Refused>() {
                Ok(refused) => self.strike(&mut ticking.send, id, refused.0),
                Err(e) => Err(e),
            },
            Ok(_) => Ok(false),
        }
    }

    /// `refusal` of `id`'s send: a node error that is not deterministic holds the queue
    /// as the tick's error for `node_error_ticks` ticks in a row; any other fails it.
    fn strike(
        &self,
        strikes: &mut Strikes,
        id: B256,
        refusal: Refusal,
    ) -> Result<bool, Error> {
        let n = self.config.node_error_ticks;
        match refusal {
            Refusal::Node(e) if !deterministic(&e) && strikes.strike(id) < n => {
                Err(e.into())
            }
            refusal => self.fail(id, &refusal),
        }
    }

    /// `result`, of reading `id`'s receipt: a success clears its strikes; a receipt that
    /// does not parse is struck, and charged at its worst after `node_error_ticks` once
    /// the mined count is past its nonce.
    fn read<T>(
        &self,
        strikes: &mut Strikes,
        id: B256,
        result: Result<T, Error>,
        mined: u64,
    ) -> Result<Option<T>, Error> {
        let e = match result {
            Ok(t) => {
                strikes.clear(id);
                return Ok(Some(t));
            }
            Err(e) if e.is::<Unreadable>() => e,
            Err(e) => return Err(e),
        };
        let sql = "SELECT nonce FROM identities WHERE id = ?1";
        let nonce = self.db().query_row(sql, [&id[..]], first::<Option<u64>>);
        let landed = nonce
            .optional()?
            .flatten()
            .is_some_and(|nonce| mined > nonce);
        if strikes.strike(id) < self.config.node_error_ticks || !landed {
            return Err(e);
        }
        tracing::warn!("{id} charged at its worst: its receipt: {e}");
        strikes.clear(id);
        self.charge_worst(id, &e).map(|()| None)
    }

    /// Charge `id` with no refund at its stored worst less its value, which a landed
    /// send to a vouched wallet cannot lose; with no receipt recorded, it fails as
    /// `Node`.
    fn charge_worst(&self, id: B256, e: &Error) -> Result<(), Error> {
        let unread = Refusal::Node(format!("its receipt does not parse: {e}"));
        let (failure, detail) = encode_failure(&unread);
        let db = self.db();
        let sql = "SELECT worst, tx FROM identities WHERE id = ?1";
        let row = |r: &Row| Ok((u256(r, 0)?, r.get::<_, String>(1)?));
        let (worst, tx) = db.query_row(sql, [&id[..]], row)?;
        let tx: TransactionRequest = serde_json::from_str(&tx)?;
        let loss = worst.saturating_sub(tx.value.unwrap_or_default());
        db.execute(
            "UPDATE identities SET loss = ?4, holds = 1, at = unixepoch(), \
             status = iif(block IS NULL, 'failed', status), \
             failure = iif(block IS NULL, ?2, failure), \
             detail = iif(block IS NULL, ?3, detail) WHERE id = ?1",
            params![&id[..], failure, detail, loss.to_be_bytes::<32>()],
        )?;
        Ok(())
    }

    /// Mark `id` failed for `refusal`; true, as the next may be sent.
    fn fail(&self, id: B256, refusal: &Refusal) -> Result<bool, Error> {
        self.set_status(id, "failed", None, Some(refusal))?;
        Ok(true)
    }

    /// A 0-value self-send at `last`'s nonce, its worst at the L1 bound `l1`. The last
    /// cancel is sent again while the pinned base fee is at most its cap; past it, a new
    /// one is signed with its cap twice the base fee up to the max cancel fee, and its
    /// tip the market's; both at least 1.1× `last`'s.
    async fn cancel(
        &self,
        id: B256,
        last: &Attempt,
        l1: U256,
        head: &Head,
    ) -> Result<(), Error> {
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
        let attempt = self.sign(cancel, true).await?;
        let worst = most(&TxEnvelope::decode_2718(&mut &attempt.raw[..])?, l1);
        self.commit(id, &attempt, worst, head.time)?;
        self.broadcast(&attempt, Default::default(), None, head)
            .await
            .map(drop)
    }

    /// The end of the inclusion window of a send signed at `at`.
    fn late(&self, at: u64) -> u64 {
        at + self.config.inclusion_window.as_secs()
    }

    /// `tx` signed, a cancel when `cancel`.
    async fn sign(&self, tx: TransactionRequest, cancel: bool) -> Result<Attempt, Error> {
        let signed = self.provider.fill_and_sign_transaction(tx).await?;
        Attempt::read(signed.encoded_2718().into(), cancel)
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
    /// end, false when a send no longer runs at `head`, to cancel at once. A first send
    /// the node's pool refuses, naming why ([`pool_refused`]), is uncommitted and queued
    /// again: pausing sending when a pending transaction of the key holds its nonce, and
    /// otherwise [`Refused`]. On any other error it stays committed, to be watched.
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
        // The node may hold `a` unless its pool named a refusal and it has no such hash.
        let first = run.is_none() && !a.cancel;
        let message = e.as_error_resp().map(|r| r.message.to_string());
        let message = message.filter(|m| first && pool_refused(m));
        let rejected = match &message {
            Some(_) => self
                .provider
                .get_transaction_by_hash(a.hash)
                .await?
                .is_none(),
            None => false,
        };
        let Some(message) = message.filter(|_| rejected) else {
            tracing::warn!("broadcast of {} refused, retried next tick: {e}", a.hash);
            return Ok(true);
        };
        self.uncommit(a)?;
        if words(&message).iter().any(|w| w == "replacement") {
            self.paused.store(true, Relaxed);
            let held = format!("the node refused send {}: {e}", a.hash);
            let held = format!("{held}; a pending transaction of the key holds it");
            return Err(format!("{held}: sending paused").into());
        }
        tracing::warn!("the node refused send {}: {e}", a.hash);
        Err(Box::new(Refused(refusal(e))))
    }

    /// Undo [`Self::commit`] of a first send: drop it, free its nonce, queue its row
    /// again.
    fn uncommit(&self, a: &Attempt) -> Result<(), Error> {
        let mut db = self.db();
        let tx = db.transaction()?;
        tx.execute(
            "UPDATE identities SET status = 'queued', nonce = NULL, send = NULL, \
             hashes = NULL, signed_at = NULL, worst = NULL WHERE hashes = ?1",
            [&a.hash[..]],
        )?;
        tx.execute("UPDATE nonce SET next = ?1 WHERE next = ?1 + 1", [a.nonce])?;
        Ok(tx.commit()?)
    }

    /// Store `a`, signed at a block of time `at`, with the row's worst raised to
    /// `worst`, and the next nonce past it, in one transaction; a send marks `id` sent.
    fn commit(&self, id: B256, a: &Attempt, worst: U256, at: u64) -> Result<(), Error> {
        let mut db = self.db();
        let tx = db.transaction()?;
        let (raw, worst) = (&a.raw[..], worst.to_be_bytes::<32>());
        if a.cancel {
            let sql = "SELECT hashes FROM identities WHERE id = ?1";
            let mut hashes: Vec<u8> = tx.query_row(sql, [&id[..]], first)?;
            hashes.extend_from_slice(&a.hash[..]);
            tx.execute(
                "UPDATE identities SET cancel = ?2, hashes = ?3, worst = max(worst, ?4) \
                 WHERE id = ?1",
                params![&id[..], raw, hashes, worst],
            )?;
        } else {
            tx.execute(
                "UPDATE identities SET status = 'sent', at = unixepoch(), nonce = ?2, \
                 send = ?3, cancel = NULL, hashes = ?4, signed_at = ?5, worst = ?6 \
                 WHERE id = ?1",
                params![&id[..], a.nonce, raw, &a.hash[..], at, worst],
            )?;
        }
        tx.execute("UPDATE nonce SET next = max(next, ?1)", [a.nonce + 1])?;
        Ok(tx.commit()?)
    }

    /// Move `id` to `status`, with a receipt and its loss, or a failure; the row's time
    /// when the status changes, and whether it did.
    fn set_status(
        &self,
        id: B256,
        status: &str,
        receipt: Option<(&AnyTransactionReceipt, Option<(U256, bool)>)>,
        failed: Option<&Refusal>,
    ) -> Result<bool, Error> {
        let (receipt, loss) = receipt.unzip();
        let block = receipt.map(|r| r.block_number.unwrap_or_default());
        let hash = receipt.and_then(|r| r.block_hash.map(|h| h.0));
        let (failure, detail) = failed.map(encode_failure).unzip();
        let reverted = receipt.map(|r| !r.status());
        let (loss, holds) = loss.flatten().unzip();
        let loss = loss.map(|w| w.to_be_bytes::<32>());
        let db = self.db();
        let was: String = db.query_row(
            "SELECT status FROM identities WHERE id = ?1",
            [&id[..]],
            first,
        )?;
        db.execute(
            "UPDATE identities SET at = iif(status = ?2, at, unixepoch()), status = ?2, \
             block = ?3, reverted = ?4, failure = ?5, detail = ?6, loss = ?7, \
             block_hash = ?8, holds = ?9 WHERE id = ?1",
            params![
                &id[..],
                status,
                block,
                reverted,
                failure,
                detail,
                loss,
                hash,
                holds
            ],
        )?;
        Ok(was != status)
    }

    /// `id`'s signed transactions, once sent.
    fn sent(&self, id: B256) -> Result<Option<Sent>, Error> {
        let sql = "SELECT nonce, hashes, send, cancel, signed_at FROM identities \
                   WHERE id = ?1 AND send IS NOT NULL";
        let row = |r: &Row| {
            let hashes = r.get::<_, Vec<u8>>(1)?;
            Ok(Sent {
                nonce: r.get(0)?,
                hashes: hashes.chunks(32).map(B256::from_slice).collect(),
                send: r.get::<_, Vec<u8>>(2)?.into(),
                cancel: r.get::<_, Option<Vec<u8>>>(3)?.map(Into::into),
                signed_at: r.get(4)?,
            })
        };
        Ok(self.db().query_row(sql, [&id[..]], row).optional()?)
    }

    fn db(&self) -> MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// What the rows lost over the last `window`: each charged loss by when it was
/// charged, and with `pending` each send not yet charged at its worst.
pub(crate) fn lost(db: &Connection, window: Duration, pending: bool) -> Result<U256, Error> {
    let sql = "SELECT coalesce(loss, worst) FROM identities WHERE worst IS NOT NULL \
               AND iif(loss IS NULL, ?2, at > unixepoch() - ?1)";
    let losses = rows(db, sql, params![window.as_secs(), pending], |r| u256(r, 0))?;
    Ok(losses.into_iter().fold(U256::ZERO, U256::saturating_add))
}

/// The rows by status, an included one whose batch reverted as `reverted`; the
/// identities a loss holds for `hold`; and the included rows whose loss is not yet
/// charged.
pub(crate) fn counts(
    db: &Connection,
    (hold, window): (Duration, Duration),
) -> Result<(BTreeMap<String, u64>, u64, u64), Error> {
    let sql = "SELECT CASE WHEN reverted THEN 'reverted' ELSE status END, count(*) \
               FROM identities GROUP BY 1";
    let statuses = rows(db, sql, [], |r| Ok((r.get(0)?, r.get(1)?)))?;
    let sql = "SELECT count(*) FROM identities \
               WHERE loss != zeroblob(32) AND at > unixepoch() - iif(holds, ?1, ?2)";
    let held = db.query_row(sql, [hold.as_secs(), window.as_secs()], first)?;
    let sql =
        "SELECT count(*) FROM identities WHERE status = 'included' AND loss IS NULL";
    let pending = db.query_row(sql, [], first)?;
    Ok((statuses.into_iter().collect(), held, pending))
}

/// A request's id: its chain, target, calldata and value.
fn request_id(chain_id: u64, call: &TransactionRequest, value: U256) -> B256 {
    let to = TransactionBuilder::to(call).unwrap_or_default();
    let input = TransactionBuilder::input(call).cloned().unwrap_or_default();
    keccak256((U256::from(chain_id), to, input, value).abi_encode_params())
}

/// `mined` lies between the lowest nonce in flight and the next nonce, and each nonce
/// in flight below it has a receipt for one of its transactions.
async fn explained<P: Provider>(
    provider: &P,
    db: &Connection,
    mined: u64,
    next: u64,
) -> Result<(), Error> {
    let sql = format!("SELECT nonce, hashes FROM identities WHERE {IN_FLIGHT}");
    let in_flight = rows(db, &sql, [mined], |r| {
        Ok((r.get::<_, u64>(0)?, r.get::<_, Vec<u8>>(1)?))
    })?;
    let below = in_flight.iter().filter(|a| a.0 < mined);
    let mut unexplained: BTreeSet<_> = below.clone().map(|a| a.0).collect();
    for (nonce, hashes) in below {
        for hash in hashes.chunks(32).map(B256::from_slice) {
            if provider.get_transaction_receipt(hash).await?.is_some() {
                unexplained.remove(nonce);
            }
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

/// `id` and its status when it is stored and has not failed.
#[allow(clippy::result_large_err)] // the `Refusal` that `relay` returns
fn stored(db: &Connection, id: B256) -> Result<Option<(B256, String)>, Refusal> {
    let status = "SELECT status FROM identities WHERE id = ?1";
    let status = db.query_row(status, [&id[..]], first::<String>).optional();
    let status = status.map_err(node)?.filter(|s| s != "failed");
    Ok(status.map(|status| (id, status)))
}

/// [`stored`], or room for `id` in `identity`'s row: none, a failed one, or one whose
/// loss is [`held`] no longer. `IdentityBusy` while another request of the identity is
/// queued, sent, or paid and not yet dropped; `IdentityHeld` while a loss holds it.
#[allow(clippy::result_large_err)] // the `Refusal` that `relay` returns
fn admit(
    db: &Connection,
    id: B256,
    (platform, id_node): Identity,
    (hold, window): (u64, u64),
) -> Result<Option<(B256, String)>, Refusal> {
    if let Some(stored) = stored(db, id)? {
        return Ok(Some(stored));
    }
    let sql =
        "SELECT id, status, loss, at, unixepoch(), coalesce(holds, 1) FROM identities \
               WHERE platform = ?1 AND id_node = ?2";
    let row = |r: &Row| {
        let loss = r.get::<_, Option<Vec<u8>>>(2)?;
        Ok((
            b256(r, 0)?,
            r.get::<_, String>(1)?,
            loss.map(|w| U256::from_be_slice(&w)),
            r.get::<_, u64>(3)?,
            r.get::<_, u64>(4)?,
            r.get::<_, bool>(5)?,
        ))
    };
    let found = db.query_row(sql, [&platform[..], &id_node[..]], row);
    let until = |at: u64, holds: bool| at + held(holds, hold, window);
    match found.optional().map_err(node)? {
        None => Ok(None),
        Some((_, status, None, ..)) if status == "failed" => Ok(None),
        Some((id, _, None, ..)) => Err(Refusal::IdentityBusy { id }),
        Some((id, _, Some(loss), ..)) if loss.is_zero() => {
            Err(Refusal::IdentityBusy { id })
        }
        Some((.., at, now, holds)) if now < until(at, holds) => {
            Err(Refusal::IdentityHeld {
                until: until(at, holds),
            })
        }
        Some(_) => Ok(None),
    }
}

/// How long a loss holds its identity and stays in the budget: `hold` for a reverted
/// batch or a skipped refund; the budget's `window` for a cancel, a paid refund short
/// of the send's cost, or unread balances.
fn held(holds: bool, hold: u64, window: u64) -> u64 {
    if holds {
        hold
    } else {
        window
    }
}

/// The send `raw` costs at most `R - value`: its gas at its cap plus the L1 bound.
/// Returns its [`most`].
fn affordable(raw: &[u8], (net, l1): Cover) -> Result<U256, Error> {
    let worst = most(&TxEnvelope::decode_2718(&mut &raw[..])?, l1);
    if worst > net {
        return Err(format!("a send costs up to {worst}, above R - value {net}").into());
    }
    Ok(worst)
}

/// The most `tx` costs at the L1 bound `l1`: its gas at its cap and `l1`.
fn most(tx: &TxEnvelope, l1: U256) -> U256 {
    U256::from(tx.gas_limit()) * U256::from(tx.max_fee_per_gas()) + l1
}

/// Words of a pool's refusal that the same send meets again: fees, gas, funds, or a
/// transaction over the node's size.
const FOR_GOOD: [&str; 4] = ["fee", "gas", "funds", "oversized"];

/// Words of a pool's refusal that may pass: a replacement or an underpriced send.
const FOR_NOW: [&str; 2] = ["replacement", "underpriced"];

/// The lowercase words of `message`, a camelCase one split at each capital.
fn words(message: &str) -> Vec<String> {
    let mut spaced = String::with_capacity(message.len());
    let mut lower = false;
    for c in message.chars() {
        if lower && c.is_ascii_uppercase() {
            spaced.push(' ');
        }
        lower = c.is_ascii_lowercase();
        spaced.push(c);
    }
    let words = spaced.split(|c: char| !c.is_ascii_alphanumeric());
    words
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// A node's answer that names a refusal of its pool, in whole words: [`FOR_GOOD`],
/// [`FOR_NOW`], or a pool that "is full". Not "nonce too low", which a gateway that
/// sends again answers for a send it landed, nor a rate limit.
fn pool_refused(message: &str) -> bool {
    let words = words(message);
    let named = |w: &String| FOR_GOOD.contains(&&w[..]) || FOR_NOW.contains(&&w[..]);
    words.iter().any(named) || words.windows(2).any(|w| w == ["is", "full"])
}

/// A node error that the same run or send meets again: an error response naming
/// [`FOR_GOOD`] in whole words.
fn deterministic(e: &str) -> bool {
    let Some(message) = answered(e) else {
        return false;
    };
    words(message).iter().any(|w| FOR_GOOD.contains(&&w[..]))
}

/// A failure as stored: its name and its fields, a node error's text redacted.
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
        Refusal::OverBudget { worst, left } => ("OverBudget".into(), words(worst, left)),
        Refusal::UnsupportedRelease { version } => {
            let version = U256::from(*version).to_be_bytes::<32>();
            ("UnsupportedRelease".into(), version.to_vec())
        }
        Refusal::Node(e) => ("Node".into(), logs::redact(e).into_bytes()),
        other => (other.name().into(), vec![]),
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
        "OverBudget" => Refusal::OverBudget {
            worst: first,
            left: second,
        },
        "UnsupportedRelease" => Refusal::UnsupportedRelease {
            version: first.saturating_to(),
        },
        "UnknownWallet" => Refusal::UnknownWallet,
        "Node" => Refusal::Node(String::from_utf8_lossy(&detail).into()),
        _ => Refusal::Expired,
    }
}

/// A receipt's `l1Fee`; 0 on a chain without one.
fn l1_fee(receipt: &AnyTransactionReceipt) -> Result<U256, Error> {
    let l1 = receipt.other.get_deserialized::<U256>("l1Fee");
    Ok(l1.transpose()?.unwrap_or_default())
}

pub(crate) fn rows<T>(
    db: &Connection,
    sql: &str,
    params: impl Params,
    row: impl FnMut(&Row) -> rusqlite::Result<T>,
) -> Result<Vec<T>, Error> {
    let mut statement = db.prepare(sql)?;
    let rows = statement.query_map(params, row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub(crate) fn first<T: rusqlite::types::FromSql>(row: &Row) -> rusqlite::Result<T> {
    row.get(0)
}

fn address(row: &Row, i: usize) -> rusqlite::Result<Address> {
    Ok(Address::from_slice(&row.get::<_, Vec<u8>>(i)?))
}

fn b256(row: &Row, i: usize) -> rusqlite::Result<B256> {
    Ok(B256::from_slice(&row.get::<_, Vec<u8>>(i)?))
}

pub(crate) fn u256(row: &Row, i: usize) -> rusqlite::Result<U256> {
    Ok(U256::from_be_slice(&row.get::<_, Vec<u8>>(i)?))
}

/// The `net` and `l1` columns at 2 and 3.
fn cover(row: &Row) -> rusqlite::Result<Cover> {
    Ok((u256(row, 2)?, u256(row, 3)?))
}

fn refused(refusal: Refusal) -> Error {
    format!("{refusal:?}").into()
}

/// A first send the node answered with an error and holds no trace of, uncommitted.
#[derive(Debug)]
struct Refused(Refusal);

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

impl std::error::Error for Refused {}

/// A receipt or attempt that does not parse, struck as a node error on a run is.
#[derive(Debug)]
struct Unreadable(String);

impl fmt::Display for Unreadable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unreadable {}

fn unreadable(e: impl fmt::Display) -> Error {
    Box::new(Unreadable(e.to_string()))
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
        assert_eq!(affordable(&raw, (net, l1)).unwrap(), U256::from(703));
        assert!(affordable(&raw, (net - U256::ONE, l1)).is_err());
    }

    #[test]
    fn only_a_node_answer_naming_fees_gas_or_funds_is_deterministic() {
        use alloy::{
            rpc::json_rpc::ErrorPayload,
            transports::{
                RpcError,
                TransportErrorKind,
            },
        };
        let answer = |message: &str| {
            RpcError::ErrorResp(ErrorPayload {
                code: -32000,
                message: message.to_owned().into(),
                data: None,
            })
        };
        let url = "error sending request for url (https://gas.example/funds?fee=1)";
        let cases = [
            (answer("insufficient funds for gas * price + value"), true),
            (answer("header not found"), false),
            (answer("oversized data"), true),
            (answer("FeeTooLow"), true),
            (TransportErrorKind::custom_str(url), false),
        ];
        for (e, expected) in cases {
            let Refusal::Node(text) = crate::check::refusal(e) else {
                panic!()
            };
            assert_eq!(deterministic(&text), expected, "{text}");
        }
    }

    #[test]
    fn only_a_pool_refusal_in_whole_words_uncommits_a_send() {
        let cases = [
            ("replacement transaction underpriced", true),
            ("txpool is full", true),
            ("insufficient funds for gas * price + value", true),
            ("oversized data", true),
            ("InsufficientFunds", true),
            ("FeeTooLow", true),
            ("ReplacementTransactionUnderpriced", true),
            ("nonce too low", false),
            ("NonceTooLow", false),
            ("already known", false),
            ("rate limit exceeded", false),
            ("bad gateway (ray id 8feeb2c1)", false),
        ];
        for (message, refused) in cases {
            assert_eq!(pool_refused(message), refused, "{message}");
        }
    }

    #[test]
    fn camel_case_splits_into_words() {
        let cases = [
            ("InsufficientFunds", "insufficient funds"),
            ("FeeTooLow", "fee too low"),
            ("maxFeePerGas too low", "max fee per gas too low"),
            ("txpool is full", "txpool is full"),
        ];
        for (message, expected) in cases {
            assert_eq!(words(message).join(" "), expected, "{message}");
        }
    }

    #[test]
    fn a_node_failure_is_stored_without_the_rpc_url() {
        let url = "https://user:pw0rd@node.example/v2/s3cr3tk3y?key=qu3ry";
        crate::logs::install(Some(url), None);
        let text = format!("error sending request for url ({url})");
        let (_, detail) = encode_failure(&Refusal::Node(text));
        let detail = String::from_utf8(detail).unwrap();
        for secret in ["pw0rd", "s3cr3tk3y", "qu3ry"] {
            assert!(!detail.contains(secret), "{detail}");
        }
    }

    #[test]
    fn a_target_failure_reads_back_as_stored() {
        let release = Refusal::UnsupportedRelease { version: 7 };
        for refusal in [release, Refusal::UnknownWallet] {
            let (name, detail) = encode_failure(&refusal);
            let read = decode_failure(&name, Some(detail));
            assert_eq!(format!("{read:?}"), format!("{refusal:?}"));
        }
    }
}
