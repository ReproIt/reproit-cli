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

`reproit init` connects one service and SDK. It writes `.reproit/project.toml` and prints the exact
SDK installation steps.

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

`PASS` means that the captured Failure is absent. `REGRESSION` means that it still occurs. `ERROR`
means that Repro It could not produce an exact result.

Read the [quick start](docs/quick-start.md) for the full bug-fix loop. Use the
[command reference](docs/commands.md) for options and exit codes.

## Develop the CLI

Run the complete repository check:

```sh
./tools/test.sh
```

Read [CONTRIBUTING.md](CONTRIBUTING.md) before you change public command behavior.
