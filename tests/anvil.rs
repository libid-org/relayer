//! S1 on anvil: K signs link, bind, claim to the wallet and a refund; the relayer checks
//! and sends `createAndExecute`, or refuses before sending. Needs `anvil` on `PATH`.

use alloy::{
    consensus::Transaction as _,
    node_bindings::{
        Anvil,
        AnvilInstance,
    },
    primitives::{
        address,
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
    config::Config,
};

/// EIP-7528's native-ETH address, `HandleEscrow.NATIVE`.
const NATIVE: Address = address!("0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE");
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
        if escrowed {
            let escrow = HandleEscrow::new(stack.escrow, &depositor);
            let amount = U256::from(CLAIMED);
            mined!(escrow
                .deposit(stack.platform, handle, NATIVE, amount)
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
            HandleEscrow::claimCall::new((self.handle, vec![NATIVE], self.wallet));
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

    /// K signs `calls` and `value` for nonce 0, valid until `deadline`.
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
        let left = escrow.escrowed(self.handle, NATIVE).call().await.unwrap();
        assert_eq!(left, U256::ZERO, "left in escrow");
        let expected = U256::from(CLAIMED) - refund;
        assert_eq!(
            self.owner.get_balance(self.wallet).await.unwrap(),
            expected,
            "the wallet's funds"
        );
    }
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
    let data = o.sign(&o.batch(o.repay(o.refund()))).await;
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
    let data = o.sign(&o.batch(o.repay(o.refund()))).await;
    let before = o.owner.get_balance(o.relayer_address).await.unwrap();
    let checked = check(&o.relayer, &o.config(), &o.request(data.clone()))
        .await
        .unwrap();
    let receipt = chain::send(&o.relayer, checked).await.unwrap();
    assert!(receipt.status(), "reverted");
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
    let (r, now) = (o.refund(), o.now().await);
    let good = o.sign(&o.batch(o.repay(r))).await;

    let mut cases: Vec<Case> = vec![];
    let missing = |r: &Refusal| matches!(r, Refusal::RefundMissing);
    let no_refund = &o.batch(o.repay(r))[..3];
    cases.push(("no refund", o.request(o.sign(no_refund).await), missing));
    let elsewhere = call(o.stack.escrow, r, vec![]);
    cases.push((
        "refund to another",
        o.request(o.sign(&o.batch(elsewhere)).await),
        missing,
    ));
    let with_data = call(o.relayer_address, r, vec![0]);
    cases.push((
        "refund with data",
        o.request(o.sign(&o.batch(with_data)).await),
        missing,
    ));
    let inner = encode_executions(&[o.repay(r)]);
    let nested = IWebWallet::executeCall {
        mode: AUTHORIZED_MODE,
        executionCalldata: inner,
    };
    let nested = call(o.wallet, U256::ZERO, nested.abi_encode());
    cases.push((
        "nested refund",
        o.request(o.sign(&o.batch(nested)).await),
        missing,
    ));

    let low = |r: &Refusal| matches!(r, Refusal::RefundTooLow { .. });
    let bare = o.repay(o.value); // repays the fronted value, nothing for gas
    cases.push((
        "refund = value",
        o.request(o.sign(&o.batch(bare)).await),
        low,
    ));
    // Signed with `value` R, so R - value is 0.
    let data = o.sign_until(encode_executions(&o.batch(o.repay(r))), r, now, now + 3600);
    cases.push(("value = refund", o.request(data), low));
    let noncanonical = |r: &Refusal| matches!(r, Refusal::NoncanonicalData);
    let mut moved = good.to_vec(); // a pad word after the head, both offsets moved past it
    moved.splice(160..160, [0; 32]);
    for at in [0..32, 128..160] {
        let offset = U256::from_be_slice(&moved[at.clone()]) + U256::from(32);
        moved[at].copy_from_slice(&offset.to_be_bytes::<32>());
    }
    cases.push(("moved offsets", o.request(moved.into()), noncanonical));
    let mut trailing = good.to_vec();
    trailing.extend([0; 32]);
    cases.push(("trailing word", o.request(trailing.into()), noncanonical));
    cases.push(("cut", o.request(good.slice(..good.len() - 1)), noncanonical));
    cases.push((
        "not abi",
        o.request(Bytes::from_static(&[0xde, 0xad])),
        noncanonical,
    ));
    let mut inner = encode_executions(&o.batch(o.repay(r))).to_vec();
    inner.extend([0; 32]); // signed, but the wallet would refuse it NoncanonicalBatch
    let data = o.sign_until(inner.into(), o.value, now, now + 3600);
    cases.push(("noncanonical batch", o.request(data), noncanonical));

    let expired = o.sign_until(
        encode_executions(&o.batch(o.repay(r))),
        o.value,
        now - 10,
        now - 1,
    );
    cases.push(("expired", o.request(expired), |r| {
        matches!(r, Refusal::Expired)
    }));
    let unknown = |r: &Refusal| matches!(r, Refusal::UnknownWallet);
    let mut other = o.request(good.clone()); // the create path names another wallet
    other.wallet = Address::repeat_byte(0x0f);
    cases.push(("create for another wallet", other, unknown));
    // A contract answering `genesisIdentity()` with the wallet's genesis identity.
    let fake = Address::repeat_byte(0xfa);
    let (platform, id) = (o.create.platform_id, o.create.id_node);
    let code =
        alloy::hex::decode(format!("7f{platform:x}6000527f{id:x}60205260606000f3"))
            .unwrap();
    o.owner.anvil_set_code(fake, code.into()).await.unwrap();
    let g = IWebWallet::new(fake, &o.owner)
        .genesisIdentity()
        .call()
        .await
        .unwrap();
    assert_eq!((g.platformId, g.idNode, g.salt), (platform, id, U256::ZERO));
    let impostor = Request {
        wallet: fake,
        create: None,
        data: good.clone(),
        gas_limit: GAS_LIMIT,
    };
    cases.push(("fake genesisIdentity", impostor, unknown));

    let nonce = o
        .relayer
        .get_transaction_count(o.relayer_address)
        .await
        .unwrap();
    for (name, request, expected) in cases {
        let refused = check(&o.relayer, &o.config(), &request).await.err();
        assert!(
            refused.as_ref().is_some_and(expected),
            "{name}: {refused:?}"
        );
    }
    let after = o
        .relayer
        .get_transaction_count(o.relayer_address)
        .await
        .unwrap();
    assert_eq!(after, nonce, "the relayer sent");
    assert!(
        o.owner.get_code_at(o.wallet).await.unwrap().is_empty(),
        "a wallet was created"
    );
    // The good request still passes: every refusal above is the change's.
    check(&o.relayer, &o.config(), &o.request(good))
        .await
        .unwrap();
}

/// D2: a step already done fails as a decoded revert. With nothing escrowed
/// the claim reverts `NothingHeld`, which the check names before sending.
#[tokio::test]
async fn an_empty_escrow_is_refused_as_nothing_held() {
    let o = Onboarding::new(false).await;
    let data = o.sign(&o.batch(o.repay(o.refund()))).await;
    let refused = check(&o.relayer, &o.config(), &o.request(data)).await.err();
    let Some(Refusal::Reverted { error: Some(e), .. }) = refused else {
        panic!("{refused:?}")
    };
    assert_eq!(
        (e.name(), e.contracts()),
        ("NothingHeld", vec!["HandleEscrow"])
    );
}
