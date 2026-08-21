# Contributing

Use this guide when you change the managed Backend v1.0 developer loop.

## Before you change code

1. Read `core-pin.json`.
2. Read the public contract in the pinned Repro It Core revision.
3. Identify the user-visible command behavior that the change affects.
4. Add or update a test for that behavior.

Do not copy Core types, schemas, or vectors into this repository. Update Core first when the shared
contract must change, then update the exact pin in one commit.

Do not add Cloud, Runtime, admission, worker service, customer OCI, private mode, or MCP code to this
repository.

## Verify a change

```sh
./tools/test.sh
```

The check formats the Rust code, runs strict Clippy with warnings denied, and runs all tests. It also
rejects vendored dependency directories and unpinned Repro It Core dependencies.

Use exact paths when you stage a change. Do not commit `.core`, `specs`, `target`, credentials,
tokens, private keys, generated release bundles, or validation evidence.
