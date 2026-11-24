//! The relayer on anvil: a real SQLite file and `flock` under `CARGO_TARGET_TMPDIR`; a
//! restart drops the instance and starts another on the same file. Needs `anvil` on
//! `PATH`.

#![allow(dead_code, unused_imports, clippy::unneeded_struct_pattern)]

include!("common/onboarding.rs");

type Rel = Relayer<DynProvider>;

impl Onboarding {
    /// The onboarding relayed, signed to expire `secs` after the latest block.
    async fn onboard(&self, r: &Rel, secs: u64) -> Result<B256, Refusal> {
        let (now, calls) = (self.now().await, encode_executions(&self.onboarding()));
        let data = self.sign_until(calls, self.value, now, now + secs);
        r.relay(&self.request(data)).await
    }

    /// The onboarding relayed by `r`, signed to expire an hour after the latest block.
    async fn queue(&self, r: &Relayer<impl Provider>) -> B256 {
        let request = self.request_for(&self.onboarding()).await;
        r.relay(&request).await.unwrap()
    }

    async fn receipt(&self, hash: B256) -> TransactionReceipt {
        let receipt = self.owner.get_transaction_receipt(hash).await;
        receipt.unwrap().unwrap()
    }

    /// A relayer whose provider is armed with `mode`.
    async fn racing(&self, mode: u8, name: &str) -> Relayer<Racing> {
        let racing = Racing(self.relayer.clone(), std::sync::Arc::new(mode.into()));
        let path = db(name);
        Relayer::start(racing, self.config(), &path).await.unwrap()
    }

    async fn mined(&self, hash: B256) -> bool {
        let receipt = self.owner.get_transaction_receipt(hash).await;
        receipt.unwrap().is_some()
    }

    async fn base_fee(&self, fee: u128) {
        let next = self.owner.anvil_set_next_block_base_fee_per_gas(fee);
        next.await.unwrap();
    }

    /// The refund covers each send attempt: `gasLimit × maxFeePerGas + l1 ≤ R − value`.
    /// A cancel, a self-send, is outside it.
    async fn assert_within_refund(&self, hashes: &[B256], l1: U256) {
        for hash in hashes {
            let t = self.tx(*hash).await.inner;
            let fee = U256::from(t.gas_limit()) * U256::from(t.max_fee_per_gas());
            let send = t.to() != Some(self.relayer_address);
            assert!(
                !send || fee + l1 <= self.refund() - self.value,
                "above R - value: {fee}"
            );
        }
    }

    /// A transaction on the relayer's key that the relayer did not send.
    async fn foreign(&self) {
        let to_self = TransactionRequest::default().with_to(self.relayer_address);
        let sent = self.relayer.send_transaction(to_self).await.unwrap();
        assert!(sent.get_receipt().await.unwrap().status());
    }

    /// Sent with automine off at a fresh block, then priced out by the base fee: still
    /// pending as its window ends, and not cancelled before.
    /// Its cap is `m` percent of the pinned base fee, at least the window's growth.
    async fn priced_out(&self, r: &Rel, m: u128) -> (B256, Request, Transaction, Bytes) {
        self.owner.anvil_set_auto_mine(false).await.unwrap();
        let request = self.request_for(&self.onboarding()).await;
        let id = r.relay(&request).await.unwrap();
        let pinned = self.owner.get_block_by_number(Default::default()).await;
        let pinned = pinned.unwrap().unwrap().header;
        let late = pinned.timestamp + self.config().inclusion_window.as_secs();
        r.tick().await.unwrap();
        let report = r.status(id).await.unwrap();
        assert!(matches!(report.status, Status::Sent), "{:?}", report.status);
        let hash = report.tx_hashes[0];
        let raw = self.owner.get_raw_transaction_by_hash(hash).await.unwrap();
        let tx = self.tx(hash).await;
        self.assert_within_refund(&[hash], U256::ZERO).await;
        let base = u128::from(pinned.base_fee_per_gas.unwrap());
        let bound = (base * m / 100).max(relayer::config::Eip1559::L1.grow(base, 5));
        assert_eq!(tx.inner.max_fee_per_gas(), bound, "not M × base");
        self.base_fee(tx.inner.max_fee_per_gas() * 100).await;
        for at in [late - 1, late] {
            let early = r.status(id).await.unwrap().tx_hashes;
            assert_eq!(early, [hash], "cancelled early");
            self.owner.anvil_set_next_block_timestamp(at).await.unwrap();
            self.owner.evm_mine(None).await.unwrap();
            r.tick().await.unwrap();
        }
        assert!(!self.mined(hash).await, "mined above its fee cap");
        (id, request, tx, raw.unwrap())
    }
}

