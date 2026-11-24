//! S1 on anvil: K signs link, bind, claim to the wallet and a refund; the relayer checks
//! and sends `createAndExecute`, or refuses before sending. Needs `anvil` on `PATH`.

use std::time::Duration;

use alloy::{
    consensus::Transaction as _,
    node_bindings::{
        Anvil,
        AnvilInstance,
    },
    primitives::{
        aliases::U48,
        Address,
        Bytes,
        B256,
        U256,
    },
    providers::{
        ext::AnvilApi,
        DynProvider,
        Provider,
        ProviderBuilder,
    },
    signers::{
        local::PrivateKeySigner,
        SignerSync,
    },
    sol_types::{
        SolCall,
        SolValue,
    },
};
use libid_contracts::{
    bindings::{
        ceremony::CeremonyProofVerifier,
        escrow::HandleEscrow,
        identity::IdentityRegistry,
        wallet::*,
    },
    operation_list::{
        Operation,
        OPERATIONS_DOMAIN,
    },
    testing::*,
    wallet_target::RELEASE_SLOT,
    Artifacts,
};
use relayer::{
    chain,
    check::{
        check,
        Create,
        Refusal,
        Request,
    },
    config::{
        Config,
        L1Fee,
    },
};

const CLAIMED: u128 = 1_000_000_000_000_000_000;
/// The paid stub's Notary Fee: the link pays it, from the value the relayer fronts.
const LINK_FEE: u128 = CLAIMED / 1000;
const GAS_LIMIT: u64 = 3_000_000;
const CALL_GAS: u64 = 200_000;
/// What the refund pays for gas: 100 gwei a unit, far above anvil's base fee.
const GAS_BUDGET: u128 = GAS_LIMIT as u128 * 100_000_000_000;
type Case = (&'static str, Request, fn(&Refusal) -> bool);

macro_rules! mined {
    ($call:expr) => {
        assert!($call
            .send()
            .await
            .unwrap()
            .get_receipt()
            .await
            .unwrap()
            .status())
    };
}

/// A stack on a fresh anvil, a paid verifier as X's version 2, and K's
/// not-yet-created wallet, its handle escrowed for unless `escrowed` is false.
struct Onboarding {
    _anvil: AnvilInstance,
    owner: DynProvider,
    relayer: DynProvider,
    relayer_address: Address,
    stack: WalletStack,
    chain_id: u64,
    k: PrivateKeySigner,
    create: Create,
    wallet: Address,
    handle: B256,
    /// `HandleEscrow.NATIVE`, the escrowed token.
    native: Address,
    list: Bytes,
    /// `WalletRegistry.quote(X, 2)`: the link fee, which the batch's `value` fronts.
    value: U256,
}

impl Onboarding {
    async fn new(escrowed: bool) -> Self {
        let anvil = Anvil::new().spawn();
        let connect = |i: usize| {
            let key = PrivateKeySigner::from(anvil.keys()[i].clone());
            ProviderBuilder::new()
                .wallet(key)
                .connect_http(anvil.endpoint_url())
                .erased()
        };
        let (owner, relayer, depositor) = (connect(0), connect(1), connect(2));
        let relayer_address = anvil.addresses()[1];
        let chain_id = anvil.chain_id();
        let artifacts = Artifacts::embedded();
        let stack = deploy_wallet_stack(&owner, &artifacts, anvil.addresses()[0])
            .await
            .unwrap();
        let fee = U256::from(LINK_FEE);
        let paid =
            deploy_stub_platform_verifier(&owner, &artifacts, stack.platform, fee, None);
        let paid = paid.await.unwrap();
        let verifiers = CeremonyProofVerifier::new(stack.proof_verifier, &owner);
        mined!(verifiers.setVerifier(stack.platform, 2, paid));
        let (id_node, handle) = (B256::repeat_byte(0xa1), B256::repeat_byte(0xb2));
        mined!(StubPlatformVerifier::new(paid, &owner).setNodes(id_node, handle));
        let escrow = HandleEscrow::new(stack.escrow, &depositor);
        let native = escrow.NATIVE().call().await.unwrap();
        if escrowed {
            let amount = U256::from(CLAIMED);
            mined!(escrow
                .deposit(stack.platform, handle, native, amount)
                .value(amount));
        }
        let create = Create {
            platform_id: stack.platform,
            id_node,
            salt: U256::ZERO,
        };
        let factory = WalletFactory::new(stack.factory, &owner);
        let wallet = factory.walletAddress(stack.platform, id_node, U256::ZERO);
        let wallet = wallet.call().await.unwrap();
        let registry = WalletRegistry::new(stack.wallet_registry, &owner);
        let value = registry.quote(stack.platform, 2).call().await.unwrap();
        assert!(value > U256::ZERO, "the relayer fronts no value");
        let k = PrivateKeySigner::random();
        let link = transaction_data(chain_id, wallet, wallet, &key_of(&k), &admin());
        let operations = vec![
            Operation::bind(wallet),
            Operation {
                domain: LINK_IDENTITY_DOMAIN,
                data: link,
            },
        ];
        let list = stub_payload(&StubPayload {
            ceremonyVersion: 1,
            operationDomain: OPERATIONS_DOMAIN,
            authorizationNonce: B256::repeat_byte(1),
            transactionData: operations.abi_encode().into(),
        });
        Self {
            _anvil: anvil,
            owner,
            relayer,
            relayer_address,
            stack,
            chain_id,
            k,
            create,
            wallet,
            handle,
            native,
            list,
            value,
        }
    }

    fn config(&self) -> Config {
        Config::anvil(
            self.chain_id,
            self.stack.factory,
            self.stack.forwarder,
            self.relayer_address,
        )
    }

    /// Link, bind, claim to the wallet, then `refund`.
    fn batch(&self, refund: Execution) -> Vec<Execution> {
        let (platform, list) = (self.stack.platform, self.list.clone());
        let link =
            IWebWallet::linkIdentityCall::new((platform, 2, list.clone(), admin()));
        let bind = (platform, 2, list, U256::ZERO, Address::ZERO);
        let bind = IdentityRegistry::bindPrivatelyCall::new(bind);
        let claim =
            HandleEscrow::claimCall::new((self.handle, vec![self.native], self.wallet));
        vec![
            call(self.wallet, self.value, link.abi_encode()),
            call(self.stack.identities, U256::ZERO, bind.abi_encode()),
            call(self.stack.escrow, U256::ZERO, claim.abi_encode()),
            refund,
        ]
    }

    /// The refund a well-priced batch carries: the fronted value plus the gas budget.
    fn refund(&self) -> U256 {
        self.value + U256::from(GAS_BUDGET)
    }

    /// The batch that onboards: link, bind, claim, then the well-priced refund.
    fn onboarding(&self) -> Vec<Execution> {
        self.batch(self.repay(self.refund()))
    }

    fn repay(&self, amount: U256) -> Execution {
        call(self.relayer_address, amount, vec![])
    }

    async fn now(&self) -> u64 {
        self.owner
            .get_block_by_number(Default::default())
            .await
            .unwrap()
            .unwrap()
            .header
            .timestamp
    }

    /// K signs `execution_calldata` and `value`, nonce 0, from `at` until `deadline`.
    fn sign_until(
        &self,
        execution_calldata: Bytes,
        value: U256,
        at: u64,
        deadline: u64,
    ) -> Bytes {
        let message = ExecuteMessage {
            execution_calldata,
            value,
            at: U48::from(at),
            nonce: U256::ZERO,
            deadline: U48::from(deadline),
            call_gas: U256::from(CALL_GAS),
        };
        // A wallet without code signs under release 1's domain, salt `wallet_salt(0, 0)`.
        let salt = wallet_salt(B256::ZERO, 0);
        let digest =
            message.digest(self.chain_id, self.wallet, self.stack.release.version, salt);
        let signature = self.k.sign_hash_sync(&digest).unwrap();
        message.authorized_data(ExecuteAuthorization {
            signatures: vec![SessionSignature {
                key: key_of(&self.k),
                signature: signature.as_bytes().to_vec().into(),
            }],
            ..Default::default()
        })
    }

    async fn sign(&self, calls: &[Execution]) -> Bytes {
        let now = self.now().await;
        self.sign_until(encode_executions(calls), self.value, now, now + 3600)
    }

    fn request(&self, data: Bytes) -> Request {
        Request {
            wallet: self.wallet,
            create: Some(self.create),
            data,
            gas_limit: GAS_LIMIT,
        }
    }

    /// The create-path request for `calls`, signed by K.
    async fn request_for(&self, calls: &[Execution]) -> Request {
        self.request(self.sign(calls).await)
    }

    async fn relayer_nonce(&self) -> u64 {
        let nonce = self.relayer.get_transaction_count(self.relayer_address);
        nonce.await.unwrap()
    }

    /// A contract answering `genesisIdentity()` with the wallet's genesis identity.
    async fn impostor(&self) -> Address {
        let fake = Address::repeat_byte(0xfa);
        let (platform, id) = (self.create.platform_id, self.create.id_node);
        let code =
            alloy::hex::decode(format!("7f{platform:x}6000527f{id:x}60205260606000f3"))
                .unwrap();
        self.owner.anvil_set_code(fake, code.into()).await.unwrap();
        let g = IWebWallet::new(fake, &self.owner)
            .genesisIdentity()
            .call()
            .await
            .unwrap();
        assert_eq!((g.platformId, g.idNode, g.salt), (platform, id, U256::ZERO));
        fake
    }

    /// K is the wallet's admin session and the escrow paid the wallet.
    async fn assert_onboarded(&self, refund: U256) {
        let wallet = IWebWallet::new(self.wallet, &self.owner);
        assert!(wallet.hadSession().call().await.unwrap(), "no session");
        assert!(
            wallet
                .sessionOf(key_of(&self.k))
                .call()
                .await
                .unwrap()
                .admin,
            "K not admin"
        );
        let escrow = HandleEscrow::new(self.stack.escrow, &self.owner);
        let left = escrow
            .escrowed(self.handle, self.native)
            .call()
            .await
            .unwrap();
        assert_eq!(left, U256::ZERO, "left in escrow");
        let expected = U256::from(CLAIMED) - refund;
        assert_eq!(
            self.owner.get_balance(self.wallet).await.unwrap(),
            expected,
            "the wallet's funds"
        );
    }
}

fn reverted(r: &Refusal, name: &str) -> bool {
    matches!(r, Refusal::Reverted { error: Some(e), .. } if e.name() == name)
}

fn admin() -> Policy {
    Policy {
        admin: true,
        ..Default::default()
    }
}

fn key_of(k: &PrivateKeySigner) -> Bytes {
    k.address().to_vec().into()
}

fn call(target: Address, value: U256, data: Vec<u8>) -> Execution {
    Execution {
        target,
        value,
        callData: data.into(),
    }
}

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
    assert_eq!(
        tx.inner.gas_limit(),
        GAS_LIMIT,
        "the limit R was priced for"
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
    let refused = check(&o.relayer, &op_stack, &o.request(good.clone()))
        .await
        .err();
    assert!(matches!(refused, Some(Refusal::Node(_))), "{refused:?}");
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
