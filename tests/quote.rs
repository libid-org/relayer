//! S2 on anvil: the quote (D7) and the relay-time gas limit (D8). Needs `anvil` on `PATH`.

#![allow(dead_code, unused_imports)]

include!("common/onboarding.rs");

use alloy::{
    primitives::keccak256,
    rpc::types::simulate::{
        SimBlock,
        SimCallResult,
        SimulatePayload,
    },
    sol_types::SolError,
};
use libid_contracts::gas::CALL_OVERHEAD;
use relayer::{
    check::Checked,
    quote::{
        quote,
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
    /// `None` once the wallet exists: then the relayer calls `execute` on it.
    async fn path(&self) -> Option<Create> {
        let code = self.owner.get_code_at(self.wallet).await.unwrap();
        code.is_empty().then_some(self.create)
    }

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
        let (wallet, create) = (self.wallet, self.path().await);
        let data = self.authorize(b, None).await;
        let below_quorum = b.3.is_some();
        let draft = Draft {
            wallet,
            create,
            data,
            below_quorum,
        };
        quote(&self.relayer, config, &draft).await.unwrap()
    }

    async fn relay(&self, data: &Bytes, gas_limit: u64) -> Result<Checked, Refusal> {
        let (wallet, create, data) = (self.wallet, self.path().await, data.clone());
        check(
            &self.relayer,
            &self.config(),
            &Request {
                wallet,
                create,
                data,
                gas_limit,
            },
        )
        .await
    }

    /// This test's own `eth_simulateV1` of the outer call for `data`, or its send.
    async fn run(&self, data: &Bytes, value: U256, send: bool) -> SimCallResult {
        let (c, mode, data) = (self.create, AUTHORIZED_MODE, data.clone());
        let tx = match self.path().await {
            Some(_) => WalletFactory::new(self.stack.factory, &self.relayer)
                .createAndExecute(c.platform_id, c.id_node, c.salt, mode, data)
                .into_transaction_request(),
            None => IWebWallet::new(self.wallet, &self.relayer)
                .execute(mode, data)
                .into_transaction_request(),
        };
        let tx = tx
            .from(self.relayer_address)
            .value(value)
            .gas_limit(10_000_000);
        if send {
            let sent = self.relayer.send_transaction(tx.clone()).await.unwrap();
            assert!(sent.get_receipt().await.unwrap().status());
        }
        let block_state_calls = vec![SimBlock {
            calls: vec![tx],
            ..Default::default()
        }];
        let payload = SimulatePayload {
            block_state_calls,
            ..Default::default()
        };
        self.relayer
            .simulate(&payload)
            .await
            .unwrap()
            .remove(0)
            .calls
            .remove(0)
    }

    /// D8 by hand: `ceil(1.25 × gasUsed_s) + (callGas + CALL_OVERHEAD) × (64/63)^d`.
    async fn d8(&self, data: &Bytes, value: U256, call_gas: u64) -> u64 {
        let used = self.run(data, value, false).await;
        assert!(used.status, "{used:?}");
        (used.gas_used * 5).div_ceil(4) + reserve(call_gas, self.path().await.is_some())
    }

    /// Onboards, then adds K2, an admin session, and K3 and K4, below quorum, whose
    /// scope is a refund of up to 1 ether and `token.transfer`, each 1 ether a day.
    async fn sessions(&self) -> ([PrivateKeySigner; 3], Address, [B256; 2]) {
        self.run(&self.sign(&self.onboarding()).await, self.value, true)
            .await;
        let token = Address::repeat_byte(0x70); // returns true to any call
        let code = alloy::hex::decode("600160005260206000f3").unwrap();
        self.owner.anvil_set_code(token, code.into()).await.unwrap();
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
        let identity = wallet
            .sessionOf(key_of(&self.k))
            .call()
            .await
            .unwrap()
            .identity;
        let k = [(); 3].map(|_| PrivateKeySigner::random());
        let add = |k: &PrivateKeySigner, policy| {
            let add = IWebWallet::addSessionCall::new((identity, key_of(k), policy));
            call(self.wallet, U256::ZERO, add.abi_encode())
        };
        let below = Policy {
            rules,
            ..Default::default()
        };
        let b = Batch(
            vec![
                add(&k[0], admin()),
                add(&k[1], below.clone()),
                add(&k[2], below),
            ],
            U256::ZERO,
            vec![&self.k],
            None,
        );
        self.run(&self.authorize(&b, Some(CALL_GAS)).await, U256::ZERO, true)
            .await;
        self.owner.anvil_increase_time(2).await.unwrap(); // the sessions are active
        self.owner.anvil_mine(Some(1), None).await.unwrap();
        (k, token, leaves)
    }
}

/// `(callGas + CALL_OVERHEAD) × (64/63)^d`: d is 2 on the create path, 1 otherwise.
fn reserve(call_gas: u64, create: bool) -> u64 {
    let d = if create { 2 } else { 1 };
    ((call_gas + CALL_OVERHEAD) * 64u64.pow(d)).div_ceil(63u64.pow(d))
}

