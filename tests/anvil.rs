//! S1 on anvil: K signs link, bind, claim to the wallet and a refund; the relayer checks
//! and sends `createAndExecute`, or refuses before sending. Needs `anvil` on `PATH`.

include!("common/onboarding.rs");

use libid_contracts::gas::CALL_OVERHEAD;

type Case = (&'static str, Request, fn(&Refusal) -> bool);

/// The fixture, without the relayer: the batch lands when sent directly.
#[tokio::test]
async fn the_onboarding_batch_lands_when_sent_directly() {
    let o = Onboarding::new(true).await;
    let data = o.sign(&o.onboarding()).await;
    let factory = WalletFactory::new(o.stack.factory, &o.relayer);
    let (c, value) = (o.create, o.value);
    let send =
        factory.createAndExecute(c.platform_id, c.id_node, c.salt, AUTHORIZED_MODE, data);
    mined!(send.value(value).gas(GAS_LIMIT));
    o.assert_onboarded(o.refund()).await;
}

/// The acceptance path: checked, sent as `createAndExecute` with the signed
/// data unchanged, at most `R - value` in L2 gas, and the relayer not out of pocket.
#[tokio::test]
async fn the_relayer_checks_and_lands_the_onboarding() {
    let o = Onboarding::new(true).await;
    let data = o.sign(&o.onboarding()).await;
    let before = o.owner.get_balance(o.relayer_address).await.unwrap();
    let checked = check(&o.relayer, &o.config(), &o.request(data.clone()))
        .await
        .unwrap();
    let receipt = chain::send(&o.relayer, checked).await.unwrap();
    assert_eq!(receipt.to, Some(o.stack.factory));
    o.assert_onboarded(o.refund()).await;
    assert!(
        o.owner.get_balance(o.relayer_address).await.unwrap() >= before,
        "the relayer paid"
    );

    let tx = o
        .owner
        .get_transaction_by_hash(receipt.transaction_hash)
        .await
        .unwrap()
        .unwrap();
    let sent = WalletFactory::createAndExecuteCall::abi_decode(tx.inner.input()).unwrap();
    let c = o.create;
    assert_eq!(
        (sent.platformId, sent.idNode, sent.salt),
        (c.platform_id, c.id_node, c.salt)
    );
    assert_eq!((sent.mode, sent.data), (AUTHORIZED_MODE, data), "I7, D4");
    assert_eq!(tx.inner.value(), o.value);
    // D8: `ceil(1.25 × gasUsed) + (callGas + CALL_OVERHEAD) × (64/63)²` on the create path.
    let reserve = ((CALL_GAS + CALL_OVERHEAD) * 64 * 64).div_ceil(63 * 63);
    let d8 = (receipt.gas_used * 5).div_ceil(4) + reserve;
    assert!(
        tx.inner.gas_limit().abs_diff(d8) <= 2,
        "sent at {}, D8 {d8}",
        tx.inner.gas_limit()
    );
    let worst = U256::from(tx.inner.gas_limit()) * U256::from(tx.inner.max_fee_per_gas());
    assert!(
        worst <= o.refund() - o.value,
        "I1: {worst} exceeds R - value"
    );
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
    let account = Address::repeat_byte(0x77); // delegated to the forwarder, release 1
    let delegation = [&[0xef, 0x01, 0x00][..], o.stack.forwarder.as_slice()].concat();
    o.owner
        .anvil_set_code(account, delegation.into())
        .await
        .unwrap();
    let release: U256 = U256::from_be_slice(o.stack.release.implementation.as_slice())
        | U256::from(o.stack.release.version) << 160;
    let slot = RELEASE_SLOT.into();
    o.owner
        .anvil_set_storage_at(account, slot, B256::from(release))
        .await
        .unwrap();
    // Nothing is mined below, so the check reads this fee and this time.
    let (r, now) = (o.refund(), o.now().await);
    let fees = o.relayer.estimate_eip1559_fees().await.unwrap();
    let bound = o.value + U256::from(GAS_LIMIT) * U256::from(fees.max_fee_per_gas);
    let good = o.sign(&o.onboarding()).await;
    let calldata = encode_executions(&o.onboarding());
    let missing = |r: &Refusal| matches!(r, Refusal::RefundMissing);
    let low = |r: &Refusal| matches!(r, Refusal::RefundTooLow { .. });
    let noncanonical = |r: &Refusal| matches!(r, Refusal::NoncanonicalData);
    let expired = |r: &Refusal| matches!(r, Refusal::Expired);
    let unknown = |r: &Refusal| matches!(r, Refusal::UnknownWallet);
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
    // Reverts once the timestamp passes now + window - 1: the window's end is simulated.
    let clock = Address::repeat_byte(0x7e);
    let code = format!("67{:016x}4211600f57005b5f80fd", now + window - 1);
    let code = alloy::hex::decode(code).unwrap();
    o.owner.anvil_set_code(clock, code.into()).await.unwrap();
    let mut late = o.onboarding();
    late.insert(0, call(clock, U256::ZERO, vec![]));

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
        ("create for another wallet", another, unknown),
        ("fake genesisIdentity", impostor, unknown),
        ("account with value", on(account, good.clone()), unknown),
        ("account, no value", on(account, sign_at(U256::ZERO, now, now + 3600)), failed_call),
        ("factory wallet", on(existing, good.clone()), wrong_bind),
    ];

    let nonce = o.relayer_nonce().await;
    for (name, request, expected) in cases {
        let refused = check(&o.relayer, &o.config(), &request).await.err();
        assert!(
            refused.as_ref().is_some_and(expected),
            "{name}: {refused:?}"
        );
    }
    assert_eq!(o.relayer_nonce().await, nonce, "the relayer sent");
    assert!(
        o.owner.get_code_at(o.wallet).await.unwrap().is_empty(),
        "a wallet was created"
    );
    let refused = check(&o.relayer, &o.config(), &below_bound).await.err();
    let Some(Refusal::RefundTooLow { refund, required }) = refused else {
        panic!("{refused:?}")
    };
    assert_eq!((refund, required), (bound - U256::ONE, bound));
    // The good request still passes: every refusal above is the change's.
    let at_bound = o.request_for(&o.batch(o.repay(bound))).await;
    let at_window = o.request(sign_at(o.value, now, now + window));
    for request in [o.request(good.clone()), at_bound, at_window] {
        check(&o.relayer, &o.config(), &request).await.unwrap();
    }
    let op_stack = Config {
        l1_fee: L1Fee::OpStack,
        ..o.config()
    };
    // An oracle whose `getL1FeeUpperBound` is 1 ether: the refund must cover 1.3 ether more.
    let oracle = "0x420000000000000000000000000000000000000F"
        .parse()
        .unwrap();
    let code = alloy::hex::decode("670de0b6b3a76400005f5260205ff3").unwrap();
    o.owner.anvil_set_code(oracle, code.into()).await.unwrap();
    let refused = check(&o.relayer, &op_stack, &o.request(good.clone()))
        .await
        .err();
    let Some(Refusal::RefundTooLow { required, .. }) = refused else {
        panic!("{refused:?}")
    };
    assert_eq!(required, bound + U256::from(CLAIMED * 13 / 10));
    // A node error that is not a revert is not reported as one.
    o.owner
        .anvil_set_balance(o.relayer_address, U256::ZERO)
        .await
        .unwrap();
    let refused = check(&o.relayer, &o.config(), &o.request(good)).await.err();
    assert!(matches!(refused, Some(Refusal::Node(_))), "{refused:?}");
}

