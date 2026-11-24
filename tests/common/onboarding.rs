// The onboarding fixture, `include!`d by the anvil suites.

use std::{
    path::{
        Path,
        PathBuf,
    },
    sync::atomic::{
        AtomicUsize,
        Ordering,
    },
};

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
    network::TransactionBuilder,
    providers::{
        ext::AnvilApi,
        DynProvider,
        Provider,
        ProviderBuilder,
    },
    rpc::types::{
        Transaction,
        TransactionReceipt,
        TransactionRequest,
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
    gas::CALL_OVERHEAD,
    testing::*,
    wallet_target::RELEASE_SLOT,
    Artifacts,
};
use relayer::{
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
    relay::{
        Relayer,
        Report,
        Status,
    },
};

/// The OP Stack `GasPriceOracle` predeploy.
const ORACLE: Address =
    alloy::primitives::address!("420000000000000000000000000000000000000F");
const CLAIMED: u128 = 1_000_000_000_000_000_000;
/// The paid stub's Notary Fee: the link pays it, from the value the relayer fronts.
const LINK_FEE: u128 = CLAIMED / 1000;
const GAS_LIMIT: u64 = 3_000_000;
const CALL_GAS: u64 = 200_000;
/// What the refund pays for gas: 100 gwei a unit, far above anvil's base fee.
const GAS_BUDGET: u128 = GAS_LIMIT as u128 * 100_000_000_000;

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
        Self::on(Anvil::new(), escrowed).await
    }

    /// The fixture on the node `anvil` configures.
    async fn on(anvil: Anvil, escrowed: bool) -> Self {
        let anvil = anvil.spawn();
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
        let registry = WalletRegistry::new(stack.wallet_registry, &owner);
        let value = registry.quote(stack.platform, 2).call().await.unwrap();
        assert!(value > U256::ZERO, "the relayer fronts no value");
        let create = Create {
            platform_id: stack.platform,
            id_node,
            salt: U256::ZERO,
        };
        let mut o = Self {
            _anvil: anvil,
            owner,
            relayer,
            relayer_address,
            stack,
            chain_id,
            k: PrivateKeySigner::random(),
            create,
            wallet: Address::ZERO,
            handle,
            native,
            list: Bytes::new(),
            value,
        };
        o.wallet_at(U256::ZERO).await;
        o
    }

    /// A fresh K onboarding the identity's wallet at `salt`.
    async fn wallet_at(&mut self, salt: U256) {
        self.create.salt = salt;
        let (c, factory) = (self.create, WalletFactory::new(self.stack.factory, &self.owner));
        let wallet = factory.walletAddress(c.platform_id, c.id_node, salt);
        self.wallet = wallet.call().await.unwrap();
        self.k = PrivateKeySigner::random();
        let (chain_id, wallet) = (self.chain_id, self.wallet);
        let link = transaction_data(chain_id, wallet, wallet, &key_of(&self.k), &admin());
        let operations = vec![
            Operation::bind(wallet),
            Operation {
                domain: LINK_IDENTITY_DOMAIN,
                data: link,
            },
        ];
        self.list = stub_payload(&StubPayload {
            ceremonyVersion: 1,
            operationDomain: OPERATIONS_DOMAIN,
            authorizationNonce: B256::repeat_byte(1),
            transactionData: operations.abi_encode().into(),
        });
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

    /// `None` once the wallet exists: then the relayer calls `execute` on it.
    async fn path(&self) -> Option<Create> {
        let code = self.owner.get_code_at(self.wallet).await.unwrap();
        code.is_empty().then_some(self.create)
    }

    /// The relayer's call of `data` with `value`, sent without the relayer's check.
    async fn outer(&self, data: &Bytes, value: U256) -> TransactionRequest {
        let (c, mode, data) = (self.create, AUTHORIZED_MODE, data.clone());
        let tx = match self.path().await {
            Some(_) => WalletFactory::new(self.stack.factory, &self.relayer)
                .createAndExecute(c.platform_id, c.id_node, c.salt, mode, data)
                .into_transaction_request(),
            None => IWebWallet::new(self.wallet, &self.relayer)
                .execute(mode, data)
                .into_transaction_request(),
        };
        tx.with_from(self.relayer_address)
            .with_value(value)
            .with_gas_limit(10_000_000)
    }

    /// `data` sent directly, and mined successfully.
    async fn land(&self, data: &Bytes, value: U256) {
        let sent = self.relayer.send_transaction(self.outer(data, value).await);
        assert!(sent.await.unwrap().get_receipt().await.unwrap().status());
    }

    async fn start(&self, db: &Path) -> Result<Relayer<DynProvider>, String> {
        let started = Relayer::start(self.relayer.clone(), self.config(), db).await;
        started.map_err(|e| e.to_string())
    }

    /// Ticks and mines until `id`'s status is `done`.
    async fn settle<P: Provider>(
        &self,
        r: &Relayer<P>,
        id: B256,
        done: fn(&Status) -> bool,
    ) -> Report {
        for _ in 0..50 {
            r.tick().await.unwrap();
            self.owner.evm_mine(None).await.unwrap();
            let report = r.status(id).await.unwrap();
            if done(&report.status) {
                return report;
            }
        }
        panic!("unsettled: {:?}", r.status(id).await.unwrap().status);
    }

    /// `request` relayed by a relayer of its own and settled; its successful receipt.
    async fn send(&self, request: &Request) -> TransactionReceipt {
        let r = self.start(&db("send")).await.unwrap();
        let id = r.relay(request).await.unwrap();
        let hash = self.settle(&r, id, included).await.tx_hashes[0];
        let receipt = self.owner.get_transaction_receipt(hash).await;
        let receipt = receipt.unwrap().unwrap();
        assert!(receipt.status(), "reverted");
        receipt
    }

    async fn tx(&self, hash: B256) -> Transaction {
        let tx = self.owner.get_transaction_by_hash(hash).await;
        tx.unwrap().unwrap()
    }

    /// Deploys `hex`, runtime code, at `at`.
    async fn set_code(&self, at: Address, hex: &str) {
        let code = alloy::hex::decode(hex).unwrap();
        self.owner.anvil_set_code(at, code.into()).await.unwrap();
    }

    async fn set_balance(&self, at: Address, balance: U256) {
        self.owner.anvil_set_balance(at, balance).await.unwrap();
    }

    async fn relayer_nonce(&self) -> u64 {
        let nonce = self.relayer.get_transaction_count(self.relayer_address);
        nonce.await.unwrap()
    }

    /// A contract answering `genesisIdentity()` with the wallet's genesis identity.
    async fn impostor(&self) -> Address {
        let fake = Address::repeat_byte(0xfa);
        let (platform, id) = (self.create.platform_id, self.create.id_node);
        let code = format!("7f{platform:x}6000527f{id:x}60205260606000f3");
        self.set_code(fake, &code).await;
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

/// A fresh database path, unique in this run.
fn db(name: &str) -> PathBuf {
    static RUN: AtomicUsize = AtomicUsize::new(0);
    let n = RUN.fetch_add(1, Ordering::Relaxed);
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let dir = dir.join(format!("{name}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("relayer.db")
}

/// Included, not reverted.
fn included(s: &Status) -> bool {
    matches!(s, Status::Included { reverted: false, .. })
}

/// The relay-time reserve `(callGas + CALL_OVERHEAD) × (64/63)^d`: `d` is 4 on the
/// create path and 2 otherwise.
fn reserve(call_gas: u64, create: bool) -> u64 {
    let d = if create { 4 } else { 2 };
    ((call_gas + CALL_OVERHEAD) * 64u64.pow(d)).div_ceil(63u64.pow(d))
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
