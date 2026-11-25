# libID relayer

Sends signed `AUTHORIZED_MODE` batches of libID web wallets, starting with
one-transaction onboarding through `WalletFactory.createAndExecute`, and is
repaid by a refund inside each batch. One relayer serves one chain with one key.

## Run

```sh
relayer --config relayer.toml run     # check the chain, then serve
relayer --config relayer.toml stats   # the ledger's numbers
relayer --config relayer.toml stats --json
```

`--config` defaults to `RELAYER_CONFIG`, then `relayer.toml`. `run` stops on
Ctrl-C or SIGTERM once requests in flight finish; a second signal exits at
once. `stats` opens the ledger read-only, so it works beside a running relayer.

## Configure

[`relayer.example.toml`](relayer.example.toml) lists every key, with what it
means. Durations are humantime (`"10s"`); wei amounts are decimal strings.
Relative paths are read from the file's directory. Loading refuses, naming the
setting, a zero `poll_interval`, `request_timeout` or `concurrency_limit`, a
`tip_percentile` outside 50 to 90, a `max_gas_limit` above 2^24 (EIP-7825), an
API port equal to `metrics_port`, and an empty `RELAYER_SIGNER`.

The environment overrides the file:

| Variable          | Overrides          |
| ----------------- | ------------------ |
| `RELAYER_CONFIG`  | `--config`         |
| `RELAYER_RPC_URL` | `[chain] rpc_url`  |
| `RELAYER_SIGNER`  | `[key] signer`     |
| `RELAYER_PORT`    | `[http] port`      |
| `RUST_LOG`        | the log filter, `info` by default |

The signer is a 64-hex-digit key or an AWS KMS key id, alias or ARN
(`alias/libid-relayer`), with credentials from the AWS environment. Prefer
the environment for both the key and an RPC URL with an API key. Logs show
the RPC URL's scheme, host and port only.

## Startup checks

`run` refuses to start, naming the fix, when:

- the node's chain id differs from `[chain] chain_id`, or a `network_file`'s;
- the signer's address differs from `[key] address` (a KMS key included);
- the factory or the forwarder has no code;
- the forwarder's `FACTORY()` is not the factory;
- the factory has published no release;
- the latest release's `FACTORY()` and `REGISTRY()` are not the factory's;
- the release's EntryPoint v0.9 has no code;
- the latest release is not in `supported_releases`;
- the signer's balance is under `min_balance`;
- `wallet_budget` is under one request's worst loss: `max_gas_limit` at
  `max_fee_percent` of the base fee, plus the L1 fee bound of `max_gas_limit`
  of zero-byte calldata, plus `max_value` when set;
- the OP-stack operator fee is on;
- another relayer holds the ledger's lock (`<db>.lock`);
- the chain's nonce of the key disagrees with the ledger's.

## HTTP API

On `[http] port`:

| Route                  | Body                     | Answer |
| ---------------------- | ------------------------ | ------ |
| `GET /v1/config`       |                          | `200` chain id, relayer, factory, registry, forwarder, supported releases, limits |
| `POST /v1/quote`       | `{wallet, create?, data, belowQuorum?}` | `200 {relayer, callGas, gasLimit, maxFeePerGas, l1FeeBound, minRefund, validUntil}` |
| `POST /v1/relay`       | `{wallet, create?, data, gasLimit}` | `202 {id, status}` |
| `GET /v1/relay/{id}`   |                          | `200 {status, txHashes, blockNumber?, reverted?, error?}` |
| `GET /health`          |                          | `200 OK`, or `503` once no tick succeeded for ten poll intervals |