/// Fails `eth_sendRawTransaction` once when armed: `RACE` mines a transaction of the
/// key first, `ECHO` sends the transaction, then answers with an error. `STALL` and
/// `BROKE` make every `eth_call` a node error: a wrong chain id, and no funds.
#[derive(Clone)]
struct Racing(DynProvider, std::sync::Arc<std::sync::atomic::AtomicU8>);

const RACE: u8 = 1;
const ECHO: u8 = 2;
const STALL: u8 = 3;
const BROKE: u8 = 4;

type Boxed<'a, T> =
    alloy::transports::BoxFuture<'a, alloy::transports::TransportResult<T>>;

impl Provider for Racing {
    fn root(&self) -> &alloy::providers::RootProvider {
        self.0.root()
    }

    fn fill_and_sign_transaction<'a, 'b>(
        &'a self,
        tx: TransactionRequest,
    ) -> Boxed<'b, alloy::consensus::TxEnvelope>
    where
        'a: 'b,
    {
        Box::pin(self.0.fill_and_sign_transaction(tx))
    }

    fn call(
        &self,
        tx: TransactionRequest,
    ) -> alloy::providers::EthCall<alloy::network::Ethereum, Bytes> {
        self.0.call(match self.1.load(Ordering::Relaxed) {
            STALL => tx.with_chain_id(1),
            BROKE => tx.from(Address::repeat_byte(2)),
            _ => tx,
        })
    }

    fn send_raw_transaction<'a, 'b, 'c>(
        &'a self,
        raw: &'b [u8],
    ) -> Boxed<'c, alloy::providers::PendingTransactionBuilder<alloy::network::Ethereum>>
    where
        'a: 'c,
        'b: 'c,
    {
        Box::pin(async move {
            let mode = self.1.swap(0, Ordering::Relaxed);
            if mode == RACE {
                let any = TransactionRequest::default().with_to(Address::repeat_byte(1));
                let sent = self.0.send_transaction(any).await?;
                sent.get_receipt().await.unwrap();
            }
            let sent = self.0.send_raw_transaction(raw).await?;
            if mode != ECHO {
                return Ok(sent);
            }
            let echoed = r#"{"code": -32000, "message": "echoed"}"#;
            let echoed = serde_json::from_str(echoed).unwrap();
            Err(alloy::transports::RpcError::ErrorResp(echoed))
        })
    }
}

/// One id and one transaction for a request delivered twice, across a restart
/// too; another batch for the busy wallet is refused; a foreign nonce refuses start.
#[tokio::test]
async fn the_same_request_twice_is_one_id_and_one_transaction() {
    let o = Onboarding::new(true).await;
    let (db, request) = (db("same"), o.request_for(&o.onboarding()).await);
    let r = o.start(&db).await.unwrap();
    let id = r.relay(&request).await.unwrap();
    assert_eq!(r.relay(&request).await.unwrap(), id, "a second id");
    let busy = matches!(o.onboard(&r, 1800).await, Err(Refusal::WalletBusy { .. }));
    assert!(busy, "another batch for a busy wallet");
    let report = o.settle(&r, id, included).await;
    o.assert_within_refund(&report.tx_hashes, U256::ZERO).await;
    drop(r);
    let r = o.start(&db).await.unwrap(); // latest = stored next, with our receipt
    let again = r.relay(&request).await.unwrap();
    assert_eq!(again, id, "a new id after restart");
    r.tick().await.unwrap();
    assert_eq!(r.status(id).await.unwrap().tx_hashes, report.tx_hashes);
    assert_eq!(o.relayer_nonce().await, 1, "sent twice");
    drop(r);
    o.foreign().await;
    let refused = o.start(&db).await.err().unwrap();
    assert!(refused.contains("inspect the key's nonce"), "{refused}");
}

