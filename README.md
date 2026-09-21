# Repro It CLI

The Repro It CLI reproduces production bugs, tests fixes, and keeps regression
checks.

## Install

Choose one installation method.

### Run the installer

```sh
git clone https://github.com/ReproIt/reproit-cli.git
cd reproit-cli
./install.sh
```

Windows users can run `.\install.ps1` from PowerShell. See
[Install Repro It](docs/install.md) for requirements and login configuration.

### Build from source

```sh
git clone https://github.com/ReproIt/reproit-cli.git
cd reproit-cli
cargo build --locked --release --package reproit-cli --bin reproit
```

The executable is at `target/release/reproit` on Linux and macOS. Windows uses
`target\release\reproit.exe`.

## Connect an application

Run these commands in the Git repository for your application:

```sh
reproit login
reproit init
```

`reproit init` checks for complete automatic World capture support. The current
probe supports direct .NET, Go, Node.js, Python, and Rust application commands.
After the check passes, the command connects one service and SDK.

## Fix a captured bug

```sh
reproit list
reproit debug REPRO_ID
reproit check REPRO_ID
reproit keep REPRO_ID
reproit check
```

| Command | Result |
| --- | --- |
| `list` | Show verified Repros that need work. |
| `triage REPRO_ID` | Change the priority, assignment, or workflow state. |
| `debug REPRO_ID` | Reproduce the Failure and show the debugger connection. |
| `check REPRO_ID` | Test the current source against one Repro. |
| `keep REPRO_ID` | Add the passing Repro to the repository. |
| `check` | Run all tracked Repros. |
| `remove REPRO_ID` | Remove a tracked reference. |
| `mcp` | Give a coding agent the same bounded Repro operations. |

`PASS` means that the captured Failure is absent. `REGRESSION` means that the
Failure still occurs. `UNKNOWN` means that the evidence cannot support a
decision. `ERROR` means that Repro It could not complete or verify the
operation.

`reproit mcp` serves MCP through standard input and standard output.

Read the [quick start](docs/quick-start.md) for the full bug-fix loop. Use the
[command reference](docs/commands.md) for options and exit codes.

## Develop the CLI

Run the complete repository check:

```sh
./tools/test.sh
```

Read [CONTRIBUTING.md](CONTRIBUTING.md) before you change public command
behavior.
