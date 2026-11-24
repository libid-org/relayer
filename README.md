# libID relayer

Sends signed `AUTHORIZED_MODE` batches of libID web wallets, starting with
one-transaction onboarding through `WalletFactory.createAndExecute`, and is
repaid by a refund inside each batch.

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
`Cargo.toml`, then run the script again.

## Test

```sh
cargo test
```

`tests/anvil.rs` starts its own `anvil` (Foundry), which must be on `PATH`.

CI (`.github/workflows/ci.yml`) runs the vendor script, then formatting,
clippy and the tests, on the Foundry and Rust versions it pins.