/// D2: a step already done fails as a decoded revert. With nothing escrowed
/// the claim reverts `NothingHeld`, which the check names before sending.
#[tokio::test]
async fn an_empty_escrow_is_refused_as_nothing_held() {
    let o = Onboarding::new(false).await;
    let data = o.sign(&o.onboarding()).await;
    let refused = check(&o.relayer, &o.config(), &o.request(data)).await.err();
    let Some(Refusal::Reverted { error: Some(e), .. }) = refused else {
        panic!("{refused:?}")
    };
    assert_eq!(
        (e.name(), e.contracts()),
        ("NothingHeld", vec!["HandleEscrow"])
    );
}

/// A send mined after its deadline is `Reverted`; one never mined times out.
#[tokio::test]
async fn a_send_that_does_not_land_is_an_error() {
    let o = Onboarding::new(true).await;
    let config = Config {
        inclusion_window: Duration::from_secs(1),
        ..o.config()
    };
    let request = o.request_for(&o.onboarding()).await;
    let checked = check(&o.relayer, &config, &request).await.unwrap();
    let deadline = o.now().await + 3600;
    o.owner
        .anvil_set_next_block_timestamp(deadline + 1)
        .await
        .unwrap();
    let sent = chain::send(&o.relayer, checked.clone()).await;
    let Err(chain::Error::Reverted { receipt }) = sent else {
        panic!("{sent:?}")
    };
    assert!(!receipt.status());
    o.owner.anvil_set_auto_mine(false).await.unwrap();
    let sent = chain::send(&o.relayer, checked).await;
    let Err(chain::Error::Timeout { hash }) = sent else {
        panic!("{sent:?}")
    };
    let pending = o.owner.get_transaction_by_hash(hash).await.unwrap();
    assert!(pending.is_some_and(|tx| tx.block_number.is_none()));
}
