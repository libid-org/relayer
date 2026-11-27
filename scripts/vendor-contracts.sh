#!/usr/bin/env bash
# Vendor the artifacts the pinned libid-contracts crate embeds.
#
# The crate is a git dependency pinned in Cargo.toml. Its artifacts/,
# artifacts-testing/ and abi/ are gitignored in that repo, so cargo's checkout
# lacks them and the crate does not compile. This script finds the checkout
# with `cargo metadata` and fills them:
#
#   scripts/vendor-contracts.sh           # runs the pinned repo's own vendor
#                                         # scripts there (forge, jq, curl)
#   scripts/vendor-contracts.sh <tree>    # copies them from <tree>, a
#                                         # libid-contracts clone at the pinned
#                                         # commit that already ran them
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
cargo fetch --quiet
MANIFEST="$(cargo metadata --format-version 1 |
    jq -r '.packages[] | select(.name == "libid-contracts") | .manifest_path')"
CRATE="$(dirname "$MANIFEST")"
CHECKOUT="$(cd "$CRATE/../.." && pwd)"
REV="$(git -C "$CHECKOUT" rev-parse HEAD)"

if [[ $# -eq 0 ]]; then
    "$CHECKOUT/scripts/vendor-circuit-verifiers.sh"
    "$CHECKOUT/scripts/vendor-artifacts.sh"
elif [[ $# -eq 1 ]]; then
    FROM="$(git -C "$1" rev-parse HEAD)"
    [[ "$FROM" == "$REV" ]] || { echo "$1 is at $FROM, the pin is $REV" >&2; exit 1; }
    for dir in artifacts artifacts-testing abi; do
        [[ -d "$1/rust/contracts/$dir" ]] || { echo "no rust/contracts/$dir in $1; run its scripts/vendor-artifacts.sh" >&2; exit 1; }
    done
    for dir in artifacts artifacts-testing abi; do
        rm -rf "${CRATE:?}/$dir"
        cp -R "$1/rust/contracts/$dir" "$CRATE/$dir"
    done
else
    echo "usage: $0 [libid-contracts tree at $REV]" >&2
    exit 2
fi
echo "==> vendored libid-contracts $REV artifacts into $CRATE"
