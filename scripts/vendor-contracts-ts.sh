#!/usr/bin/env bash
# Build @libid/contracts, the TypeScript package of the libid-contracts commit
# Cargo.toml pins, and link it at ts/vendor/contracts, where ts/package.json
# depends on it.
#
# The build runs in a scratch copy of cargo's checkout: the pinned repo's
# circuit verifiers, `forge build`, pnpm install, codegen and tsc (forge, jq,
# curl, pnpm). Only the finished dist/ moves into the checkout, so a failed
# build leaves nothing there, and the checkout's solidity/ stays as cargo and
# scripts/vendor-contracts.sh left it. A checkout that holds dist/ is only linked.
#
#   scripts/vendor-contracts-ts.sh
#   pnpm -C ts install                # copies the package into node_modules
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
cargo fetch --quiet
MANIFEST="$(cargo metadata --format-version 1 |
    jq -r '.packages[] | select(.name == "libid-contracts") | .manifest_path')"
CHECKOUT="$(cd "$(dirname "$MANIFEST")/../.." && pwd)"
PACKAGE="$CHECKOUT/ts/packages/contracts"

if [[ ! -d "$PACKAGE/dist" ]]; then
    BUILD="$(mktemp -d)"
    trap 'rm -rf "$BUILD"' EXIT
    for dir in scripts solidity ts; do
        rsync -a --exclude out --exclude cache --exclude node_modules \
            "$CHECKOUT/$dir" "$BUILD/"
    done
    "$BUILD/scripts/vendor-circuit-verifiers.sh"
    (cd "$BUILD/solidity" && forge build --skip test --skip script)
    # From inside the copy: pnpm runs the version its package.json pins.
    (cd "$BUILD/ts" && pnpm install --frozen-lockfile)
    (cd "$BUILD/ts/packages/contracts" && pnpm codegen && pnpm build)
    mv "$BUILD/ts/packages/contracts/dist" "$PACKAGE/dist.$$"
    # Not into a dist/ another run finished meanwhile; `mv -T` is GNU only.
    if [[ -e "$PACKAGE/dist" ]]; then
        rm -rf "$PACKAGE/dist.$$"
    else
        mv "$PACKAGE/dist.$$" "$PACKAGE/dist"
    fi
fi

mkdir -p ts/vendor
ln -sfn "$PACKAGE" ts/vendor/contracts
echo "==> linked @libid/contracts at $(git -C "$CHECKOUT" rev-parse HEAD) to ts/vendor/contracts"
