# libID relayer

Sends signed `AUTHORIZED_MODE` batches of libID factory wallets, starting with
one-transaction onboarding through `WalletFactory.createAndExecute`, and is
repaid by a refund inside each batch. EIP-7702 accounts are refused as
`UnknownWallet`. One relayer serves one chain with one key.

## Run

```sh
relayer --config relayer.toml run     # check the chain, then serve
relayer --config relayer.toml stats   # what the ledger holds now
relayer --config relayer.toml stats --json
```

`--config` defaults to `RELAYER_CONFIG`, then `relayer.toml`. `run` stops on
Ctrl-C or SIGTERM once requests in flight finish; a second signal exits at
once. `stats` opens the ledger read-only and prints its rows by status, the
identities a loss holds, what the global budget counts as lost and the losses
not yet charged. It reads a table of at most one row per identity active in
the last `loss_hold`, so it holds the database for milliseconds beside a
running relayer, whose writes wait up to 5 s for it.

## Ledger

`[state] db` is a SQLite file with one row per genesis identity
(`platformId`, `idNode`): `create`'s, or the wallet's `genesisIdentity()` for
an existing wallet, whatever its salt. The row holds the identity's request,
its signed send and cancels, written before each broadcast, the send's worst
cost with its value until its receipt, then what it lost. A paid row is
dropped `receipt_lag_blocks` after its receipt, so a reorg that removes it
has it sent again. A row with a loss from a reverted batch or a skipped
refund is dropped `loss_hold` after it last moved; one from a landed cancel,
a paid refund short of the send's cost, or balances unread
`receipt_lag_blocks` after its receipt, and a failed one, after the global
budget's `window`.

- An identity has one request in flight: another is `IdentityBusy` until the
  row is dropped or failed. Before its check, a relay answers only for its own
  id; busy and held answer after it, so a batch the wallet's key did not sign
  is refused by its check and learns nothing of the identity's request.
- A loss refuses the identity as `IdentityHeld` until its row is dropped:
  `loss_hold` after a reverted batch or a skipped refund, so an honest batch
  that reverts after a good check waits as long; the budget's `window` after
  a landed cancel, an L1 fee over its bound, or balances left unread.
- The global budget sums the rows' charged losses over its `window`. Past it
  sending pauses until the window passes; under `min_balance`, until the key
  is funded. A send also counts each send not yet charged at its worst.
- A database of another schema version is refused, naming the fix: start on
  a fresh file. This is a pre-release; there is no migration.

## Configure

[`relayer.example.toml`](relayer.example.toml) lists every key, with what it
means. Durations are humantime (`"10s"`); wei amounts are decimal strings.
Relative paths are read from the file's directory. Loading refuses, naming the
setting, a zero `poll_interval`, `request_timeout`, `tick_timeout` or
`concurrency_limit`, a `tip_percentile` outside 50 to 90, a `max_gas_limit`
above 2^24 (EIP-7825), an `l1_margin_percent` under 100, a `global_budget` window of 0,
a `loss_hold` of 0 or under that window,
a `receipt_lag_blocks` of 0, an API port equal to `metrics_port`, and an empty
`RELAYER_SIGNER`.

A tick, and each node request, ends at `tick_timeout`: a tick past it is dropped
and logged, and the next resumes its sends, which are committed before they are
broadcast.

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

`run` refuses to start, naming the fix, on what waiting does not cure:

- the node's chain id differs from `[chain] chain_id`, or a `network_file`'s;
- on an OP-stack chain, or with `fee_params = "header"`, `block_time` is more
  than 25% off the chain's mean block time over the last 10 seconds of blocks;
- the signer's address differs from `[key] address` (a KMS key included);
- the factory has no code;
- the factory has published no release;
- the latest release's `FACTORY()` and `REGISTRY()` are not the factory's;
- the release's EntryPoint v0.9 has no code;
- the OP-stack operator fee cannot be read;
- another relayer holds the ledger's lock (`<db>.lock`);
- the chain's nonce of the key disagrees with the ledger's.

It logs a warning, naming the fix, and serves through what the running relayer
serves through:

- the latest release is not in `supported_releases`: its new wallets are
  `UnsupportedRelease`;
- the signer's balance is under `min_balance`: sending pauses until it is
  funded;
- `global_budget` is under one request's worst loss, `max_gas_limit` at
  `max_fee_percent` of the base fee plus the L1 fee bound of `max_gas_limit`
  of zero-byte calldata, at most 128 KiB: such a request is `OverBudget`;
- the OP-stack operator fee is on, which pauses sending while a tick finds it
  on, or `L1Block` has no operator fee getters.

A request whose worst loss, its gas at the head's fee cap (M × base fee, at
least the floor, at most the cap from R), its L1 fee bound and its `value` in
full, is over what the charged losses leave of the global budget is refused
as `OverBudget` when relayed, and fails as `OverBudget` at its send when it
is over what they and the sends not yet charged, each at its worst, leave.

## HTTP API

On `[http] port`:

| Route                  | Body                     | Answer |
| ---------------------- | ------------------------ | ------ |
| `GET /v1/config`       |                          | `200` chain id, relayer, factory, registry, supported releases, limits |
| `POST /v1/quote`       | `{wallet, create?, data, belowQuorum?}` | `200 {relayer, callGas, gasLimit, maxFeePerGas, l1FeeBound, minRefund, validUntil}` |
| `POST /v1/relay`       | `{wallet, create?, data, gasLimit}` | `202 {id, status}` |
| `GET /v1/relay/{id}`   |                          | `200 {status, txHashes, blockNumber?, reverted?, error?}` |
| `GET /health`          |                          | `200 OK`, or `503` once no tick succeeded for `tick_timeout` and ten poll intervals |

