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

    /// Sent with automine off, priced out, then at its deadline: still pending.
    async fn priced_out(&self, r: &Rel) -> (B256, Request, Transaction, Bytes) {
        self.owner.anvil_set_auto_mine(false).await.unwrap();
        let deadline = self.now().await + 60;
        let calls = encode_executions(&self.onboarding());
        let request =
            self.request(self.sign_until(calls, self.value, deadline - 60, deadline));
        let id = r.relay(&request).await.unwrap();
        r.tick().await.unwrap();
        let report = r.status(id).await.unwrap();
        assert!(matches!(report.status, Status::Sent), "{:?}", report.status);
        let hash = report.tx_hashes[0];
        let raw = self.owner.get_raw_transaction_by_hash(hash).await.unwrap();
        let tx = self.tx(hash).await;
        self.assert_within_refund(&[hash], U256::ZERO).await;
        self.base_fee(tx.inner.max_fee_per_gas() * 100).await;
        self.owner.evm_mine(None).await.unwrap();
        r.tick().await.unwrap(); // priced out, but before its deadline
        let early = r.status(id).await.unwrap().tx_hashes;
        assert_eq!(early, [hash], "cancelled before its deadline");
        // The latest block at the deadline: the next tick cancels.
        let at = self.owner.anvil_set_next_block_timestamp(deadline);
        at.await.unwrap();
        self.owner.evm_mine(None).await.unwrap();
        assert!(!self.mined(hash).await, "mined above its fee cap");
        (id, request, tx, raw.unwrap())
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
    assert!(refused.contains("relayer reconcile"), "{refused}");
}

/// An instance holds `flock` on `<db>.lock`; any other holder refuses start. A
/// database of another schema version is refused, naming the fix.
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
}

/// A committed raw transaction the node never got is sent again at start, once.
#[tokio::test]
async fn a_crash_between_commit_and_broadcast_lands_once() {
    let (o, db) = (Onboarding::new(true).await, db("crash"));
    o.owner.anvil_set_auto_mine(false).await.unwrap();
    let r = o.start(&db).await.unwrap();
    let id = o.onboard(&r, 3600).await.unwrap();
    r.tick().await.unwrap();
    let hashes = r.status(id).await.unwrap().tx_hashes;
    o.owner.anvil_drop_transaction(hashes[0]).await.unwrap();
    drop(r);
    let r = o.start(&db).await.unwrap();
    let report = o.settle(&r, id, included).await;
    assert_eq!(report.tx_hashes, hashes, "not the stored raw transaction");
    o.assert_onboarded(o.refund()).await;
}

/// A send priced out by the base fee is replaced at its deadline by a 0-value
/// self-send at its nonce, +10% on both fees; `cancelled`, its id not queued again,
/// and a new batch lands.
#[tokio::test]
async fn a_send_priced_out_is_cancelled_at_its_deadline() {
    let o = Onboarding::new(true).await;
    let r = o.start(&db("cancel")).await.unwrap();
    let (id, request, sent, _) = o.priced_out(&r).await;
    let latest = o.owner.get_block_by_number(Default::default()).await;
    let base = latest.unwrap().unwrap().header.base_fee_per_gas.unwrap();
    let cancelled = |s: &Status| matches!(s, Status::Cancelled { .. });
    let report = o.settle(&r, id, cancelled).await;
    let (first, cancel) = (report.tx_hashes[0], report.tx_hashes[1]);
    assert_eq!(first, *sent.inner.tx_hash());
    let cancel = o.tx(cancel).await;
    let (c, s) = (&cancel.inner, &sent.inner);
    let to_self = (Some(o.relayer_address), U256::ZERO, s.nonce());
    assert_eq!((c.to(), c.value(), c.nonce()), to_self);
    assert!(c.max_fee_per_gas() * 10 >= s.max_fee_per_gas() * 11);
    assert!(
        c.max_fee_per_gas() >= 2 * u128::from(base),
        "cap below 2 × base fee"
    );
    let tip = |t: &Transaction| t.inner.max_priority_fee_per_gas().unwrap();
    assert!(tip(&cancel) * 10 >= tip(&sent) * 11);
    o.base_fee(1_000_000_000).await;
    o.owner.evm_mine(None).await.unwrap();
    assert_eq!(r.relay(&request).await.unwrap(), id);
    let again = r.status(id).await.unwrap();
    let kept = cancelled(&again.status) && again.tx_hashes == report.tx_hashes;
    assert!(kept, "queued again: {:?}", again.status);
    let retry = o.onboard(&r, 1800).await.unwrap();
    o.settle(&r, retry, included).await;
    o.assert_onboarded(o.refund()).await;
}

/// The original wins the nonce over its cancel: resolved from its receipt, no pause.
#[tokio::test]
async fn the_original_landing_before_its_cancel_resolves_from_its_receipt() {
    let o = Onboarding::new(true).await;
    let r = o.start(&db("race")).await.unwrap();
    let (id, _, sent, raw) = o.priced_out(&r).await;
    r.tick().await.unwrap();
    let hashes = r.status(id).await.unwrap().tx_hashes;
    assert_eq!(hashes.len(), 2, "no cancel at the deadline");
    o.owner.anvil_drop_transaction(hashes[1]).await.unwrap();
    o.base_fee(1_000_000_000).await;
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
    assert!(refused.contains("relayer reconcile"), "{refused}");
}

