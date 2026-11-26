//! One onboarding through the compose stack, over HTTP: quoted, relayed, included.
//!
//! ```sh
//! docker compose up -d --wait --build
//! cargo test --test e2e -- --ignored --nocapture
//! ```
//!
//! Without the stack it skips with a note; under CI it fails.

use alloy::{
    primitives::{
        aliases::U48,
        Address,
        Bytes,
        B256,
        U256,
    },
    providers::{
        ext::AnvilApi,
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
        wallet::*,
    },
    operation_list::{
        Operation,
        OPERATIONS_DOMAIN,
    },
    platform_verifier::PlatformVerifier,
    testing::*,
};
use serde_json::{
    json,
    Value,
};

const API: &str = "http://127.0.0.1:8080";
const ANVIL: &str = "http://127.0.0.1:8545";
const GAS_LIMIT: u64 = 3_000_000;
const CALL_GAS: u64 = 200_000;
/// What the refund pays for gas: 100 gwei a unit, far above anvil's base fee.
const GAS_BUDGET: u64 = GAS_LIMIT * 100_000_000_000;

#[tokio::test]
#[ignore = "needs the compose stack"]
async fn an_onboarding_lands_through_the_stack() {
    let http = reqwest::Client::new();
    if let Err(e) = http.get(format!("{API}/health")).send().await {
        assert!(std::env::var_os("CI").is_none(), "no stack under CI: {e}");
        return println!("skipped: no relayer at {API}; docker compose up --wait");
    }
    let get = async |path: &str| -> Value {
        let response = http.get(format!("{API}{path}")).send().await.unwrap();
        response.json().await.unwrap()
    };
    let config = get("/v1/config").await;
    let address = |key: &str| config[key].as_str().unwrap().parse::<Address>().unwrap();
    let (factory, registry, relayer) =
        (address("factory"), address("registry"), address("relayer"));
    let chain = ProviderBuilder::new().connect_http(ANVIL.parse().unwrap());
    chain
        .client()
        .set_poll_interval(std::time::Duration::from_millis(100));
    let chain_id = chain.get_chain_id().await.unwrap();
    let anyone = chain.get_accounts().await.unwrap()[2];

    // A fresh identity each run, which the stack's stub, verifier 1, reports.
    let platform = PlatformVerifier::X.platform_id();
    let registry = WalletRegistry::new(registry, &chain);
    let verifiers = registry.proofVerifier().call().await.unwrap();
    let verifiers = CeremonyProofVerifier::new(verifiers, &chain);
    let stub = verifiers.verifierOf(platform, 1).call().await.unwrap();
    let random = || PrivateKeySigner::random().to_bytes();
    let (id_node, handle) = (random(), random());
    let stub = StubPlatformVerifier::new(stub, &chain);
    let set = stub
        .setNodes(id_node, handle)
        .from(anyone)
        .send()
        .await
        .unwrap()
        .get_receipt()
        .await
        .unwrap();
    assert!(set.status());
    let factory = WalletFactory::new(factory, &chain);
    let wallet = factory.walletAddress(platform, id_node, U256::ZERO);
    let wallet = wallet.call().await.unwrap();
    let release = factory.latest().call().await.unwrap().version;

    // Link K as admin, then repay the relayer from the wallet's own ETH.
    let k = PrivateKeySigner::random();
    let key: Bytes = k.address().to_vec().into();
    let admin = Policy {
        admin: true,
        ..Default::default()
    };
    let link = transaction_data(chain_id, wallet, wallet, &key, &admin);
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
        authorizationNonce: random(),
        transactionData: operations.abi_encode().into(),
    });
    let value = registry.quote(platform, 1).call().await.unwrap();
    let link = IWebWallet::linkIdentityCall::new((platform, 1, list, admin));
    let refund = value + U256::from(GAS_BUDGET);
    let calls = [
        Execution {
            target: wallet,
            value,
            callData: link.abi_encode().into(),
        },
        Execution {
            target: relayer,
            value: refund,
            callData: Bytes::new(),
        },
    ];
    let funds = U256::from(2 * GAS_BUDGET);
    chain.anvil_set_balance(wallet, funds).await.unwrap();

    let now = chain.get_block_by_number(Default::default()).await.unwrap();
    let now = now.unwrap().header.timestamp;
    // The latest deadline a relay accepts: a quote's `validUntil`, the queue's wait and
    // the window.
    let seconds = |key: &str| config[key].as_u64().unwrap();
    let longest = seconds("quoteValiditySeconds")
        + seconds("queueWaitSeconds")
        + seconds("inclusionWindowSeconds");
    let message = ExecuteMessage {
        execution_calldata: encode_executions(&calls),
        value,
        at: U48::from(now),
        nonce: U256::ZERO,
        deadline: U48::from(now + longest),
        call_gas: U256::from(CALL_GAS),
    };
    let digest = message.digest(chain_id, wallet, release, wallet_salt(B256::ZERO, 0));
    let signature = k.sign_hash_sync(&digest).unwrap();
    let data = message.authorized_data(ExecuteAuthorization {
        signatures: vec![SessionSignature {
            key: key.clone(),
            signature: signature.as_bytes().to_vec().into(),
        }],
        ..Default::default()
    });
    let create = json!({"platformId": platform, "idNode": id_node, "salt": "0"});
    let request =
        json!({"wallet": wallet, "create": create, "data": data, "gasLimit": GAS_LIMIT});

    let post = async |path: &str| {
        let url = format!("{API}{path}");
        let response = http.post(url).json(&request).send().await.unwrap();
        (
            response.status().as_u16(),
            response.json::<Value>().await.unwrap(),
        )
    };
    let (code, quote) = post("/v1/quote").await;
    assert_eq!(code, 200, "{quote}");
    let min_refund: U256 = quote["minRefund"].as_str().unwrap().parse().unwrap();
    assert!(min_refund <= refund - value, "{quote}");
    let (code, accepted) = post("/v1/relay").await;
    assert_eq!(code, 202, "{accepted}");
    let path = format!("/v1/relay/{}", accepted["id"].as_str().unwrap());
    let mut status = Value::Null;
    for _ in 0..120 {
        status = get(&path).await;
        if status["status"] == "included" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    assert_eq!(
        (&status["status"], &status["reverted"]),
        (&json!("included"), &json!(false)),
        "{status}"
    );
    let wallet = IWebWallet::new(wallet, &chain);
    assert!(
        wallet.sessionOf(key).call().await.unwrap().admin,
        "K is not admin"
    );
}