`create` is `{platformId, idNode, salt}` when the batch creates the wallet.
`data` is `execute(AUTHORIZED_MODE, data)`'s data; a quote's carries dummy
signatures. A quote's `callGas`, which the wallet reserves before each call,
covers what the batch's dearest call used and a quarter, measured on cuts of
the draft run in one block, for batches of up to 8 calls; a longer batch, one
whose cuts do not run, or one with a cut billed at its EIP-7623 calldata floor,
gets the whole run's gas. A call that needs more
than its run showed, made dearer by an earlier call or swallowing a failure
deep down, may be starved by whoever sends the signed batch. A status is `queued`, `sent`, `included`, `cancelled` or `failed`;
once its row is dropped (see [Ledger](#ledger)) the request is
`404 UnknownRequest`. Relaying the same request again answers its current
status; a failed one is checked and queued again. A later request of the same
identity replaces a failed one, and the failed id is then `UnknownRequest`. A batch whose signed transaction would be over 128 KiB, which node
pools refuse, is `TooLarge`; `/v1/config` gives that bound as
`maxTransactionBytes`. A first send the node's pool refuses, naming in whole
words a replacement, an underpriced or oversized transaction, a full pool
(`is full`), fees, gas or funds, is uncommitted: refused for good (fees, gas,
funds, oversized) it fails its request as `Node`; refused for now it holds the
queue and fails as `Node` after `node_error_ticks` ticks in a row; refused
because a pending transaction of the key holds its nonce it pauses sending.
Any other error, `nonce too low` and a rate limit included, keeps the send
committed: it is watched, sent again and cancelled at its window's end like
any other.
A receipt that does not parse for `node_error_ticks` ticks, once
its nonce is mined, is charged at its send's worst cost without its value, which
a landed send to a vouched wallet cannot lose, and its request fails
as `Node`; until then the send is watched, sent again and cancelled as usual.

A batch's deadline must cover the inclusion window and be at most `max`:
the latest block's time plus `inclusion_window`, `quote_validity` and two
`block_time`s, which `validUntil` plus the window stays within. A later one
is `DeadlineTooFar` (`args.max`): a send in flight at a restart is sent again
until its deadline, holding the key's next nonce until then.

A refusal is `{"error": {"name", "message", "args"?, "contract"?}}`:

| Status | Names |
| ------ | ----- |
| `400`  | `BadRequest`, `NoncanonicalData`, `RefundMissing`, `RefundTooLow`, `GasLimitTooLow`, `GasLimitTooHigh`, `TooLarge`, `ValueTooHigh`, `UnknownWallet`, `UnsupportedRelease`, `Expired`, `DeadlineTooFar`, a reverting batch's decoded error (`Reverted` when unknown) |
| `404`  | `UnknownRequest` |
| `408`  | past `request_timeout` |
| `409`  | `IdentityBusy`: another request of the identity is in flight, `args.id` |
| `429`  | `IdentityHeld`: a loss holds the identity until `args.until`, in unix seconds |
| `503`  | `OverBudget` (`args.worst`, `args.left`), `RelayerUnderfunded`, `Paused`, `Node`, `Ledger` |

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
the nonce check. Anvil mines only on transactions, so a paid row there stays
until `receipt_lag_blocks` more blocks are mined. Beside `relayer.toml`, the one-shot writes `stack.json`, the
identity registry's and the escrow's addresses.

## Client

[`ts/`](ts/README.md) is `@libid/relayer-client`, a TypeScript client that
onboards a wallet through the API: link, bind, escrow claim, a token swap
when the claimed ETH falls short, and refund, in one relayed transaction.

## Operate

- Put a proxy in front of the relayer (Cloudflare, an AWS load balancer)
  that buffers whole request headers and bodies, closes idle connections,
  and limits connections and requests per IP. The relayer sets no header
  deadline and no connection limit of its own: half-sent headers or bodies
  sent to it directly hold its file descriptors and its quote and relay
  permits. Config, status and `/health` answer outside those permits; a
  shutdown waits for requests in flight at most `request_timeout`. The
  relayer bounds losses per identity and in total, not per caller.
- Keep `/metrics` private: it has its own port for that.
- Watch `libid_relayer_oldest_in_flight_seconds`: a node that refuses sends
  and cancels with errors that name no pool refusal stalls the queue with no
  tick failing.
- Run one instance per key. The ledger's lock stops a second one on the same
  file, not on another host.
- Fund the key. Under `min_balance` the relayer pauses sending and answers
  `503`; watch `libid_relayer_key_balance_wei` and `libid_relayer_paused`.
- While sending is paused, for any reason, a relay answers `503 Paused`.
  Sending resumes on its own once the key is funded, the operator fee is off,
  the global budget's window has passed, or a lagging node serves our missing
  receipt. Another sender's transaction on the key, pending at our nonce or
  mined, holds the pause until a restart, and start refuses while the key's
  nonce is past the ledger's: inspect the key, then start on a fresh `db`,
  which drops the held identities and the budget's history.
- For a KMS key, set `RELAYER_SIGNER=alias/<name>` and `[key] address` to the
  key's address.

The image (`Dockerfile`) reads `/etc/relayer/relayer.toml` and holds no key.
Point `[state] db` at a mounted volume, such as `/var/lib/relayer/relayer.db`.

## Known gaps

- A reorg deeper than `receipt_lag_blocks` that removes a paid send after its
  row is dropped leaves the key's nonce behind the ledger with nothing to send
  again: sending waits for that nonce, and start refuses with nonce drift.

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
runs the client's checks, then the Rust and the client's e2e tests against the
compose stack.
