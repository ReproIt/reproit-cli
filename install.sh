#!/usr/bin/env bash
set -euo pipefail

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly repository_root

if ! command -v cargo >/dev/null 2>&1; then
  echo "Cargo is required. Install Rust, then run this script again." >&2
  exit 1
fi

cargo install \
  --locked \
  --force \
  --path "$repository_root/crates/reproit-cli" \
  --bin reproit