/// An instance holds `flock` on `<db>.lock`; any other holder refuses start. A
/// database of another schema version is refused, naming the fix, and so are settings
/// under the least the sender keeps.
#[tokio::test]
async fn a_second_instance_or_an_older_schema_is_refused() {
    let (o, db) = (Onboarding::new(false).await, db("lock"));
    let lock = format!("{}.lock", db.display());
    let r = o.start(&db).await.unwrap();
    let refused = o.start(&db).await.err().unwrap();
    assert!(refused.contains(&lock), "{refused}");
    let held = std::fs::File::open(&lock).unwrap();
    assert!(held.try_lock().is_err(), "no flock on {lock}");
    drop(r);
    held.try_lock().unwrap();
    assert!(o.start(&db).await.err().unwrap().contains(&lock));
    let old = crate::db("schema");
    let conn = rusqlite::Connection::open(&old).unwrap();
    conn.execute_batch("CREATE TABLE nonce (next)").unwrap();
    let refused = o.start(&old).await.err().unwrap();
    assert!(refused.contains("fresh database file"), "{refused}");
    let short = Config {
        inclusion_window: o.config().block_time,
        ..o.config()
    };
    let cheap = Config {
        max_cancel_fee_percent: o.config().max_fee_percent,
        ..o.config()
    };
    let none = Config {
        supported_releases: vec![],
        ..o.config()
    };
    for (config, setting) in [
        (short, "inclusion_window"),
        (cheap, "max_cancel_fee_percent"),
        (none, "supported_releases"),
    ] {
        let refused = Relayer::start(o.relayer.clone(), config, &crate::db("bad")).await;
        let refused = refused.err().unwrap().to_string();
        assert!(refused.contains(setting), "{refused}");
    }
    let least = Config {
        inclusion_window: 2 * o.config().block_time,
        max_fee_percent: 100,
        max_cancel_fee_percent: 110,
        ..o.config()
    };
    Relayer::start(o.relayer.clone(), least, &crate::db("least"))
        .await
        .unwrap();
}

/// A send left pending at a restart holds the queue: its stored raw transaction is
/// sent again, once, and another wallet's batch waits until it lands.
#[tokio::test]
async fn a_send_pending_at_start_lands_once_before_the_next() {
    let (mut o, db) = (Onboarding::new(true).await, db("crash"));
    o.owner.anvil_set_auto_mine(false).await.unwrap();
    let r = o.start(&db).await.unwrap();
    let id = o.onboard(&r, 3600).await.unwrap();
    r.tick().await.unwrap();
    let hashes = r.status(id).await.unwrap().tx_hashes;
    o.owner.anvil_drop_transaction(hashes[0]).await.unwrap();
    drop(r);
    let r = o.start(&db).await.unwrap();
    o.wallet_at(U256::ONE).await;
    let next = o.queue(&r).await;
    r.tick().await.unwrap();
    let pending = o.owner.get_transaction_by_hash(hashes[0]).await.unwrap();
    assert!(pending.is_some(), "not sent again");
    let held = r.status(next).await.unwrap().status;
    assert!(matches!(held, Status::Queued), "served first: {held:?}");
    let report = o.settle(&r, id, included).await;
    assert_eq!(report.tx_hashes, hashes, "not the stored raw transaction");
    assert!(o.receipt(hashes[0]).await.status(), "reverted");
}

/// Two wallets' batches that link and bind one identity, queued together: the second
/// waits until the first lands, then fails its check unsent. The relayer gains the
/// first's refund net of its value and gas, and pays nothing for the second.
#[tokio::test]
async fn colliding_batches_are_sent_one_at_a_time() {
    let mut o = Onboarding::new(true).await;
    let r = o.start(&db("collide")).await.unwrap();
    o.owner.anvil_set_auto_mine(false).await.unwrap();
    let first = o.queue(&r).await;
    o.wallet_at(U256::ONE).await;
    let second = o.queue(&r).await;
    let before = o.owner.get_balance(o.relayer_address).await.unwrap();
    r.tick().await.unwrap();
    let held = r.status(second).await.unwrap().status;
    assert!(matches!(held, Status::Queued), "{held:?}");
    let hash = o.settle(&r, first, included).await.tx_hashes[0];
    let failed = |s: &Status| matches!(s, Status::Failed(Refusal::Reverted { .. }));
    assert!(o.settle(&r, second, failed).await.tx_hashes.is_empty());
    let receipt = o.receipt(hash).await;
    let gas = U256::from(receipt.gas_used) * U256::from(receipt.effective_gas_price);
    let after = o.owner.get_balance(o.relayer_address).await.unwrap();
    assert_eq!(after, before + o.refund() - o.value - gas);
}

