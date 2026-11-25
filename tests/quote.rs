//! The quote and the relay-time gas limit on anvil, on the create path and existing
//! factory wallets. Needs `anvil` on `PATH`.

#![allow(dead_code, unused_imports)]

include!("common/onboarding.rs");

use alloy::{
    primitives::keccak256,
    sol_types::SolError,
};
use libid_contracts::deploy::deploy_with_ctor;
use relayer::{
    check::Checked,
    config::FeeParams,
    quote::{
        Draft,
        Quote,
    },
};

alloy::sol! { function transfer(address to, uint256 amount) returns (bool); }

/// Records the gas it got in slot 1, calls itself to burn about 160k gas, and
/// records whether that call succeeded in slot 0 instead of reverting.
const SWALLOWER: &str =
    "366019575a60015560006000600160006000305af1600055005b6117705b6001900380601d5700";

/// `calls` sent with `value`, signed by `keys`, below quorum with proofs when `.3`.
struct Batch<'a>(
    Vec<Execution>,
    U256,
    Vec<&'a PrivateKeySigner>,
    Option<Vec<CallProof>>,
);

impl Onboarding {
    /// `b`'s data signing `call_gas`; with `None`, a draft a stranger signs.
    async fn authorize(&self, b: &Batch<'_>, call_gas: Option<u64>) -> Bytes {
        let w = IWebWallet::new(self.wallet, &self.owner);
        let (nonce, salt) = match self.path().await {
            None => (
                w.getNonce_1().call().await.unwrap(),
                w.eip712Domain().call().await.unwrap().salt,
            ),
            Some(_) => (U256::ZERO, wallet_salt(B256::ZERO, 0)),
        };
        let (now, version) = (self.now().await, self.stack.release.version);
        let message = ExecuteMessage {
            execution_calldata: encode_executions(&b.0),
            value: b.1,
            at: U48::from(now),
            nonce,
            deadline: U48::from(now + 3600),
            call_gas: U256::from(call_gas.unwrap_or(0)),
        };
        let mut digest = message.digest(self.chain_id, self.wallet, version, salt);
        if b.3.is_some() {
            digest = below_quorum_hash(digest, message.at, &key_of(b.2[0]));
        }
        let stranger = PrivateKeySigner::random();
        let signatures = b.2.iter().map(|k| {
            let signer = if call_gas.is_some() { *k } else { &stranger };
            let signature = signer.sign_hash_sync(&digest).unwrap().as_bytes();
            SessionSignature {
                key: key_of(k),
                signature: signature.to_vec().into(),
            }
        });
        let (signatures, calls) = (signatures.collect(), b.3.clone().unwrap_or_default());
        message.authorized_data(ExecuteAuthorization {
            signatures,
            calls,
            ..Default::default()
        })
    }

    async fn quote_with(&self, config: &Config, b: &Batch<'_>) -> Quote {
        self.try_quote(config, b).await.unwrap()
    }

    async fn try_quote(&self, config: &Config, b: &Batch<'_>) -> Result<Quote, Refusal> {
        let draft = Draft {
            wallet: self.wallet,
            create: self.path().await,
            data: self.authorize(b, None).await,
            below_quorum: b.3.is_some(),
        };
        draft.quote(&self.relayer, config).await
    }

    /// The request for `data` on the wallet's current path, priced at `gas_limit`.
    async fn request_at(&self, data: &Bytes, gas_limit: u64) -> Request {
        let create = self.path().await;
        Request {
            create,
            gas_limit,
            ..self.request(data.clone())
        }
    }

    /// The relayer's check of `data` at `gas_limit`, without sending.
    async fn check_at(&self, data: &Bytes, gas_limit: u64) -> Result<Checked, Refusal> {
        let request = self.request_at(data, gas_limit).await;
        request.check(&self.relayer, &self.config()).await
    }

