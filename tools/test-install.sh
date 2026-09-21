#!/usr/bin/env bash
set -euo pipefail

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly repository_root

mkdir -p "$repository_root/target"
temporary="$(mktemp -d "$repository_root/target/install-test.XXXXXX")"
readonly temporary
trap 'find "$temporary" -depth -delete' EXIT

mkdir "$temporary/bin"
printf '%s\n' \
  '#!/bin/sh' \
  'printf "%s\n" "$@"' \
  >"$temporary/bin/cargo"
chmod +x "$temporary/bin/cargo"

actual="$(PATH="$temporary/bin:/usr/bin:/bin" "$repository_root/install.sh")"
expected="$(printf '%s\n' \
  install \
  --locked \
  --force \
  --path \
  "$repository_root/crates/reproit-cli" \
  --bin \
  reproit)"

if [[ "$actual" != "$expected" ]]; then
  echo "The shell installer used unexpected Cargo arguments." >&2
  exit 1
fi

find "$temporary/bin/cargo" -delete
ln -s /usr/bin/dirname "$temporary/bin/dirname"
if PATH="$temporary/bin" /bin/bash "$repository_root/install.sh" \
  >"$temporary/stdout" 2>"$temporary/stderr"
then
  echo "The shell installer accepted a missing Cargo executable." >&2
  exit 1
fi

if [[ -s "$temporary/stdout" ]] ||
  [[ "$(<"$temporary/stderr")" != \
    "Cargo is required. Install Rust, then run this script again." ]]
then
  echo "The shell installer returned an unexpected missing-Cargo error." >&2
  exit 1
fi