/// A foreign transaction mined at a stored send's nonce pauses its rebroadcast,
/// and the stored send without a receipt refuses start.
#[tokio::test]
async fn a_foreign_transaction_at_a_stored_nonce_pauses_sending() {
    let (o, db) = (Onboarding::new(true).await, db("taken"));
    o.owner.anvil_set_auto_mine(false).await.unwrap();
    let r = o.start(&db).await.unwrap();
    let id = o.onboard(&r, 3600).await.unwrap();
    r.tick().await.unwrap();
    let hash = r.status(id).await.unwrap().tx_hashes[0];
    o.owner.anvil_drop_transaction(hash).await.unwrap();
    o.owner.anvil_set_auto_mine(true).await.unwrap();
    o.foreign().await;
    r.tick().await.unwrap();
    assert!(r.paused().await, "no pause on a taken nonce");
    drop(r);
    let refused = o.start(&db).await.err().unwrap();
    assert!(refused.contains("relayer reconcile"), "{refused}");
}

/// On an OP-stack chain a balance of exactly `gasLimit × maxFeePerGas + k × U + value`
/// sends, within the refund's L1 part.
#[tokio::test]
async fn a_balance_of_the_worst_cost_sends_on_an_op_stack_chain() {
    let o = Onboarding::new(true).await;
    let mut config = o.config();
    config.l1_fee = L1Fee::OpStack;
    // An oracle whose `getL1FeeUpperBound` is 10 gwei.
    o.set_code(ORACLE, "6402540be4005f5260205ff3").await;
    let k_u = U256::from(13_000_000_000u64);
    let path = db("l1");
    let r = Relayer::start(o.relayer.clone(), config.clone(), &path).await;
    let r = r.unwrap();
    let id = o.onboard(&r, 1800).await.unwrap();
    o.set_balance(o.relayer_address, U256::from(1)).await;
    let refused = r.tick().await.unwrap_err().to_string();
    let worst = refused.split(' ').nth(6).unwrap().trim_end_matches(':');
    let worst: U256 = worst.parse().unwrap();
    o.set_balance(o.relayer_address, worst).await;
    drop(r);
    let r = Relayer::start(o.relayer.clone(), config, &path)
        .await
        .unwrap();
    let hashes = o.settle(&r, id, included).await.tx_hashes;
    o.assert_within_refund(&hashes, k_u).await;
    let t = o.tx(hashes[0]).await.inner;
    let cap = U256::from(t.gas_limit()) * U256::from(t.max_fee_per_gas());
    assert_eq!(worst, cap + k_u + o.value);
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
    for _ in 0..50 {
        r.tick().await.unwrap();
    }
    assert!(o.mined(hashes[0]).await, "not sent again");
    assert_eq!(r.status(id).await.unwrap().tx_hashes, hashes);
    let late = r.status(late).await.unwrap().status;
    let reverted = matches!(late, Status::Failed(Refusal::Reverted { .. }));
    assert!(reverted && !r.paused().await, "{late:?}");
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

/// A landed send's receipt cost goes on its request row, seen after it was queued;
/// it is confirmed once the safe head reaches it.
#[tokio::test]
async fn a_landed_send_records_its_receipt_cost() {
    let o = Onboarding::new(true).await;
    let r = o.start(&db("ledger")).await.unwrap();
    let id = o.onboard(&r, 3600).await.unwrap();
    let report = o.settle(&r, id, included).await;
    let receipt = o.owner.get_transaction_receipt(report.tx_hashes[0]).await;
    let receipt = receipt.unwrap().unwrap();
    let paid = U256::from(receipt.gas_used) * U256::from(receipt.effective_gas_price);
    let cost = report.cost.expect("no cost recorded");
    assert_eq!(U256::from(cost.gas_used) * cost.gas_price, paid);
    assert_eq!(cost.l1_fee, U256::ZERO, "an L1 fee on anvil");
    let (created, sent) = (report.created_at, report.sent_at[0]);
    assert!(created > 0 && sent >= created && cost.included_at >= sent);
    o.owner.anvil_mine(Some(32), None).await.unwrap(); // anvil's safe head lags 32
    r.tick().await.unwrap();
    let status = r.status(id).await.unwrap().status;
    let block = receipt.block_number.unwrap();
    let confirmed = Status::Confirmed {
        block,
        reverted: false,
    };
    assert_eq!(format!("{status:?}"), format!("{confirmed:?}"));
}

/// A balance below a send's worst cost leaves it queued with no nonce, and pauses.
#[tokio::test]
async fn an_underfunded_relayer_pauses_before_its_send() {
    let o = Onboarding::new(true).await;
    let r = o.start(&db("underfunded")).await.unwrap();
    let id = o.onboard(&r, 3600).await.unwrap();
    // The send's gas at its cap is above `R - value - GAS_LIMIT`.
    let short = o.refund() - U256::from(GAS_LIMIT);
    o.set_balance(o.relayer_address, short).await;
    let refused = r.tick().await.expect_err("sent underfunded").to_string();
    assert!(refused.contains("below a send's cost"), "{refused}");
    assert!(r.paused().await, "no pause");
    let report = r.status(id).await.unwrap();
    assert!(
        matches!(report.status, Status::Queued),
        "{:?}",
        report.status
    );
    assert!(
        report.tx_hashes.is_empty() && o.relayer_nonce().await == 0,
        "sent"
    );
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
    let _pending = o.relayer.send_transaction(ahead).await.unwrap();
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
}