    /// Onboards, then adds K2, an admin session, and K3 and K4, below quorum, whose
    /// scope is a refund of up to 1 ether and `token.transfer`, each 1 ether a day.
    async fn sessions(&self) -> ([PrivateKeySigner; 3], Address, [B256; 2]) {
        self.land(&self.sign(&self.onboarding()).await, self.value)
            .await;
        let token = Address::repeat_byte(0x70); // returns true to any call
        self.set_code(token, "600160005260206000f3").await;
        let leaf = |to: Address, selector: [u8; 4], max: u128| {
            let leaf = (to, B256::right_padding_from(&selector), U256::from(max));
            keccak256(keccak256(leaf.abi_encode()))
        };
        let refund = leaf(self.relayer_address, [0; 4], CLAIMED);
        let leaves = [refund, leaf(token, transferCall::SELECTOR, 0)];
        let root =
            keccak256((leaves[0].min(leaves[1]), leaves[0].max(leaves[1])).abi_encode());
        let period = U48::from(86_400);
        let allowances = [self.native, token].map(|token| Allowance {
            token,
            amount: CLAIMED,
            period,
        });
        let mut rules = Rules {
            scopeRoot: root,
            allowances: allowances.into(),
            ..Default::default()
        };
        rules.sort_allowances();
        let wallet = IWebWallet::new(self.wallet, &self.owner);
        let session = wallet.sessionOf(key_of(&self.k)).call().await.unwrap();
        let k = [(); 3].map(|_| PrivateKeySigner::random());
        let add = |k: &PrivateKeySigner, policy| {
            let add = (session.identity, key_of(k), policy);
            call(
                self.wallet,
                U256::ZERO,
                IWebWallet::addSessionCall::new(add).abi_encode(),
            )
        };
        let below = Policy {
            rules,
            ..Default::default()
        };
        let adds = vec![
            add(&k[0], admin()),
            add(&k[1], below.clone()),
            add(&k[2], below),
        ];
        let b = Batch(adds, U256::ZERO, vec![&self.k], None);
        self.land(&self.authorize(&b, Some(CALL_GAS)).await, U256::ZERO)
            .await;
        self.owner.anvil_increase_time(2).await.unwrap(); // the sessions are active
        self.owner.anvil_mine(Some(1), None).await.unwrap();
        (k, token, leaves)
    }
}

/// anvil's `eth_simulateV1` reports `gasUsed` and the revert data of a draft.
#[tokio::test]
async fn anvil_simulates_a_draft_to_execution_not_signed() {
    let o = Onboarding::new(true).await;
    let b = Batch(o.onboarding(), o.value, vec![&o.k], None);
    let sim = o.simulated(&o.authorize(&b, None).await, o.value).await;
    assert!(!sim.status && sim.gas_used > 21_000, "{sim:?}");
    // anvil 1.8 returns the revert data only in `error.data`.
    let data = match sim.error.and_then(|e| e.data) {
        Some(data) if sim.return_data.is_empty() => data,
        _ => sim.return_data,
    };
    assert_eq!(data[..], IWebWallet::ExecutionNotSigned::SELECTOR);
}

