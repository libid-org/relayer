//! Onboarding on anvil: K signs link, bind, claim to the wallet and a refund; the relayer
//! checks and sends `createAndExecute`, or refuses before sending. Needs `anvil` on `PATH`.

#![allow(dead_code)]

include!("common/onboarding.rs");

use libid_contracts::wallet_target::Release;
use relayer::quote::Draft;

type Case = (&'static str, Request, fn(&Refusal) -> bool);

/// The fixture, without the relayer: the batch lands when sent directly.
#[tokio::test]
async fn the_onboarding_batch_lands_when_sent_directly() {
    let o = Onboarding::new(true).await;
    o.land(&o.sign(&o.onboarding()).await, o.value).await;
    o.assert_onboarded(o.refund()).await;
}

/// The acceptance path: checked, sent as `createAndExecute` with the signed
/// data unchanged, at most `R - value` in L2 gas, and the relayer not out of pocket.
#[tokio::test]
async fn the_relayer_checks_and_lands_the_onboarding() {
    let o = Onboarding::new(true).await;
    let data = o.sign(&o.onboarding()).await;
    let before = o.owner.get_balance(o.relayer_address).await.unwrap();
    let receipt = o.send(&o.request(data.clone())).await;
    assert_eq!(receipt.to, Some(o.stack.factory));
    o.assert_onboarded(o.refund()).await;
    assert!(
        o.owner.get_balance(o.relayer_address).await.unwrap() >= before,
        "the relayer paid"
    );

    let tx = o.tx(receipt.transaction_hash).await;
    let sent = WalletFactory::createAndExecuteCall::abi_decode(tx.inner.input()).unwrap();
    let c = o.create;
    assert_eq!(
        (sent.platformId, sent.idNode, sent.salt),
        (c.platform_id, c.id_node, c.salt)
    );
    assert_eq!(
        (sent.mode, sent.data),
        (AUTHORIZED_MODE, data),
        "the signed data"
    );
    assert_eq!(tx.inner.value(), o.value);
    // Sent at `ceil(1.25 × gasUsed)` plus the create path's reserve.
    let limit = (receipt.gas_used * 5).div_ceil(4) + reserve(CALL_GAS, true);
    let gas_limit = tx.inner.gas_limit();
    assert!(
        gas_limit.abs_diff(limit) <= 2,
        "sent at {gas_limit}, expected {limit}"
    );
    let worst = U256::from(gas_limit) * U256::from(tx.inner.max_fee_per_gas());
    assert!(worst <= o.refund() - o.value, "{worst} exceeds R - value");
}

/// Each case changes one thing of the passing request and is refused with
/// its own reason; nothing is sent and no wallet is created.
#[tokio::test]
async fn refuses_before_sending() {
    let o = Onboarding::new(true).await;
    let factory = WalletFactory::new(o.stack.factory, &o.owner);
    let (platform, id_node) = (o.stack.platform, B256::repeat_byte(0xa2));
    let existing = factory.walletAddress(platform, id_node, U256::ZERO);
    let existing = existing.call().await.unwrap(); // a factory wallet K has no session in
    mined!(factory.createWallet(platform, id_node, U256::ZERO));
    // An account at `byte` delegated to `to`, its release slot set to `release`.
    let delegated = async |byte, to: Address, release: Release| {
        let at = Address::repeat_byte(byte);
        let delegation = [&[0xef, 0x01, 0x00][..], to.as_slice()].concat();
        o.owner.anvil_set_code(at, delegation.into()).await.unwrap();
        let word = U256::from_be_slice(release.implementation.as_slice())
            | U256::from(release.version) << 160;
        let slot = RELEASE_SLOT.into();
        let set = o.owner.anvil_set_storage_at(at, slot, B256::from(word));
        set.await.unwrap();
        at
    };
    let account = delegated(0x77, o.stack.forwarder, o.stack.release).await;
    let other = Address::repeat_byte(0x66); // the forwarder's code, at another address
    let code = o.owner.get_code_at(o.stack.forwarder).await.unwrap();
    o.owner.anvil_set_code(other, code).await.unwrap();
    let other = delegated(0x78, other, o.stack.release).await;
    let unpublished = Release {
        implementation: Address::repeat_byte(0x55),
        ..o.stack.release
    };
    let planted = delegated(0x79, o.stack.forwarder, unpublished).await;
    // Nothing is mined below, so the check reads this fee and this time.
    let (r, now) = (o.refund(), o.now().await);
    // The relay-time fee floor: the base fee grown 12.5% a block over the window's 5.
    let latest = o.owner.get_block_by_number(Default::default()).await;
    let base = latest.unwrap().unwrap().header.base_fee_per_gas.unwrap();
    let floor = (0..5).fold(u128::from(base), |f, _| (f * 9).div_ceil(8));
    let bound = o.value + U256::from(GAS_LIMIT) * U256::from(floor);
    let good = o.sign(&o.onboarding()).await;
    let calldata = encode_executions(&o.onboarding());
    let missing = |r: &Refusal| matches!(r, Refusal::RefundMissing);
    let low = |r: &Refusal| matches!(r, Refusal::RefundTooLow { .. });
    let noncanonical = |r: &Refusal| matches!(r, Refusal::NoncanonicalData);
    let expired = |r: &Refusal| matches!(r, Refusal::Expired);
    let unknown = |r: &Refusal| matches!(r, Refusal::UnknownWallet);
    let value_not_allowed = |r: &Refusal| matches!(r, Refusal::ValueNotAllowed);
    // The batch ran on the target: its link or bind failed, not the check.
    let failed_call = |r: &Refusal| reverted(r, "FailedCall");
    let wrong_bind = |r: &Refusal| reverted(r, "WrongBindValue");

    let nested = IWebWallet::executeCall {
        mode: AUTHORIZED_MODE,
        executionCalldata: encode_executions(&[o.repay(r)]),
    };
    let nested = call(o.wallet, U256::ZERO, nested.abi_encode());
    let mut moved = good.to_vec(); // a pad word after the head, both offsets moved past it
    moved.splice(160..160, [0; 32]);
    for at in [0..32, 128..160] {
        let offset = U256::from_be_slice(&moved[at.clone()]) + U256::from(32);
        moved[at].copy_from_slice(&offset.to_be_bytes::<32>());
    }
    let mut trailing = good.to_vec();
    trailing.extend([0; 32]);
    let mut padded = calldata.to_vec();
    padded.extend([0; 32]); // signed, but the wallet would refuse it NoncanonicalBatch
    let padded = o.sign_until(padded.into(), o.value, now, now + 3600);
    let another = Request {
        wallet: Address::repeat_byte(0x0f), // the create path names another wallet
        ..o.request(good.clone())
    };
    let impostor = Request {
        wallet: o.impostor().await,
        create: None,
        ..o.request(good.clone())
    };
    let elsewhere = call(o.stack.escrow, r, vec![]);
    let with_data = call(o.relayer_address, r, vec![0]);
    let bare = o.repay(o.value); // repays the fronted value, nothing for gas
    let sign_at =
        |value, at, deadline| o.sign_until(calldata.clone(), value, at, deadline);
    let on = |wallet, data| Request {
        wallet,
        create: None,
        ..o.request(data)
    };
    let below_bound = o.request_for(&o.batch(o.repay(bound - U256::ONE))).await;
    let window = o.config().inclusion_window.as_secs();
    // Pushes now + window - 1 and the timestamp, for a comparison.
    let end = format!("67{:016x}42", now + window - 1);
    // The onboarding after a call to `code` deployed at `byte`'s address.
    let first_call = async |byte, code: String| {
        let at = Address::repeat_byte(byte);
        o.set_code(at, &code).await;
        let mut calls = o.onboarding();
        calls.insert(0, call(at, U256::ZERO, vec![]));
        calls
    };
    // Reverts once the timestamp passes now + window - 1: the window's end is simulated.
    let late = first_call(0x7e, format!("{end}11600f57005b5f80fd")).await;
    // Reverts with under 2.5M gas left: it runs unbounded, not at the limit sent.
    let starved = first_call(0x9a, "622625a05a10600a57005b5f80fd".into()).await;
    // Both: reverts only by the window's end at the limit sent.
    let both = format!("{end}11622625a05a1016601657005b5f80fd");
    let late_starved = first_call(0x7f, both).await;
    // Over a node pool's 128 KiB once signed.
    let mut large = o.onboarding();
    large.insert(
        0,
        call(Address::repeat_byte(0x55), U256::ZERO, vec![0; 140_000]),
    );
    let large = o.request_for(&large).await;
    let too_large = |r: &Refusal| matches!(r, Refusal::TooLarge { max: 131_072 });

    #[rustfmt::skip]
    let cases: Vec<Case> = vec![
        ("no refund", o.request_for(&o.onboarding()[..3]).await, missing),
        ("refund to another", o.request_for(&o.batch(elsewhere)).await, missing),
        ("refund with data", o.request_for(&o.batch(with_data)).await, missing),
        ("nested refund", o.request_for(&o.batch(nested)).await, missing),
        ("refund = value", o.request_for(&o.batch(bare)).await, low),
        // Signed with `value` R, so R - value is 0.
        ("value = refund", o.request(sign_at(r, now, now + 3600)), low),
        ("value > refund", o.request(sign_at(r + U256::ONE, now, now + 3600)), low),
        ("refund 1 wei below the bound", below_bound.clone(), low),
        ("moved offsets", o.request(moved.into()), noncanonical),
        ("trailing word", o.request(trailing.into()), noncanonical),
        ("cut", o.request(good.slice(..good.len() - 1)), noncanonical),
        ("not abi", o.request(Bytes::from_static(&[0xde, 0xad])), noncanonical),
        ("noncanonical batch", o.request(padded), noncanonical),
        ("expired", o.request(sign_at(o.value, now - 10, now - 1)), expired),
        ("deadline = now", o.request(sign_at(o.value, now, now)), expired),
        ("deadline inside the window", o.request(sign_at(o.value, now, now + window - 1)), expired),
        ("fails by the window's end", o.request_for(&late).await, failed_call),
        ("fails at the limit sent", o.request_for(&starved).await, failed_call),
        ("fails at the limit sent by the window's end", o.request_for(&late_starved).await, failed_call),
        ("too large", large.clone(), too_large),
        ("create for another wallet", another, unknown),
        ("fake genesisIdentity", impostor, unknown),
        ("account with value", on(account, good.clone()), value_not_allowed),
        ("another forwarder", on(other, sign_at(U256::ZERO, now, now + 3600)), unknown),
        ("planted release", on(planted, sign_at(U256::ZERO, now, now + 3600)), unknown),
        ("account, no value", on(account, sign_at(U256::ZERO, now, now + 3600)), failed_call),
        ("factory wallet", on(existing, good.clone()), wrong_bind),
    ];

    let nonce = o.relayer_nonce().await;
    for (name, request, expected) in cases {
        let refused = request.check(&o.relayer, &o.config()).await.err();
        assert!(
            refused.as_ref().is_some_and(expected),
            "{name}: {refused:?}"
        );
    }
    assert_eq!(o.relayer_nonce().await, nonce, "the relayer sent");
    let draft = Draft {
        wallet: large.wallet,
        create: large.create,
        data: large.data,
        below_quorum: false,
    };
    let refused = draft.quote(&o.relayer, &o.config()).await.err();
    assert!(refused.as_ref().is_some_and(too_large), "{refused:?}");
    assert!(
        o.owner.get_code_at(o.wallet).await.unwrap().is_empty(),
        "a wallet was created"
    );
    let refused = below_bound.check(&o.relayer, &o.config()).await.err();
    let Some(Refusal::RefundTooLow { refund, required }) = refused else {
        panic!("{refused:?}")
    };
    assert_eq!((refund, required), (bound - U256::ONE, bound));
    // The good request still passes: every refusal above is the change's.
    let at_bound = o.request_for(&o.batch(o.repay(bound))).await;
    let at_window = o.request(sign_at(o.value, now, now + window));
    for request in [o.request(good.clone()), at_bound, at_window] {
        request.check(&o.relayer, &o.config()).await.unwrap();
    }
    let op_stack = Config {
        l1_fee: L1Fee::OpStack,
        ..o.config()
    };
    // An oracle whose `getL1FeeUpperBound` is 1 ether: the refund must cover 1.3 ether more.
    o.set_code(ORACLE, "670de0b6b3a76400005f5260205ff3").await;
    let refused = o
        .request(good.clone())
        .check(&o.relayer, &op_stack)
        .await
        .err();
    let Some(Refusal::RefundTooLow { required, .. }) = refused else {
        panic!("{refused:?}")
    };
    assert_eq!(required, bound + U256::from(CLAIMED * 13 / 10));
    // Funded for the gas at M × base, not at the cap from R: run at the cap sent.
    let m = U256::from(o.config().max_fee_percent);
    let at_bound = U256::from(GAS_LIMIT) * U256::from(base) * m / U256::from(100);
    o.set_balance(o.relayer_address, o.value + at_bound).await;
    o.request(good.clone())
        .check(&o.relayer, &o.config())
        .await
        .unwrap();
    // A node error that is not a revert is not reported as one.
    o.set_balance(o.relayer_address, U256::ZERO).await;
    let refused = o
        .request(good.clone())
        .check(&o.relayer, &o.config())
        .await
        .err();
    assert!(matches!(refused, Some(Refusal::Node(_))), "{refused:?}");
    // The value but not the gas: only the priced call fails, and not as a revert.
    o.set_balance(o.relayer_address, o.value).await;
    let refused = o.request(good).check(&o.relayer, &o.config()).await.err();
    assert!(matches!(refused, Some(Refusal::Node(_))), "{refused:?}");
}

/// A step already done fails as a decoded revert. With nothing escrowed
/// the claim reverts `NothingHeld`, which the check names before sending.
#[tokio::test]
async fn an_empty_escrow_is_refused_as_nothing_held() {
    let o = Onboarding::new(false).await;
    let data = o.sign(&o.onboarding()).await;
    let refused = o.request(data).check(&o.relayer, &o.config()).await.err();
    let Some(Refusal::Reverted { error: Some(e), .. }) = refused else {
        panic!("{refused:?}")
    };
    assert_eq!(
        (e.name(), e.contracts()),
        ("NothingHeld", vec!["HandleEscrow"])
    );
}
