# Repro It CLI

The Repro It CLI reproduces a production bug, tests a fix, and keeps a regression check.

## Install

Install the signed `reproit` executable from your Repro It release bundle. Verify its checksum
before you run it.

See [Install Repro It](docs/install.md) for Linux, macOS, Windows, and source-build instructions.

## Connect an application

Run these commands in the Git repository for your application:

```sh
reproit login
reproit init
```

`reproit init` first checks for complete automatic World capture support. For Go,
it compiles the selected package and verifies the required instrumentation in the
temporary binary. It does not run the binary. For Node.js and Python, the SDK
exits from an internal probe before application code runs. If the exact proof is
absent, the command stops before it writes `.reproit/project.toml`.

The current probe supports direct Go, Node.js, and Python application commands.
After complete support is installed, `reproit init` connects one service and SDK.
The application does not create Repro It schemas or IDs.

## Fix a captured bug

```sh
reproit list
reproit debug <id>
reproit check <id>
reproit keep <id>
reproit check
```

| Command | Result |
| --- | --- |
| `list` | Show verified Repros that need work. |
| `debug <id>` | Reproduce the Failure and show the debugger connection. |
| `check <id>` | Test the current source against one Repro. |
| `keep <id>` | Add the passing Repro to the repository. |
| `check` | Run all tracked Repros. |
| `mcp` | Give a coding agent the same bounded Repro operations. |
| `gate --config <path>` | Run a baseline and candidate, then make a release decision. |
| `verify <bundle-path>` | Verify a content-addressed release evidence bundle offline. |

`PASS` means that the captured Failure is absent. `REGRESSION` means that it still occurs. `ERROR`
means that Repro It could not produce an exact result.

`reproit mcp` serves MCP through standard input and standard output. It uses the same login,
authorization, and application operations as the human commands.

Read the [quick start](docs/quick-start.md) for the full bug-fix loop. Use the
[command reference](docs/commands.md) for options and exit codes.

## Develop the CLI

Run the complete repository check:

```sh
./tools/test.sh
```

Read [CONTRIBUTING.md](CONTRIBUTING.md) before you change public command behavior.

The release-gate integration pins the Experiments and ML repositories to exact Git revisions.