/// The create path: `callGas` is quoted for its dearest call; a `callGas` signed above
/// the quote is `GasLimitTooLow` with the
/// relay-time limit; the quoted one lands at that limit, and a last call that swallows
/// a failed inner call (`ReferralSplitter._deliver`) gets `callGas` and succeeds.
#[tokio::test]
async fn the_create_path_lands_at_the_relay_time_limit() {
    let o = Onboarding::new(true).await;
    let swallower = Address::repeat_byte(0x5a);
    o.set_code(swallower, SWALLOWER).await;
    let mut calls = o.onboarding();
    calls.push(call(swallower, U256::ZERO, vec![]));
    let b = Batch(calls, o.value, vec![&o.k], None);
    let q = o.quote_with(&o.config(), &b).await;
    // `callGas` covers the dearest call, the swallower, not the whole batch.
    let whole = o.simulated(&o.authorize(&b, None).await, o.value).await;
    assert!(
        q.call_gas < whole.gas_used,
        "{} of {}",
        q.call_gas,
        whole.gas_used
    );
    let latest = o.owner.get_block_by_number(Default::default()).await;
    let base = latest.unwrap().unwrap().header.base_fee_per_gas.unwrap();
    // At least the base fee grown over the quote's validity and the window, 15 blocks,
    // and at most what a send pays: that, or M × the base fee grown over the validity.
    let grow = |blocks| relayer::config::Eip1559::L1.grow(base.into(), blocks);
    let m = u128::from(o.config().max_fee_percent);
    let floor = grow(15);
    assert!(
        q.max_fee_per_gas >= floor,
        "{} under {floor}",
        q.max_fee_per_gas
    );
    let most = floor.max(grow(10) * m / 100);
    assert!(
        q.max_fee_per_gas <= most,
        "{} over {most}",
        q.max_fee_per_gas
    );

    let above = o.authorize(&b, Some(2 * q.call_gas)).await;
    let expected = o.limit(&above, o.value, 2 * q.call_gas).await;
    // Reported with its fillable zero bytes as nonzero, at 12 gas, run at the limit's
    // margin.
    let fill = 12 * fillable(&above, &[o.refund()]);
    let filled = o.limit_with(&above, o.value, 2 * q.call_gas, fill).await;
    let allowance = filled - expected;
    let mut required = 0;
    for gas_limit in [q.gas_limit, expected - 3] {
        let refused = o.check_at(&above, gas_limit).await.err();
        let Some(Refusal::GasLimitTooLow { required: r }) = refused else {
            panic!("{refused:?}")
        };
        assert!(
            r.abs_diff(expected + allowance) <= 2,
            "required {r}, expected {expected} + {allowance}"
        );
        required = r;
    }
    // A limit priced for exactly the batch's passes; one gas less does not.
    let exact = required - allowance;
    let refused = o.check_at(&above, exact - 1).await.err();
    assert!(
        matches!(refused, Some(Refusal::GasLimitTooLow { .. })),
        "{refused:?}"
    );
    o.check_at(&above, exact).await.unwrap();
    // Re-signed with fewer zero bytes in R, at the `required` reported, it passes.
    let refunding = |r| {
        let mut calls = o.batch(o.repay(r));
        calls.push(call(swallower, U256::ZERO, vec![]));
        Batch(calls, o.value, vec![&o.k], None)
    };
    let round = U256::from(1u64 << 58);
    let zeroed = o.authorize(&refunding(round), Some(2 * q.call_gas)).await;
    let refused = o.check_at(&zeroed, q.gas_limit).await.err();
    let Some(Refusal::GasLimitTooLow { required }) = refused else {
        panic!("{refused:?}")
    };
    let filled = refunding(round + U256::from(0x0101_0101_0101_0101u64));
    let filled = o.authorize(&filled, Some(2 * q.call_gas)).await;
    o.check_at(&filled, required).await.unwrap();

    let data = o.authorize(&b, Some(q.call_gas)).await;
    let limit = o.limit(&data, o.value, q.call_gas).await;
    let hash = o
        .send(&o.request_at(&data, q.gas_limit).await)
        .await
        .transaction_hash;
    let tx = o.tx(hash).await;
    let sent = tx.inner.gas_limit();
    assert!(
        sent.abs_diff(limit) <= 2,
        "sent at {sent}, expected {limit}"
    );
    let slot = |i: u64| o.owner.get_storage_at(swallower, U256::from(i));
    assert_eq!(
        slot(0).await.unwrap(),
        U256::ONE,
        "the swallowed call failed"
    );
    let got = slot(1).await.unwrap();
    assert!(got >= U256::from(q.call_gas), "it got less than callGas");
    // The batch again, on the wallet it created: a draft whose call fails is no quote.
    let draft = Draft {
        wallet: o.wallet,
        create: None,
        data: o.authorize(&b, None).await,
        below_quorum: false,
    };
    let refused = draft.quote(&o.relayer, &o.config()).await.err();
    let spent = |r: &Refusal| reverted(r, "DigestSpent"); // the link's, spent by its landing
    assert!(refused.as_ref().is_some_and(spent), "{refused:?}");
}