/// A send priced out by the base fee is replaced when its window ends, long before its
/// deadline, by a 0-value self-send at its nonce, +10% on both fees and its cap at least
/// twice the base fee. Lost by the node with the base fee below its cap, that cancel is
/// sent again; above it, a new one is signed. `cancelled`, its id is not queued again,
/// and a new batch lands.
#[tokio::test]
async fn a_send_priced_out_is_cancelled_when_its_window_ends() {
    let (o, path) = (Onboarding::new(true).await, db("cancel"));
    let r = o.start(&path).await.unwrap();
    let m = o.config().max_fee_percent.into();
    let (id, request, sent, _) = o.priced_out(&r, m).await;
    let latest = o.owner.get_block_by_number(Default::default()).await;
    let base = latest.unwrap().unwrap().header.base_fee_per_gas.unwrap();
    let hashes = r.status(id).await.unwrap().tx_hashes;
    let cancel = o.tx(hashes[1]).await;
    for _ in 0..3 {
        o.owner.anvil_drop_transaction(hashes[1]).await.unwrap();
        o.owner.evm_mine(None).await.unwrap();
        r.tick().await.unwrap();
    }
    assert_eq!(
        r.status(id).await.unwrap().tx_hashes,
        hashes,
        "raised below its cap"
    );
    let again = o.owner.get_transaction_by_hash(hashes[1]).await.unwrap();
    assert!(again.is_some(), "not sent again");
    let (c, s) = (&cancel.inner, &sent.inner);
    o.base_fee(2 * c.max_fee_per_gas()).await;
    o.owner.evm_mine(None).await.unwrap();
    r.tick().await.unwrap();
    o.base_fee(1_000_000_000).await;
    o.owner.evm_mine(None).await.unwrap();
    let cancelled = |s: &Status| matches!(s, Status::Cancelled { .. });
    let report = o.settle(&r, id, cancelled).await;
    assert_eq!(report.tx_hashes[..2], hashes);
    let raised = o.tx(report.tx_hashes[2]).await;
    let to_self = (Some(o.relayer_address), U256::ZERO, s.nonce());
    assert_eq!((c.to(), c.value(), c.nonce()), to_self);
    assert!(c.max_fee_per_gas() * 10 >= s.max_fee_per_gas() * 11);
    assert!(
        c.max_fee_per_gas() >= 2 * u128::from(base),
        "below 2 × base"
    );
    let tip = |t: &Transaction| t.inner.max_priority_fee_per_gas().unwrap();
    assert!(tip(&cancel) * 10 >= tip(&sent) * 11);
    let twice = 4 * c.max_fee_per_gas(); // twice the base fee it was raised at
    assert_eq!(raised.inner.max_fee_per_gas(), twice, "not 2 × base");
    assert!(tip(&raised) * 10 >= tip(&cancel) * 11);
    drop(r);
    let r = o.start(&path).await.unwrap();
    assert_eq!(r.relay(&request).await.unwrap(), id);
    let again = r.status(id).await.unwrap();
    let kept = cancelled(&again.status) && again.tx_hashes == report.tx_hashes;
    assert!(kept, "queued again: {:?}", again.status);
    let retry = o.onboard(&r, 1800).await.unwrap();
    o.settle(&r, retry, included).await;
    o.assert_onboarded(o.refund()).await;
}

/// A send the node lost that no longer runs is cancelled at once, inside its window,
/// at 1.1× its cap even when the base fee fell below a tenth of it.
#[tokio::test]
async fn a_lost_send_that_no_longer_runs_is_cancelled_at_once() {
    let o = Onboarding::new(true).await;
    let r = o.start(&db("lost")).await.unwrap();
    o.owner.anvil_set_auto_mine(false).await.unwrap();
    let id = o.queue(&r).await;
    r.tick().await.unwrap();
    let sent = r.status(id).await.unwrap().tx_hashes[0];
    let cap = o.tx(sent).await.inner.max_fee_per_gas();
    o.owner.anvil_drop_transaction(sent).await.unwrap();
    o.set_code(o.stack.factory, "5f5ffd").await; // the factory now reverts
    o.base_fee(cap / 1000).await; // 1.1× the send's cap is above the max cancel fee
    o.owner.evm_mine(None).await.unwrap();
    r.tick().await.unwrap();
    let hashes = r.status(id).await.unwrap().tx_hashes;
    assert_eq!(hashes.len(), 2, "not cancelled at once");
    o.settle(&r, id, |s| matches!(s, Status::Cancelled)).await;
}

/// The original wins the nonce over its cancel: resolved from its receipt, no pause.
#[tokio::test]
async fn the_original_landing_before_its_cancel_resolves_from_its_receipt() {
    let o = Onboarding::new(true).await;
    let r = o.start(&db("race")).await.unwrap();
    let m = o.config().max_fee_percent.into();
    let (id, _, sent, raw) = o.priced_out(&r, m).await;
    r.tick().await.unwrap();
    let hashes = r.status(id).await.unwrap().tx_hashes;
    assert_eq!(hashes.len(), 2, "no cancel at the window's end");
    o.owner.anvil_drop_transaction(hashes[1]).await.unwrap();
    o.base_fee(sent.inner.max_fee_per_gas() / 2).await;
    o.owner.evm_mine(None).await.unwrap();
    let _pending = o.owner.send_raw_transaction(&raw).await.unwrap();
    o.owner.evm_mine(None).await.unwrap();
    assert!(o.mined(*sent.inner.tx_hash()).await);
    o.settle(&r, id, included).await;
    assert!(!r.paused().await, "an alarm on our own receipt");
}

