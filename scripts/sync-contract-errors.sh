#!/usr/bin/env bash
# Vendors contracts/bounty_escrow/src/errors.rs from Soroban-escrow/escrow-contracts at a pinned commit.
# Usage: scripts/sync-contract-errors.sh [<commit-sha>]   (default: the current head of the contracts repo)
# Then update CONTRACT_ERRORS in crates/escrow-core/src/contract_errors.rs and run:
#   cargo test -p escrow-core --test contract_errors_sync
set -euo pipefail

repo=Soroban-escrow/escrow-contracts
dir="$(cd "$(dirname "$0")/.." && pwd)/crates/escrow-core/tests/fixtures"
sha="${1:-$(gh api "repos/$repo/commits/HEAD" -q .sha)}"
[[ "$sha" =~ ^[0-9a-f]{40}$ ]] || { echo "expected a full 40-character commit sha, got: $sha" >&2; exit 1; }

gh api -H "Accept: application/vnd.github.raw" "repos/$repo/contents/contracts/bounty_escrow/src/errors.rs?ref=$sha" >"$dir/bounty_escrow_errors.rs.txt"
echo "$sha" >"$dir/bounty_escrow_errors.commit"
echo "Vendored errors.rs at $sha"