/// An existing wallet: two admin signers at quorum are quoted at least their run's
/// limit. They, one signer, and below quorum a native refund's
/// first spend and an ERC-20 transfer's first spend before one, each land at their
/// quoted limit.
#[tokio::test]
async fn session_batches_are_quoted_within_the_gas_margin() {
    let o = Onboarding::new(true).await;
    let ([k2, k3, k4], token, leaves) = o.sessions().await;
    let r = U256::from(CLAIMED / 100);
    let b = Batch(vec![o.repay(r)], U256::ZERO, vec![&o.k, &k2], None);
    let q = o.quote_with(&o.config(), &b).await;
    let data = o.authorize(&b, Some(q.call_gas)).await;
    let run = o.limit(&data, U256::ZERO, q.call_gas).await;
    assert!(q.gas_limit >= run, "{} for {run}", q.gas_limit);
    // Signed with a refund of fewer zero bytes than quoted: the quote overshoots the
    // run by exactly the zero bytes left in R, the signatures, `at` and the deadline,
    // at 12 gas run at the limit's margin.
    let signed_r = r + U256::from(0x0101_0101_0101u64);
    let signed = Batch(vec![o.repay(signed_r)], U256::ZERO, vec![&o.k, &k2], None);
    let data = o.authorize(&signed, Some(q.call_gas)).await;
    let fill = 12 * fillable(&data, &[signed_r]);
    let run = o.limit(&data, U256::ZERO, q.call_gas).await;
    let filled = o.limit_with(&data, U256::ZERO, q.call_gas, fill).await - run;
    let overshoot = q.gas_limit.saturating_sub(run);
    // 1 for rounding, 15 for a zero byte inside `call_gas`.
    assert!(overshoot.abs_diff(filled) <= 16, "{overshoot} for {filled}");
    // R's zero bytes leave the quote unchanged: the draft runs them cheaper by as much.
    let q2 = o.quote_with(&o.config(), &signed).await;
    let unreserved = |q: &Quote| q.gas_limit - reserve(q.call_gas, false);
    assert_eq!(unreserved(&q2), unreserved(&q));
    // So do the deadline's: one of no zero bytes is quoted as the draft's.
    let draft = o.authorize(&b, None).await;
    let (mut message, authorization) =
        ExecuteMessage::from_authorized_data(&draft).unwrap();
    message.deadline = U48::from(0x0101_0101_0101u64);
    let draft = Draft {
        wallet: o.wallet,
        create: None,
        data: message.authorized_data(authorization),
        below_quorum: false,
    };
    let q3 = draft.quote(&o.relayer, &o.config()).await.unwrap();
    assert_eq!(unreserved(&q3), unreserved(&q));
    o.send(&o.request_at(&data, q.gas_limit).await).await;

    let proof = |max: u128, leaf| CallProof {
        maxValue: U256::from(max),
        proof: vec![leaf],
    };
    let (refund, to_token) = (proof(CLAIMED, leaves[1]), proof(0, leaves[0]));
    let transfer = transferCall::new((o.relayer_address, U256::ONE)).abi_encode();
    let calls = vec![call(token, U256::ZERO, transfer.clone()), o.repay(r)];
    let below = |calls, k, proofs| Batch(calls, U256::ZERO, vec![k], Some(proofs));
    let cases = [
        Batch(vec![o.repay(r)], U256::ZERO, vec![&o.k], None),
        below(vec![o.repay(r)], &k3, vec![refund.clone()]),
        below(calls, &k4, vec![to_token, refund]),
    ];
    for b in &cases {
        let q = o.quote_with(&o.config(), b).await;
        assert!(r >= q.min_refund, "the fixture underpays");
        let data = o.authorize(b, Some(q.call_gas)).await;
        o.send(&o.request_at(&data, q.gas_limit).await).await;
    }
    // A refund drafted at 0 and signed at R: the quote covers sending value.
    let at_quorum = |calls| Batch(calls, U256::ZERO, vec![&o.k], None);
    let q = o
        .quote_with(&o.config(), &at_quorum(vec![o.repay(U256::ZERO)]))
        .await;
    let data = o
        .authorize(&at_quorum(vec![o.repay(r)]), Some(q.call_gas))
        .await;
    o.check_at(&data, q.gas_limit).await.unwrap();
    let unpaid = at_quorum(vec![call(token, U256::ZERO, transfer)]);
    let refused = o.try_quote(&o.config(), &unpaid).await.err();
    assert!(
        matches!(refused, Some(Refusal::RefundMissing)),
        "{refused:?}"
    );
}