/// §9's first check: anvil's `eth_simulateV1` reports `gasUsed` and the revert data.
#[tokio::test]
async fn anvil_simulates_a_draft_to_execution_not_signed() {
    let o = Onboarding::new(true).await;
    let b = Batch(o.onboarding(), o.value, vec![&o.k], None);
    let sim = o.run(&o.authorize(&b, None).await, o.value, false).await;
    assert!(!sim.status && sim.gas_used > 21_000, "{sim:?}");
    assert_eq!(
        sim.return_data[..],
        IWebWallet::ExecutionNotSigned::SELECTOR
    );
}

/// The create path: a `callGas` signed above the quote is `GasLimitTooLow` with
/// D8's limit; the quoted one lands at D8's limit, and a last call that swallows a
/// failed inner call (`ReferralSplitter._deliver`) gets `callGas` and succeeds.
#[tokio::test]
async fn the_create_path_lands_at_the_d8_limit() {
    let o = Onboarding::new(true).await;
    let swallower = Address::repeat_byte(0x5a);
    let code = alloy::hex::decode(SWALLOWER).unwrap();
    o.owner
        .anvil_set_code(swallower, code.into())
        .await
        .unwrap();
    let mut calls = o.onboarding();
    calls.push(call(swallower, U256::ZERO, vec![]));
    let b = Batch(calls, o.value, vec![&o.k], None);
    let q = o.quote_with(&o.config(), &b).await;

    let above = o.authorize(&b, Some(2 * q.call_gas)).await;
    let expected = o.d8(&above, o.value, 2 * q.call_gas).await;
    for gas_limit in [q.gas_limit, expected - 3] {
        let refused = o.relay(&above, gas_limit).await.err();
        let Some(Refusal::GasLimitTooLow { required }) = refused else {
            panic!("{refused:?}")
        };
        assert!(
            required.abs_diff(expected) <= 2,
            "required {required}, D8 {expected}"
        );
    }
    o.relay(&above, expected + 2).await.unwrap();

    let data = o.authorize(&b, Some(q.call_gas)).await;
    let limit = o.d8(&data, o.value, q.call_gas).await;
    let checked = o.relay(&data, q.gas_limit).await.unwrap();
    let hash = chain::send(&o.relayer, checked)
        .await
        .unwrap()
        .transaction_hash;
    let sent = o
        .owner
        .get_transaction_by_hash(hash)
        .await
        .unwrap()
        .unwrap()
        .inner
        .gas_limit();
    assert!(sent.abs_diff(limit) <= 2, "sent at {sent}, D8 {limit}");
    let slot = |i: u64| o.owner.get_storage_at(swallower, U256::from(i));
    assert_eq!(
        slot(0).await.unwrap(),
        U256::ONE,
        "the swallowed call failed"
    );
    assert!(
        slot(1).await.unwrap() >= U256::from(q.call_gas),
        "it got less than callGas"
    );
}

/// An existing wallet: two admin signers at quorum are quoted within the 1.25 gas
/// margin of the gas they use. They, one signer, and below quorum a native refund's
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
    let used = o.run(&data, U256::ZERO, false).await.gas_used;
    let estimate = (q.gas_limit - reserve(q.call_gas, false)) * 4 / 5;
    assert!(
        4 * used <= 5 * estimate && 4 * estimate <= 5 * used,
        "{estimate} for {used}"
    );
    let checked = o.relay(&data, q.gas_limit).await.unwrap();
    chain::send(&o.relayer, checked).await.unwrap();

    let refund = CallProof {
        maxValue: U256::from(CLAIMED),
        proof: vec![leaves[1]],
    };
    let to_token = CallProof {
        maxValue: U256::ZERO,
        proof: vec![leaves[0]],
    };
    let transfer = transferCall::new((o.relayer_address, U256::ONE)).abi_encode();
    let calls = vec![call(token, U256::ZERO, transfer), o.repay(r)];
    let cases = [
        Batch(vec![o.repay(r)], U256::ZERO, vec![&o.k], None),
        Batch(
            vec![o.repay(r)],
            U256::ZERO,
            vec![&k3],
            Some(vec![refund.clone()]),
        ),
        Batch(calls, U256::ZERO, vec![&k4], Some(vec![to_token, refund])),
    ];
    for b in &cases {
        let q = o.quote_with(&o.config(), b).await;
        assert!(r >= q.min_refund, "the fixture underpays");
        let data = o.authorize(b, Some(q.call_gas)).await;
        let checked = o.relay(&data, q.gas_limit).await.unwrap();
        chain::send(&o.relayer, checked).await.unwrap();
    }
}
#[tokio::test]
#[ignore = "forks https://sepolia.base.org"]
async fn the_quote_bounds_the_l1_fee_on_a_base_sepolia_fork() {
    let fork = Anvil::new()
        .fork("https://sepolia.base.org")
        .chain_id(84_532);
    let o = Onboarding::on(fork, true).await;
    let op_stack = Config {
        l1_fee: L1Fee::OpStack,
        ..o.config()
    };
    let q = o
        .quote_with(&op_stack, &Batch(o.onboarding(), o.value, vec![&o.k], None))
        .await;
    assert!(q.l1_fee_bound > U256::ZERO);
    assert!(q.min_refund > U256::from(q.gas_limit) * U256::from(q.max_fee_per_gas));
}