/// A foreign nonce pauses sending, and our stored nonce under it refuses start.
#[tokio::test]
async fn a_foreign_nonce_at_runtime_pauses_sending() {
    let (o, db) = (Onboarding::new(true).await, db("pause"));
    let r = o.start(&db).await.unwrap();
    o.foreign().await;
    o.onboard(&r, 3600).await.unwrap();
    let _ = r.tick().await;
    assert!(r.paused().await, "no pause on a foreign nonce");
    let _ = r.tick().await;
    assert_eq!(o.relayer_nonce().await, 1, "sent while paused");
    drop(r);
    let refused = o.start(&db).await.err().unwrap();
    assert!(refused.contains("inspect the key's nonce"), "{refused}");
}

/// A foreign transaction mined at a stored send's nonce after its window pauses sending
/// once our receipt is still missing `receipt_lag_blocks` later, with no cancel signed,
/// and stays paused, another wallet's batch held; the stored send without a receipt
/// refuses start.
#[tokio::test]
async fn a_foreign_transaction_at_a_stored_nonce_pauses_sending() {
    let (mut o, db) = (Onboarding::new(true).await, db("taken"));
    o.owner.anvil_set_auto_mine(false).await.unwrap();
    let r = o.start(&db).await.unwrap();
    let id = o.onboard(&r, 3600).await.unwrap();
    r.tick().await.unwrap();
    let hash = r.status(id).await.unwrap().tx_hashes[0];
    o.wallet_at(U256::ONE).await;
    let next = o.queue(&r).await;
    o.owner.anvil_drop_transaction(hash).await.unwrap();
    let late = o.now().await + o.config().inclusion_window.as_secs();
    o.owner.anvil_set_next_block_timestamp(late).await.unwrap();
    o.owner.anvil_set_auto_mine(true).await.unwrap();
    o.foreign().await;
    for lag in 0..=o.config().receipt_lag_blocks {
        assert!(
            !r.paused().await,
            "paused {lag} blocks after a missing receipt"
        );
        r.tick().await.unwrap();
        o.owner.evm_mine(None).await.unwrap();
    }
    r.tick().await.unwrap();
    assert!(r.paused().await, "no pause on a taken nonce");
    assert_eq!(r.status(id).await.unwrap().tx_hashes, [hash], "cancelled");
    let held = r.status(next).await.unwrap().status;
    assert!(matches!(held, Status::Queued), "sent past it: {held:?}");
    drop(r);
    let refused = o.start(&db).await.err().unwrap();
    assert!(refused.contains("inspect the key's nonce"), "{refused}");
}

/// On an OP-stack chain a rise of the oracle's bound since the check that the refund
/// no longer covers fails a send for `value + limit × floor + k × U`; one it covers
/// sends. A balance below a send's worst cost fails it unpaused, and one of exactly
/// `gasLimit × maxFeePerGas + k × U + value` sends, within the refund's L1 part.
#[tokio::test]
async fn a_balance_of_the_worst_cost_sends_on_an_op_stack_chain() {
    let o = Onboarding::new(true).await;
    let mut config = o.config();
    config.l1_fee = L1Fee::OpStack;
    // An oracle whose `getL1FeeUpperBound` is `wei`.
    let oracle = async |wei: u128| {
        o.set_code(ORACLE, &format!("7f{wei:064x}5f5260205ff3"))
            .await
    };
    let gwei = 1_000_000_000u128;
    // `k × U` a gwei under `R - value`: a cap far below the floor.
    let short = (u128::try_from(o.refund() - o.value).unwrap() - gwei) * 10 / 13;
    oracle(10 * gwei).await;
    let k_u = U256::from(13 * gwei);
    let r = Relayer::start(o.relayer.clone(), config, &db("l1")).await;
    let (r, request) = (r.unwrap(), o.request_for(&o.onboarding()).await);
    let id = r.relay(&request).await.unwrap();
    oracle(short).await;
    let pinned = o.owner.get_block_by_number(Default::default()).await;
    let base = u128::from(pinned.unwrap().unwrap().header.base_fee_per_gas.unwrap());
    r.tick().await.unwrap();
    let status = r.status(id).await.unwrap().status;
    let Status::Failed(Refusal::RefundTooLow { refund, required }) = status else {
        panic!("{status:?}")
    };
    assert_eq!(refund, o.refund());
    oracle(10 * gwei).await;
    assert_eq!(r.relay(&request).await.unwrap(), id);
    oracle(20 * gwei).await;
    o.set_balance(o.relayer_address, U256::from(1)).await;
    r.tick().await.unwrap();
    let status = r.status(id).await.unwrap().status;
    let Status::Failed(Refusal::RelayerUnderfunded {
        required: worst, ..
    }) = status
    else {
        panic!("{status:?}")
    };
    assert!(!r.paused().await, "paused by one request");
    o.set_balance(o.relayer_address, worst).await;
    oracle(10 * gwei).await;
    assert_eq!(r.relay(&request).await.unwrap(), id);
    oracle(20 * gwei).await;
    let hashes = o.settle(&r, id, included).await.tx_hashes;
    o.assert_within_refund(&hashes, k_u * U256::from(2)).await;
    let t = o.tx(hashes[0]).await.inner;
    let cap = U256::from(t.gas_limit()) * U256::from(t.max_fee_per_gas());
    assert_eq!(worst, cap + k_u * U256::from(2) + o.value);
    let floor = U256::from(relayer::config::Eip1559::L1.grow(base, 5));
    let l1 = U256::from(short * 13 / 10);
    assert_eq!(required, o.value + U256::from(t.gas_limit()) * floor + l1);
}