/// A `callGas` far beyond the run's margin, on the create path and then on the wallet
/// it created, and the relayer lands each at the relay-time limit. On the wallet, the
/// same data one 64/63 factor of the reserve below that limit reverts
/// `InsufficientCallGas`. On the create path, under EIP-7825's 2^24 gas, the run's
/// margin covers one factor, so the least gas it runs at is shown above its gas used
/// plus the whole depth-4 reserve instead.
#[tokio::test]
async fn a_large_call_gas_lands_at_the_relay_time_limit() {
    let o = Onboarding::new(true).await;
    let r = U256::from(CLAIMED / 10);
    let onboarding = Batch(o.onboarding(), o.value, vec![&o.k], None);
    let repay = Batch(vec![o.repay(r)], U256::ZERO, vec![&o.k], None);
    for (b, times) in [(onboarding, 8), (repay, 1000)] {
        let call_gas = times * o.quote_with(&o.config(), &b).await.call_gas;
        let data = o.authorize(&b, Some(call_gas)).await;
        let limit = o.limit(&data, b.1, call_gas).await;
        let create = o.path().await.is_some();
        let tx = o.outer(&data, b.1).await;
        if create {
            let used = o.simulated(&data, b.1).await.gas_used;
            let tx = TransactionRequest { gas: None, ..tx };
            let least = o.relayer.estimate_gas(tx).await.unwrap();
            let reserve = reserve(call_gas, true);
            assert!(least > used + reserve, "{least} within {used} + {reserve}");
        } else {
            let short = limit - reserve(call_gas, false) / 64;
            let e = o.relayer.call(tx.with_gas_limit(short)).await.unwrap_err();
            let revert = e.as_error_resp().and_then(|r| r.as_revert_data());
            let revert = revert.unwrap_or_default();
            assert_eq!(
                revert[..4],
                IWebWallet::InsufficientCallGas::SELECTOR,
                "{e}"
            );
        }
        let hash = o
            .send(&o.request_at(&data, limit + 2).await)
            .await
            .transaction_hash;
        let sent = o.tx(hash).await.inner.gas_limit();
        assert!(
            sent.abs_diff(limit) <= 2,
            "sent at {sent}, expected {limit}"
        );
    }
}

/// The zero bytes a client may fill when it signs `data` with `refunds`: in them, the
/// signatures, and the six bytes of `at` and of the deadline.
fn fillable(data: &Bytes, refunds: &[U256]) -> u64 {
    let (message, authorization) = ExecuteMessage::from_authorized_data(data).unwrap();
    let zeros = |b: &[u8]| b.iter().filter(|&&b| b == 0).count() as u64;
    let signatures = authorization.signatures.iter().map(|s| zeros(&s.signature));
    let refunds = refunds.iter().map(|r| zeros(&r.to_be_bytes::<32>()));
    let stamps = [message.at, message.deadline].map(|t| zeros(&t.to_be_bytes::<6>()));
    signatures.sum::<u64>() + refunds.sum::<u64>() + stamps.iter().sum::<u64>()
}

