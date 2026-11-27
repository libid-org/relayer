//! `relayer run` and its configuration: the example file with its overrides, each
//! startup check's refusal, and the binary served from a file whose key and RPC URL
//! carry markers no output may. Needs `anvil` on `PATH`.

#![allow(dead_code, unused_imports)]

include!("common/onboarding.rs");

use std::{
    process::{
        Command,
        Stdio,
    },
    time::Duration,
};

use relayer::{
    config::FeeParams,
    settings::Settings,
    startup::check,
};
use serde_json::{
    json,
    Value,
};

const KEY_MARKER: &str = "5ec7e75ec7e7";
const URL_MARKER: &str = "a11ce5ec7e7";
const EXAMPLE: &str = include_str!("../relayer.example.toml");

/// `text` as `relayer.toml` in a fresh directory, and the file's path.
fn write(name: &str, text: &str) -> PathBuf {
    let path = db(name).with_file_name("relayer.toml");
    std::fs::write(&path, text).unwrap();
    path
}

/// The example, with the environment's overrides; an unknown key and a network file on
/// another chain refused.
#[test]
fn the_example_loads_with_its_overrides() {
    let load = |text: &str, env: &dyn Fn(&str) -> Option<String>| {
        Settings::load(&write("example", text), env).map_err(|e| e.to_string())
    };
    let port = |value: &'static str| {
        move |n: &str| (n != "RELAYER_RPC_URL").then(|| value.into())
    };
    let refused = load(EXAMPLE, &port("x")).err().unwrap();
    assert_eq!(refused, "RELAYER_PORT: invalid digit found in string");
    let s = load(EXAMPLE, &port("7")).unwrap();
    let url = "https://sepolia.base.org";
    assert_eq!(
        (s.rpc_url.unwrap(), s.signer.unwrap()),
        (url.into(), "7".into())
    );
    assert_eq!(
        (s.port, s.metrics_port, s.config.chain_id),
        (7, 9090, 84532)
    );
    assert_eq!(s.config.fee_params, FeeParams::Header);
    assert_eq!(s.config.global_budget.window, Duration::from_secs(3600));
    assert_eq!(s.config.loss_hold, Duration::from_secs(24 * 3600));
    assert!(s.db.ends_with("relayer.db") && s.db.is_absolute());
    assert_eq!(s.config.max_value, None);
    let unset = load(&EXAMPLE.replace("tick_timeout = \"30s\"", ""), &|_| None);
    assert_eq!(unset.unwrap().config.tick_timeout, Duration::from_secs(30));
    let waits = load(
        &EXAMPLE.replace("queue_wait = \"60s\"", "queue_wait = \"90s\""),
        &|_| None,
    );
    assert_eq!(waits.unwrap().config.queue_wait, Duration::from_secs(90));
    let unset = load(&EXAMPLE.replace("queue_wait = \"60s\"", ""), &|_| None);
    assert_eq!(unset.unwrap().config.queue_wait, Duration::from_secs(60));
    let capped = EXAMPLE.replace("# max_value", "max_value");
    let max = load(&capped, &|_| None).unwrap().config.max_value;
    assert_eq!(max, Some(U256::from(10u64.pow(16))));

    let limit = "concurrency_limit = 64";
    let unknown = EXAMPLE.replace(limit, &format!("{limit}\ncolour = 1"));
    let line = unknown.lines().position(|l| l == "colour = 1").unwrap() + 1;
    let e = load(&unknown, &|_| None).err().unwrap();
    let at = format!("line {line}, column 1: unknown field `colour`");
    assert!(e.contains(&at), "{e}");
    let other = write("network", "[network]\nchain_id = 1\nname = \"other\"\n");
    let file = format!(
        "chain_id = 84532\nnetwork_file = {:?}",
        other.display().to_string()
    );
    let e = load(&EXAMPLE.replace("chain_id = 84532", &file), &|_| None)
        .err()
        .unwrap();
    assert!(e.ends_with("name the same chain"), "{e}");
}