/// With M under the fee rule's growth over the window, a request sends at the floor.
#[tokio::test]
async fn a_max_fee_percent_under_the_window_growth_still_sends() {
    let o = Onboarding::new(true).await;
    let config = Config {
        max_fee_percent: 100,
        ..o.config()
    };
    let r = Relayer::start(o.relayer.clone(), config, &db("low-m")).await;
    let (r, latest) = (r.unwrap(), o.owner.get_block_by_number(Default::default()));
    let base = u128::from(
        latest
            .await
            .unwrap()
            .unwrap()
            .header
            .base_fee_per_gas
            .unwrap(),
    );
    let id = o.queue(&r).await;
    let hash = o.settle(&r, id, included).await.tx_hashes[0];
    let floor = relayer::config::Eip1559::L1.grow(base, 5);
    assert!(o.tx(hash).await.inner.max_fee_per_gas() >= floor);
}

/// A transaction removed by `evm_revert` is sent again, the same raw transaction.
#[tokio::test]
async fn a_transaction_removed_by_evm_revert_is_sent_again() {
    let o = Onboarding::new(true).await;
    let r = o.start(&db("reorg")).await.unwrap();
    let id = o.onboard(&r, 3600).await.unwrap();
    let snapshot = o.owner.anvil_snapshot().await.unwrap();
    let hashes = o.settle(&r, id, included).await.tx_hashes;
    assert!(o.owner.anvil_revert(snapshot).await.unwrap());
    assert!(!o.mined(hashes[0]).await);
    // Queued while the node's nonce is below ours; the resent batch then reverts it.
    let late = o.onboard(&r, 1800).await.unwrap();
    o.owner.anvil_set_auto_mine(false).await.unwrap();
    for _ in 0..50 {
        r.tick().await.unwrap();
        o.owner.evm_mine(None).await.unwrap();
    }
    assert!(o.mined(hashes[0]).await, "not sent again");
    assert_eq!(r.status(id).await.unwrap().tx_hashes, hashes);
    let late = r.status(late).await.unwrap().status;
    let reverted = matches!(late, Status::Failed(Refusal::Reverted { .. }));
    assert!(reverted && !r.paused().await, "{late:?}");
}

/// A queued request whose cap from R falls below the pinned base fee's growth over the
/// window while it waits behind a send fails `RefundTooLow` unsent once that send is
/// cancelled, and the next is sent in the same tick.
#[tokio::test]
async fn a_request_priced_out_while_queued_fails_and_the_next_is_sent() {
    let mut o = Onboarding::new(true).await;
    let r = o.start(&db("outbid")).await.unwrap();
    o.owner.anvil_set_auto_mine(false).await.unwrap();
    let ahead = o.queue(&r).await;
    r.tick().await.unwrap();
    let latest = o.owner.get_block_by_number(Default::default()).await;
    let header = latest.unwrap().unwrap().header;
    let base = u128::from(header.base_fee_per_gas.unwrap());
    let refund = o.value + U256::from(GAS_LIMIT) * U256::from(10 * base);
    o.wallet_at(U256::ONE).await;
    let low = o.request_for(&o.batch(o.repay(refund))).await;
    let low = r.relay(&low).await.unwrap();
    o.wallet_at(U256::from(2)).await;
    let next = o.queue(&r).await;
    o.base_fee(100 * base).await;
    let late = header.timestamp + o.config().inclusion_window.as_secs();
    o.owner.anvil_set_next_block_timestamp(late).await.unwrap();
    o.owner.evm_mine(None).await.unwrap();
    o.settle(&r, ahead, |s| matches!(s, Status::Cancelled))
        .await;
    let report = r.status(low).await.unwrap();
    let failed = matches!(report.status, Status::Failed(Refusal::RefundTooLow { .. }));
    assert!(failed && report.tx_hashes.is_empty(), "{:?}", report.status);
    assert!(matches!(r.status(next).await.unwrap().status, Status::Sent));
    o.settle(&r, next, included).await;
}

