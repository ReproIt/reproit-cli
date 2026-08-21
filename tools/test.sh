#!/usr/bin/env bash
set -euo pipefail

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly repository_root

if find "$repository_root" -type d \( -name vendor -o -name node_modules \) -print -quit | grep -q .; then
  echo "Vendored dependency directories are not allowed." >&2
  exit 1
fi

if rg -n '^\[patch\.crates-io\]' "$repository_root"; then
  echo "Crates.io dependency patches are not allowed." >&2
  exit 1
fi

if rg -n 'reproit-(core|backend|cloud-api|worker)\s*=.*path\s*=' \
  "$repository_root"/Cargo.toml "$repository_root"/crates/*/Cargo.toml; then
  echo "Repro It Core dependencies must use the exact shared revision." >&2
  exit 1
fi

"$repository_root/tools/with-core.sh" cargo fmt --all -- --check
"$repository_root/tools/with-core.sh" cargo clippy --workspace --all-targets --all-features -- -D warnings
"$repository_root/tools/with-core.sh" cargo test --workspace --all-targets
