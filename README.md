# Repro It CLI

Use the Repro It CLI to reproduce a production bug, inspect it with a standard debugger, test a
fix, and keep the bug as a regression check.

The CLI connects to managed Repro It. It does not contain Cloud, Runtime, admission, or worker
service code. [Repro It Core](https://github.com/ReproIt/reproit-core) owns the shared contracts.
This repository pins one exact Core revision in `core-pin.json`.

## Install

Install the signed `reproit` executable for your operating system from a Repro It release bundle.
The bundle includes a checksum manifest. Verify the checksum before you install the executable.

For source builds, install Git and the Rust toolchain from `rust-toolchain.toml`, then run:

```sh
cargo install --locked --path crates/reproit-cli
reproit --version
```

Source builds are for development. Official release builds contain the production OAuth metadata.
An unbound source build fails closed when you run `reproit login`.

See [Install Repro It](docs/install.md) for Linux, macOS, and Windows instructions.

## Start

Run these commands from the Git repository for your application:

```sh
reproit login
reproit init
```

`reproit init` selects one managed service and one SDK. It writes the reviewed project binding to
`.reproit/project.toml` and prints the SDK setup for your application.

After your application captures a failed operation, use this loop:

```sh
reproit list
reproit debug <id>
reproit check <id>
reproit keep <id>
reproit check
```

`debug` reproduces the captured Failure and prints a loopback debugger endpoint. `check <id>` runs
the changed source. It prints `PASS` when the captured Failure is absent and `REGRESSION` when the
Failure returns. `keep` writes a tracked managed reference. A later `reproit check` runs every
tracked reference.

Read the [quick start](docs/quick-start.md) for the complete developer loop. Read the
[command reference](docs/commands.md) for command behavior and exit codes.

## Public commands

- `reproit login`
- `reproit init`
- `reproit list`
- `reproit triage <id>`
- `reproit debug <id>`
- `reproit check <id>`
- `reproit check`
- `reproit keep <id>`
- `reproit remove <id>`

The v1.0 CLI does not include MCP, private Runtime, customer OCI storage, or customer worker
commands.

## SDKs

Use the framework-neutral Rust, Python, Go, Node.js, or .NET SDK from
[reproit-sdk](https://github.com/ReproIt/reproit-sdk). The SDK works in a host process or a
container. It does not require a sidecar, container engine, orchestrator, or container socket.

## Develop

Run the complete repository check:

```sh
./tools/test.sh
```

The script fetches the exact Core revision into the ignored `.core` directory. No dependency is
vendored in this repository.

See [CONTRIBUTING.md](CONTRIBUTING.md) before you change public behavior.