/// A request whose deadline no longer covers the window fails unsent at its send.
#[tokio::test]
async fn a_request_expired_before_its_send_fails_unsent() {
    let o = Onboarding::new(true).await;
    let r = o.start(&db("stale")).await.unwrap();
    let (now, calls) = (o.now().await, encode_executions(&o.onboarding()));
    let request = o.request(o.sign_until(calls, o.value, now, now + 60));
    let id = r.relay(&request).await.unwrap();
    o.owner.anvil_increase_time(58).await.unwrap();
    o.owner.evm_mine(None).await.unwrap();
    r.tick().await.unwrap();
    let report = r.status(id).await.unwrap();
    let expired = matches!(report.status, Status::Failed(Refusal::Expired));
    assert!(expired, "{:?}", report.status);
    assert!(
        report.tx_hashes.is_empty() && o.relayer_nonce().await == 0,
        "sent"
    );
    let again = r.relay(&request).await;
    assert!(
        matches!(again, Err(Refusal::Expired)),
        "not checked again: {again:?}"
    );
}

/// A landed send's receipt cost goes on its request row, seen after it was queued,
/// across a restart before its receipt was read.
#[tokio::test]
async fn a_landed_send_records_its_receipt_cost() {
    let (o, path) = (Onboarding::new(true).await, db("ledger"));
    let r = o.start(&path).await.unwrap();
    let id = o.onboard(&r, 3600).await.unwrap();
    r.tick().await.unwrap();
    o.owner.evm_mine(None).await.unwrap(); // mined, its receipt unread
    drop(r);
    let r = o.start(&path).await.unwrap();
    let report = o.settle(&r, id, included).await;
    let receipt = o.owner.get_transaction_receipt(report.tx_hashes[0]).await;
    let receipt = receipt.unwrap().unwrap();
    let paid = U256::from(receipt.gas_used) * U256::from(receipt.effective_gas_price);
    let cost = report.cost.expect("no cost recorded");
    assert_eq!(U256::from(cost.gas_used) * cost.gas_price, paid);
    assert_eq!(cost.l1_fee, U256::ZERO, "an L1 fee on anvil");
    let (created, sent) = (report.created_at, report.sent_at[0]);
    assert!(created > 0 && sent >= created && cost.included_at >= sent);
}

/// A balance below `min_balance` pauses sending, even one that covers the send, and
/// leaves it queued with no nonce; funded to the floor, the next tick sends.
#[tokio::test]
async fn an_underfunded_relayer_pauses_before_its_send() {
    let (o, floor) = (Onboarding::new(true).await, U256::from(CLAIMED));
    let config = Config {
        min_balance: floor,
        ..o.config()
    };
    let r = Relayer::start(o.relayer.clone(), config, &db("underfunded")).await;
    let r = r.unwrap();
    let id = o.onboard(&r, 3600).await.unwrap();
    // The value, not its gas; then the send's cost, below the floor.
    for balance in [o.value, floor - U256::ONE] {
        o.set_balance(o.relayer_address, balance).await;
        let refused = r.tick().await.expect_err("sent underfunded").to_string();
        assert!(refused.contains("min_balance"), "{refused}");
        assert!(r.paused().await, "no pause");
        let report = r.status(id).await.unwrap();
        let queued =
            matches!(report.status, Status::Queued) && report.tx_hashes.is_empty();
        assert!(queued && o.relayer_nonce().await == 0, "sent");
    }
    o.set_balance(o.relayer_address, floor).await;
    o.settle(&r, id, included).await;
    assert!(!r.paused().await, "paused while funded");
}