/// Each setting the relayer cannot run with is refused, naming it.
#[test]
fn settings_it_cannot_run_with_are_refused() {
    let env = |name: &'static str, value: &'static str| {
        move |n: &str| (n == name).then(|| value.to_owned())
    };
    #[rustfmt::skip]
    let rows: [(&str, &str, &str, &str); 15] = [
        ("poll_interval = \"1s\"", "poll_interval = \"0s\"", "", "poll_interval"),
        ("concurrency_limit = 64", "concurrency_limit = 0", "", "concurrency_limit"),
        ("request_timeout = \"30s\"", "request_timeout = \"0s\"", "", "request_timeout"),
        ("tick_timeout = \"30s\"", "tick_timeout = \"0s\"", "", "tick_timeout"),
        ("tip_percentile = 75.0", "tip_percentile = 49.9", "", "tip_percentile"),
        ("tip_percentile = 75.0", "tip_percentile = 90.1", "", "tip_percentile"),
        ("tip_percentile = 75.0", "tip_percentile = nan", "", "tip_percentile"),
        ("", "", "RELAYER_PORT", "RELAYER_PORT 9090 is [http] metrics_port"),
        ("", "", "RELAYER_SIGNER", "RELAYER_SIGNER is empty"),
        ("tip_percentile = 75.0", "tip_percentile = 90.0", "", ""),
        ("max_gas_limit = 8000000", "max_gas_limit = 16777217", "", "max_gas_limit"),
        ("max_gas_limit = 8000000", "max_gas_limit = 16777216", "", ""),
        ("l1_margin_percent = 130", "l1_margin_percent = 99", "", "l1_margin"),
        ("receipt_lag_blocks = 50", "receipt_lag_blocks = 0", "", "receipt_lag_blocks"),
        ("\"20000000000000000\", window = \"1h\"", "\"20000000000000000\", window = \"0s\"", "", "global_budget"),
    ];
    for (from, to, name, hint) in rows {
        let value = if name == "RELAYER_PORT" { "9090" } else { "" };
        assert!(EXAMPLE.contains(from), "{from}");
        let loaded =
            Settings::load(&write("bad", &EXAMPLE.replace(from, to)), env(name, value));
        match loaded {
            Ok(_) => assert_eq!(hint, "", "{to} {name} loaded"),
            Err(e) => assert!(!hint.is_empty() && e.to_string().contains(hint), "{e}"),
        }
    }
}

/// A configuration, code planted at an address, the hint refused with, and the hint
/// warned with; empty when none.
type Case<'a> = (Config, Option<(Address, &'a str)>, &'a str, &'a str);