/// An existing factory wallet relays as the create path does. A release outside the
/// supported set is refused, for it and on the create path, created or not. Its release
/// changed while queued, it keeps its id and fails at its send. Its code replaced by one
/// that skips the refund after its send is broadcast, it costs the relayer only that
/// send's gas.
#[tokio::test]
async fn an_existing_wallet_relays_and_a_skipped_refund_costs_only_gas() {
    let o = Onboarding::new(true).await;
    o.land(&o.sign(&o.onboarding()).await, o.value).await;
    let refund = o.repay(U256::from(CLAIMED / 100));
    let b = Batch(vec![refund], U256::ZERO, vec![&o.k], None);
    let q = o.quote_with(&o.config(), &b).await;
    let data = o.authorize(&b, Some(q.call_gas)).await;
    let request = o.request_at(&data, q.gas_limit).await;
    assert_eq!(request.create, None);
    let factory = WalletFactory::new(o.stack.factory, &o.owner);
    let id_node = B256::repeat_byte(0xa2);
    let wallet = factory.walletAddress(o.stack.platform, id_node, U256::ZERO);
    let wallet = wallet.call().await.unwrap();
    mined!(factory.createWallet(o.stack.platform, id_node, U256::ZERO));
    let version = o.stack.release.version;
    let unsupported = Config {
        supported_releases: vec![version + 1],
        ..o.config()
    };
    // On the create path: a new wallet's release from `latest`.
    let (platform_id, new) = (o.stack.platform, B256::repeat_byte(0xa3));
    let new_wallet = factory.walletAddress(platform_id, new, U256::ZERO);
    let (new_wallet, salt) = (new_wallet.call().await.unwrap(), U256::ZERO);
    let path = |id_node| {
        Some(Create {
            platform_id,
            id_node,
            salt,
        })
    };
    for (wallet, create) in [(o.wallet, None), (wallet, None), (new_wallet, path(new))] {
        let request = Request {
            wallet,
            create,
            ..request.clone()
        };
        let refused = request.check(&o.relayer, &unsupported).await.err();
        let named = matches!(refused, Some(Refusal::UnsupportedRelease { version: v }) if v == version);
        assert!(named, "{wallet}: {refused:?}");
    }
    o.send(&request).await;

    // Queued, then moved to a release 2 outside the supported set: relayed again it
    // keeps its id, and its send fails unsent.
    let data = o.authorize(&b, Some(q.call_gas)).await;
    let queued = o.request_at(&data, q.gas_limit).await;
    let r = o.start(&db("moved")).await.unwrap();
    let id = r.relay(&queued).await.unwrap();
    let (s, artifacts) = (o.stack, Artifacts::embedded());
    let deploy = async |name: &str, args: Vec<u8>| {
        let code = artifacts.bytecode(name).unwrap();
        deploy_with_ctor(&o.owner, &code, &args, "", None)
            .await
            .unwrap()
    };
    let (admin, rules) = (
        deploy("IdentityAdmin", vec![]).await,
        deploy("SessionRules", vec![]).await,
    );
    let args = (s.factory, s.wallet_registry, 2u64, admin, rules).abi_encode_params();
    let release = deploy("WebWallet", args).await;
    mined!(factory.publish(release));
    // The created wallet on the create path: its own release, not `latest`.
    let created = Request {
        wallet,
        create: path(id_node),
        ..request.clone()
    };
    let refused = created.check(&o.relayer, &unsupported).await.err();
    let own = matches!(refused, Some(Refusal::UnsupportedRelease { version: 1 }));
    assert!(own, "{refused:?}");
    let moved = U256::from(2) << 160 | U256::from_be_slice(release.as_slice());
    let slot = o.owner.get_storage_at(o.wallet, RELEASE_SLOT.into()).await;
    let at = |word: U256| {
        o.owner
            .anvil_set_storage_at(o.wallet, RELEASE_SLOT.into(), word.into())
    };
    let slot = slot.unwrap();
    at(moved).await.unwrap();
    assert_eq!(r.relay(&queued).await.unwrap(), id, "a new id");
    r.tick().await.unwrap();
    let report = r.status(id).await.unwrap();
    let gated = matches!(
        report.status,
        Status::Failed(Refusal::UnsupportedRelease { version: 2 })
    );
    assert!(gated && report.tx_hashes.is_empty(), "{:?}", report.status);
    at(slot).await.unwrap();

    // The same batch at the wallet's next nonce.
    let data = o.authorize(&b, Some(q.call_gas)).await;
    let r = o.start(&db("skipped")).await.unwrap();
    let id = r
        .relay(&o.request_at(&data, q.gas_limit).await)
        .await
        .unwrap();
    let (before, held, left) = (
        o.owner.get_balance(o.relayer_address).await.unwrap(),
        o.owner.get_balance(o.wallet).await.unwrap(),
        r.budget_left().unwrap(),
    );
    o.owner.anvil_set_auto_mine(false).await.unwrap();
    r.tick().await.unwrap();
    let sent = matches!(r.status(id).await.unwrap().status, Status::Sent);
    assert!(sent, "not broadcast before the code changed");
    o.set_code(o.wallet, "00").await;
    let report = o.settle(&r, id, included).await; // lands, without the refund
    let tx = o.tx(report.tx_hashes[0]).await;
    assert_eq!(tx.inner.value(), U256::ZERO, "value fronted");
    let receipt = o.owner.get_transaction_receipt(report.tx_hashes[0]).await;
    let receipt = receipt.unwrap().unwrap();
    let gas = U256::from(receipt.gas_used) * U256::from(receipt.effective_gas_price);
    let after = o.owner.get_balance(o.relayer_address).await.unwrap();
    assert_eq!(
        before - after,
        gas,
        "the relayer paid more than the send's gas"
    );
    r.tick().await.unwrap();
    assert_eq!(left - r.budget_left().unwrap(), gas, "not charged its gas");
    assert_eq!(
        o.owner.get_balance(o.wallet).await.unwrap(),
        held,
        "the refund was paid"
    );
}

#[tokio::test]
#[ignore = "forks https://sepolia.base.org"]
async fn the_quote_bounds_the_l1_fee_on_a_base_sepolia_fork() {
    let fork = Anvil::new()
        .fork("https://sepolia.base.org")
        .chain_id(84_532)
        .args(["--network", "optimism"]); // mines the fork's extraData
    let o = Onboarding::on(fork, true).await;
    let op_stack = Config {
        l1_fee: L1Fee::OpStack,
        fee_params: FeeParams::Header,
        ..o.config()
    };
    let b = Batch(o.onboarding(), o.value, vec![&o.k], None);
    let q = o.quote_with(&op_stack, &b).await;
    assert!(q.l1_fee_bound > U256::ZERO);
    assert!(q.min_refund > U256::from(q.gas_limit) * U256::from(q.max_fee_per_gas));
}