/// A first send the node refuses is uncommitted, queued again, and pauses.
#[tokio::test]
async fn a_refused_first_send_is_queued_again_and_pauses() {
    let o = Onboarding::new(true).await;
    let r = o.start(&db("refused")).await.unwrap();
    o.owner.anvil_set_auto_mine(false).await.unwrap();
    // A pending transaction at nonce 0 that the send cannot replace.
    let gwei = 1_000_000_000;
    let ahead = TransactionRequest::default()
        .with_to(o.relayer_address)
        .with_max_fee_per_gas(1000 * gwei)
        .with_max_priority_fee_per_gas(100 * gwei);
    let pending = o.relayer.send_transaction(ahead).await.unwrap();
    let id = o.onboard(&r, 3600).await.unwrap();
    let refused = r.tick().await.expect_err("refusal unreported").to_string();
    assert!(refused.contains("refused"), "{refused}");
    let report = r.status(id).await.unwrap();
    assert!(
        matches!(report.status, Status::Queued),
        "{:?}",
        report.status
    );
    assert!(
        report.tx_hashes.is_empty() && r.paused().await,
        "sent or unpaused"
    );
    // Its nonce free again: still paused, nothing sent.
    let ahead = *pending.tx_hash();
    o.owner.anvil_drop_transaction(ahead).await.unwrap();
    r.tick().await.unwrap();
    let report = r.status(id).await.unwrap();
    assert!(report.tx_hashes.is_empty(), "sent while paused");
}

/// The key's nonce taken between the pinned block and the broadcast: the node answers
/// "nonce too low", the attempt is dropped unpaused, and the next pinned block's check
/// finds the foreign nonce and pauses, nothing signed past it.
#[tokio::test]
async fn a_nonce_taken_before_broadcast_is_checked_again_from_a_new_block() {
    let o = Onboarding::new(true).await;
    let r = o.racing(RACE, "low").await;
    let id = o.queue(&r).await;
    r.tick().await.unwrap();
    let report = r.status(id).await.unwrap();
    let dropped = matches!(report.status, Status::Queued) && report.tx_hashes.is_empty();
    assert!(dropped && !r.paused().await, "{:?}", report.status);
    r.tick().await.unwrap();
    assert!(r.paused().await, "no pause on the foreign nonce");
    assert_eq!(o.relayer_nonce().await, 1, "signed past the taken nonce");
}

/// A first send the node answers with an error but holds keeps its commit and lands.
#[tokio::test]
async fn a_first_send_the_node_holds_despite_an_error_keeps_its_commit() {
    let o = Onboarding::new(true).await;
    let r = o.racing(ECHO, "echo").await;
    let id = o.queue(&r).await;
    o.settle(&r, id, included).await; // unsettled if uncommitted
    o.assert_onboarded(o.refund()).await;
}

/// A node error on a send's run holds the queue for `node_error_ticks` ticks, then
/// fails it unsent as `Node`; one naming funds fails it at once.
#[tokio::test]
async fn a_node_error_on_a_run_fails_its_send_after_its_ticks() {
    let o = Onboarding::new(true).await;
    for (mode, ticks) in [(STALL, o.config().node_error_ticks), (BROKE, 1)] {
        let path = db("node");
        let id = o.queue(&o.start(&path).await.unwrap()).await;
        let failing = Racing(o.relayer.clone(), std::sync::Arc::new(mode.into()));
        let r = Relayer::start(failing, o.config(), &path).await.unwrap();
        for _ in 1..ticks {
            r.tick().await.expect_err("no node error");
            let held = r.status(id).await.unwrap().status;
            assert!(matches!(held, Status::Queued), "{held:?}");
        }
        r.tick().await.unwrap();
        let report = r.status(id).await.unwrap();
        let node = matches!(report.status, Status::Failed(Refusal::Node(_)));
        assert!(node && report.tx_hashes.is_empty(), "{:?}", report.status);
    }
}

/// With the max cancel fee under twice the base fee, a cancel's cap is that max.
#[tokio::test]
async fn a_cancel_is_capped_at_the_max_cancel_fee() {
    let o = Onboarding::new(true).await;
    let config = Config {
        max_fee_percent: 100,
        max_cancel_fee_percent: 150,
        ..o.config()
    };
    let r = Relayer::start(o.relayer.clone(), config, &db("max-cancel")).await;
    let r = r.unwrap();
    let (id, ..) = o.priced_out(&r, 100).await;
    let latest = o.owner.get_block_by_number(Default::default()).await;
    let base = latest.unwrap().unwrap().header.base_fee_per_gas.unwrap();
    let report = r.status(id).await.unwrap();
    let cancel = o.tx(report.tx_hashes[1]).await.inner;
    assert_eq!(cancel.max_fee_per_gas(), u128::from(base) * 3 / 2);
}