/// Each check refuses what waiting does not cure with its fix, and warns of the rest;
/// the code planted for a case is restored.
#[tokio::test]
async fn each_startup_check_refuses_or_warns_with_its_hint() {
    let o = Onboarding::new(true).await;
    let (ok, at) = (o.config(), Address::repeat_byte(0x0c));
    let with = |change: &dyn Fn(&mut Config)| {
        let mut config = ok.clone();
        change(&mut config);
        config
    };
    let (one, revert, zero) = ("600160005260206000f3", "60006000fd", "60206000f3");
    let l1_block =
        alloy::primitives::address!("4200000000000000000000000000000000000015");
    let (ep, implementation) = (o.stack.entry_point, o.stack.release.implementation);
    let missing_ep = format!("no EntryPoint v0.9 at {ep}");
    let op = with(&|c| c.l1_fee = L1Fee::OpStack);
    // The oracle's L1 fee bound: 0, or for one row its calldata size × 2^230.
    let oracle = alloy::primitives::address!("420000000000000000000000000000000000000F");
    o.set_code(oracle, zero).await;
    let huge = "60043560e61b60005260206000f3";
    let costly = with(&|c| {
        (c.l1_fee, c.global_budget.wei) = (L1Fee::OpStack, U256::MAX >> 10);
        c.max_gas_limit = 5_000_000; // 1 250 000 zero bytes: an L1 bound over 2^250
    });
    let balance = o.owner.get_balance(o.relayer_address).await.unwrap();
    // A block a second, as the configuration sets, for the OP-stack rows.
    o.owner.anvil_mine(Some(12), Some(1)).await.unwrap();
    #[rustfmt::skip]
    let rows: Vec<Case> = vec![
        (ok.clone(), None, "", ""),
        (with(&|c| c.min_balance = balance), None, "", ""),
        (with(&|c| c.chain_id = 1), None, "point rpc_url at it", ""),
        (with(&|c| c.relayer = at), None, "set address to the signer's", ""),
        (with(&|c| c.factory = at), None, "[contracts] factory", ""),
        (ok.clone(), Some((implementation, one)), "set the factory that published it", ""),
        (ok.clone(), Some((ep, "")), &missing_ep, ""),
        (ok.clone(), Some((o.stack.factory, "60406000f3")), "published no release", ""),
        (with(&|c| c.supported_releases = vec![2]), None, "", "run a relayer that decodes"),
        (with(&|c| c.min_balance = U256::MAX), None, "", "fund"),
        (with(&|c| c.global_budget.wei = U256::ONE), None, "", "global_budget 1 is below"),
        // Blocks a second apart below, not the 2 s set.
        (with(&|c| (c.l1_fee, c.block_time) = (L1Fee::OpStack, Duration::from_secs(2))),
         None, "block_time 2s", ""),
        // 5M gas at a fee bound over 2e5 wei is over 1e12 wei.
        (with(&|c| (c.max_gas_limit, c.global_budget.wei) = (5_000_000, U256::from(10u64.pow(12)))),
         None, "", "below one request's worst loss"),
        (costly, Some((oracle, huge)), "", "the L1 fee bound"),
        (op.clone(), Some((l1_block, one)), "", "operator fee is on"),
        (op.clone(), Some((l1_block, revert)), "", "no operator fee getters"),
        (op.clone(), Some((l1_block, "60016000f3")), "buffer overrun", ""), // one byte back
        (op.clone(), Some((l1_block, zero)), "", ""),
        (op.clone(), Some((l1_block, "fe")), "InvalidFEOpcode", ""), // a halt, no revert
        (op, None, "", "no operator fee getters"),
    ];
    for (config, planted, refused, warned) in rows {
        let (at, hex) = planted.unwrap_or((Address::ZERO, "")); // no code at zero
        let kept = o.owner.get_code_at(at).await.unwrap();
        o.set_code(at, hex).await;
        match check(&o.relayer, &config, o.relayer_address).await {
            Ok(warnings) => {
                assert_eq!(refused, "", "passed");
                let named = warnings.iter().any(|w| w.contains(warned));
                let expected = if warned.is_empty() {
                    warnings.is_empty()
                } else {
                    named
                };
                assert!(expected, "{warned}: {warnings:?}");
            }
            Err(e) => assert!(
                !refused.is_empty() && e.to_string().contains(refused),
                "{e}"
            ),
        }
        o.owner.anvil_set_code(at, kept).await.unwrap();
    }
    // A global budget of exactly one request's worst loss passes unwarned.
    let budget =
        |wei| with(&|c| (c.max_gas_limit, c.global_budget.wei) = (5_000_000, wei));
    let warned = check(&o.relayer, &budget(U256::ONE), o.relayer_address).await;
    let warned = warned.unwrap().concat();
    let worst = warned.split("worst loss ").nth(1).unwrap();
    let worst = &worst[..worst.find(',').unwrap()];
    let exact = budget(worst.parse().unwrap());
    let none = check(&o.relayer, &exact, o.relayer_address).await.unwrap();
    assert!(none.is_empty(), "{none:?}");
    // A batch's value is not a loss: a value cap adds none.
    let capped = Config {
        max_value: Some(U256::MAX),
        ..exact
    };
    let none = check(&o.relayer, &capped, o.relayer_address).await.unwrap();
    assert!(none.is_empty(), "{none:?}");
}

/// A binary's exit: its file, its environment, its arguments and its hint; none when
/// it succeeds.
type Exit<'a> = (
    &'a PathBuf,
    &'a [(&'a str, &'a str)],
    &'a [&'a str],
    &'a str,
);

/// A child process, killed when dropped.
struct Killed(std::process::Child);

impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
}