`create` is `{platformId, idNode, salt}` when the batch creates the wallet.
`data` is `execute(AUTHORIZED_MODE, data)`'s data; a quote's carries dummy
signatures. A status is `queued`, `sent`, `included`, `cancelled` or `failed`.
Relaying the same request again answers its current status; a failed one is
queued again. A batch whose signed transaction would be over 128 KiB, which node
pools refuse, is `TooLarge`; `/v1/config` gives that bound as
`maxTransactionBytes`. A first send the node refuses fails its request as `Node`,
unless a pending transaction of the key holds its nonce, which pauses sending.
A receipt that does not parse for `node_error_ticks` ticks, once
its nonce is mined, is charged at its send's worst cost, and its request fails
as `Node`; until then the send is watched, sent again and cancelled as usual.

A refusal is `{"error": {"name", "message", "args"?, "contract"?}}`:

| Status | Names |
| ------ | ----- |
| `400`  | `BadRequest`, `NoncanonicalData`, `RefundMissing`, `RefundTooLow`, `GasLimitTooLow`, `GasLimitTooHigh`, `TooLarge`, `ValueTooHigh`, `UnknownWallet`, `ValueNotAllowed`, `UnsupportedRelease`, `Expired`, a reverting batch's decoded error (`Reverted` when unknown) |
| `404`  | `UnknownRequest` |
| `408`  | past `request_timeout` |
| `409`  | `WalletBusy` |
| `429`  | `RateLimited`: the wallet's loss budget is spent |
| `503`  | `RelayerUnderfunded`, `Paused`, `Node`, `Ledger` |

`GET /metrics` is OpenMetrics on `[http] metrics_port`, under `libid_relayer_`.

## Local stack

```sh
docker compose up --wait --build   # anvil, the wallet stack, the relayer
cargo test --test e2e -- --ignored --nocapture
docker compose down -v
```

The API is on `127.0.0.1:8080`, metrics on `:9090`, anvil on `:8545`. The
`deploy` one-shot (`examples/deploy_local.rs`) deploys the contracts' test
stack, whose verifier checks no proof, and writes the relayer's file. The
relayer signs with anvil's dev key #1, a public key: never fund it on a real
chain. Run `down -v` before starting again: a ledger on a fresh anvil fails
the nonce check.

## Operate

- Rate-limit per IP in front of the relayer (Cloudflare, an AWS load
  balancer). The relayer bounds losses per wallet and in total, not per caller.
- Keep `/metrics` private: it has its own port for that.
- Run one instance per key. The ledger's lock stops a second one on the same
  file, not on another host.
- Fund the key. Under `min_balance` the relayer pauses sending and answers
  `503`; watch `libid_relayer_key_balance_wei` and `libid_relayer_paused`.
- For a KMS key, set `RELAYER_SIGNER=alias/<name>` and `[key] address` to the
  key's address.

The image (`Dockerfile`) reads `/etc/relayer/relayer.toml` and holds no key.
Point `[state] db` at a mounted volume, such as `/var/lib/relayer/relayer.db`.

## Build

The contract bindings come from the `libid-contracts` crate, a git dependency
pinned to a commit of [libID-contracts](https://github.com/libid-org/libID-contracts)
in `Cargo.toml`. The crate embeds forge artifacts that its repo does not
commit, so vendor them into cargo's checkout once per pin, before any other
cargo command:

```sh
scripts/vendor-contracts.sh          # builds them there: needs forge, jq, curl
scripts/vendor-contracts.sh <clone>  # or copies them from a libID-contracts
                                     # clone at the pinned commit that ran
                                     # its scripts/vendor-artifacts.sh
```

Moving the pin: change `rev` in `[workspace.dependencies]` of
`Cargo.toml`, then run the script again. The Docker build runs the script
itself.

## Test

```sh
cargo test
```

`tests/anvil.rs` starts its own `anvil` (Foundry), which must be on `PATH`.
`tests/e2e.rs` needs the local stack; it skips without it, and fails under CI.

CI (`.github/workflows/ci.yml`) runs the vendor script, then formatting,
clippy and the tests, on the Foundry and Rust versions it pins; its `e2e` job
runs the e2e test against the compose stack.
