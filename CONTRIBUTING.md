# Contributing

CLI changes must preserve the managed Backend v1.0 developer loop.

## Before you change code

1. Read `core-pin.json`.
2. Read the public contract in the pinned Repro It Core revision.
3. Identify the user-visible command behavior that the change affects.
4. Add or update a test for that behavior.

Do not copy Core types, schemas, or vectors into this repository. Update Core first when the shared
contract must change, then update the exact pin in one commit.

Keep Cloud, Runtime, admission, worker services, customer OCI, and private mode in their owning
repositories. Keep MCP schemas in Repro It Core. Keep the MCP command adapter in this repository.

## Verify a change

```sh
./tools/test.sh
```

The check formats Rust code and runs strict Clippy with warnings denied. It runs all tests. It also
rejects vendored dependencies and unpinned Repro It Core dependencies.

Use exact paths when you stage a change. Do not commit `.core`, `specs`, `target`, credentials,
tokens, private keys, generated release bundles, or validation evidence.