/// `relayer` started from a file holding its key and RPC URL, both marked: help, a
/// missing file, a mangled key, a wrong chain, an unknown key, the key pasted as the
/// address and a missing network file exit with their hints; `run` serves `/health`,
/// `/v1/config`, a quote and a relay; with the node gone, its errors log the URL
/// redacted; an interrupt stops it. No output carries a marker.
#[tokio::test]
async fn run_serves_from_a_file_and_no_output_carries_a_secret() {
    let mut o = Onboarding::new(true).await;
    let key = format!("0x{}0123", KEY_MARKER.repeat(5));
    let signer: PrivateKeySigner = key.parse().unwrap();
    o.relayer_address = signer.address();
    o.set_balance(signer.address(), U256::from(CLAIMED)).await;
    let port = o._anvil.port();
    let rpc = format!(
        "rpc_url = \"http://user:{URL_MARKER}@127.0.0.1:{port}/?key={URL_MARKER}\""
    );
    let (chain, signed) = (
        format!("chain_id = {}", o.chain_id),
        format!("signer = \"{key}\""),
    );
    let fixed = "{ fixed = { denominator = 8, elasticity = 2, min_base_fee = 0 } }";
    let file = |extra: &[(&str, &str)]| {
        let mut text = EXAMPLE.to_owned();
        for at in [o.stack.factory, signer.address()] {
            text = text.replacen(&Address::ZERO.to_string(), &at.to_string(), 1);
        }
        #[rustfmt::skip]
        let anvil = [
            ("rpc_url = \"https://sepolia.base.org\"", rpc.as_str()),
            ("# signer = \"alias/libid-relayer\"", &signed),
            ("chain_id = 84532", &chain), ("\"op-stack\"", "\"none\""), ("\"header\"", fixed),
            ("max_fee_percent = 210", "max_fee_percent = 181"), ("\"200ms\"", "\"1s\""),
            ("poll_interval = \"1s\"", "poll_interval = \"100ms\""), ("\"10s\"", "\"5s\""),
            ("port = 8080", "port = 0"), ("metrics_port = 9090", "metrics_port = 0"),
            // /health goes stale 1 s and ten 100 ms polls after the last good tick.
            ("tick_timeout = \"30s\"", "tick_timeout = \"1s\""),
            ("\"20000000000000000\"", "\"1000000000000000000\""),
        ];
        let pairs = anvil.iter().chain(extra);
        let replace = |t: String, (a, b): &(&str, &str)| {
            assert!(t.contains(a), "{a} not in the example");
            t.replace(a, b)
        };
        write("run", &pairs.fold(text, replace))
    };
    let relayer = |config: &Path, env: &[(&str, &str)], args: &[&str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_relayer"));
        command.arg("--config").arg(config).args(args);
        command.env("RUST_LOG", "trace").envs(env.iter().copied());
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        command
    };
    let (address, mangled) = (
        format!("address = \"{}\"", signer.address()),
        format!("{KEY_MARKER}0"),
    );
    let (pasted, limit) = (format!("address = \"{key}\""), "concurrency_limit = 64");
    let network =
        "# network_file = \"../chain-configurations/networks/base-sepolia.toml\"";
    let (good, wrong) = (file(&[]), file(&[(&chain, "chain_id = 1")]));
    let unknown = file(&[(limit, "concurrency_limit = 64\ncolour = 1")]);
    let (pasted, lost) = (
        file(&[(&address, &pasted)]),
        file(&[(network, "network_file = \"lost.toml\"")]),
    );
    let (nowhere, none) = (PathBuf::from("nowhere.toml"), &[][..]);
    #[rustfmt::skip]
    let exits: [Exit; 8] = [
        (&good, none, &["--help"], ""),
        (&good, none, &["run", "--help"], ""),
        (&nowhere, none, &["run"], "--config or RELAYER_CONFIG"),
        (&good, &[("RELAYER_SIGNER", &mangled)], &["run"], "fix RELAYER_SIGNER"),
        (&wrong, none, &["run"], "point rpc_url at it"),
        (&unknown, none, &["run"], "unknown field `colour`"),
        (&pasted, none, &["run"], "column 11"),
        (&lost, none, &["run"], "network_file lost.toml: No such file"),
    ];
    let mut seen = String::new();
    for (config, env, args, hint) in exits {
        let out = relayer(config, env, args).output().unwrap();
        let text =
            String::from_utf8_lossy(&[out.stdout, out.stderr].concat()).into_owned();
        assert_eq!(out.status.success(), hint.is_empty(), "{text}");
        assert!(text.contains(hint), "{hint}: {text}");
        seen.push_str(&text);
    }

    let log = good.with_file_name("relayer.log");
    let stdout = std::fs::File::create(&log).unwrap();
    let stderr = stdout.try_clone().unwrap();
    let child = relayer(&good, none, &["run"])
        .stdout(stdout)
        .stderr(stderr)
        .spawn();
    let mut child = Killed(child.unwrap());
    let logs = || std::fs::read_to_string(&log).unwrap();
    let mut ports = vec![];
    for _ in 0..600 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Some((_, at)) = logs().split_once("relayer listening on ") {
            let at = at.lines().next().unwrap().split(", metrics on ");
            ports.extend(at.map(|a| a.rsplit(':').next().unwrap().to_owned()));
            break;
        }
    }
    let url = |i: usize, path: &str| format!("http://127.0.0.1:{}{path}", ports[i]);
    let http = reqwest::Client::new();
    let call = async |i: usize, path: &str, body: Option<&Value>| {
        let request = match body {
            Some(body) => http.post(url(i, path)).json(body),
            None => http.get(url(i, path)),
        };
        let response = request.send().await.unwrap();
        (response.status().as_u16(), response.text().await.unwrap())
    };
    let health = async |code: u16| {
        for _ in 0..50 {
            if call(0, "/health", None).await.0 == code {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    };
    assert!(health(200).await, "no tick succeeded: {}", logs());
    tokio::time::sleep(Duration::from_millis(37)).await; // off the 100 ms ticks
    assert_eq!(
        call(0, "/health", None).await.0,
        200,
        "stale before 10 ticks"
    );
    let config: Value =
        serde_json::from_str(&call(0, "/v1/config", None).await.1).unwrap();
    assert_eq!(config["relayer"], json!(signer.address()));
    assert_eq!(config["registry"], json!(o.stack.wallet_registry));
    // Signed to the file's quote validity and window, 5 s each.
    let (now, calls) = (o.now().await, encode_executions(&o.onboarding()));
    let request = o.request(o.sign_until(calls, o.value, now, now + 10));
    let c = request.create.unwrap();
    let create =
        json!({"platformId": c.platform_id, "idNode": c.id_node, "salt": c.salt});
    let wire = json!({"wallet": request.wallet, "create": create, "data": request.data,
                      "gasLimit": request.gas_limit});
    let quoted = call(0, "/v1/quote", Some(&wire)).await;
    let relayed = call(0, "/v1/relay", Some(&wire)).await;
    assert_eq!((quoted.0, relayed.0), (200, 202), "{quoted:?} {relayed:?}");
    let mut answered = [quoted.1, relayed.1, call(1, "/metrics", None).await.1].concat();

    drop(o); // the node gone: ticks and quotes meet node errors naming its URL
    tokio::time::sleep(Duration::from_millis(500)).await;
    let unreachable = call(0, "/v1/quote", Some(&wire)).await;
    assert_eq!(unreachable.0, 503, "{unreachable:?}");
    answered.push_str(&unreachable.1);
    assert!(health(503).await, "healthy with every tick failing");
    let pid = child.0.id().to_string();
    Command::new("kill").args(["-INT", &pid]).status().unwrap();
    assert!(child.0.wait().unwrap().success(), "not shut down");
    let (logs, all) = (logs(), [logs(), seen, answered].concat().to_lowercase());
    for marker in [KEY_MARKER, URL_MARKER] {
        assert!(!all.contains(marker), "{marker} in an output: {all}");
    }
    assert!(
        logs.contains(&format!("127.0.0.1:{port}/…")),
        "no node error: {logs}"
    );
    assert!(logs.contains("shutting down"), "{logs}");
    // Trace from other targets; the HTTP transport's own debug held back.
    assert!(logs.contains("TRACE alloy_rpc_client"), "{logs}");
    assert!(!logs.contains("received response from server"), "{logs}");
}
