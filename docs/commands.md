# Command reference

## `reproit login`

Sign in through the browser. The CLI stores the session in the native credential store.

## `reproit init`

Check for complete automatic World capture support. For Go, the CLI compiles the
selected package and verifies the required instrumentation in the temporary
binary. It does not run the binary. For Node.js and Python, the SDK reports the
exact shared proof and exits before application code runs. For .NET and Rust, the
SDK verifies its packaged native sentinel and exits during
startup. If verification fails, stop before the command writes
`.reproit/project.toml`.

The current probe supports .NET, Go, Node.js, Python, and Rust. When support is
present, connect the current repository to one service and SDK.

Run it again to change the current setup. The command shows the file change before it writes it.

For Go, provide a direct `go run` command. Initialization adds the internal build
instrumentation flags to the stored run configuration. It does not add a public
language-specific command.

For .NET and Rust, provide a direct `dotnet run` or `cargo run` command. The CLI
stores the command without a language-specific wrapper.

For an agent or script, use:

```sh
reproit init --non-interactive --service NAME --sdk rust --service-path . -- COMMAND ARGUMENT
```

## `reproit add <definition.json>`

Verify and seal one authored research recipe. The definition contains setup
commands, verified inputs, one measurement command, and an inclusive expected
range for each reported metric. It also selects one dataset input, one
criterion, and a bounded claim scope. Run this command only for trusted sources
and commands. The executor uses argument arrays and does not use a shell.

The installed profile must declare `authored-repro`. The Experiments profile on
macOS, Linux, and Windows is the only profile that declares this capability.
Other profiles and platforms remain observed-only.

The command creates an empty temporary workspace. It runs each setup command
once in order, then verifies every declared input path and digest. It runs the
measurement command for the declared warmup and measurement schedule only when
all inputs match. Setup commands can clone a Git commit, download a fixed
Hugging Face revision, create a package environment, compile code, or perform
another required preparation step.

A file digest covers its bytes. A directory digest covers a canonical sorted
list of its regular files, relative paths, byte sizes, and file digests. It does
not cover timestamps, permissions, or empty directories. Symbolic links are not
valid inputs. A missing, unreadable, or mismatched input produces `UNKNOWN` and
prevents all measurement runs.

The result records the observed minimum, median, and maximum for each metric.
The result passes when every observed median is inside its expected range. The
evidence records each verified input ID and digest. It also records the
operating system, architecture, processor, logical processor count, and memory
size. Memory size is present when the host exposes it through the supported
native route. Measurements use positive integers. The declared unit must
identify any scale such as microseconds or millipercent.

The command deletes the temporary workspace after the operation. It does not
store cloned repositories, downloaded data, model weights, package
environments, or compiled executables as authored experiment evidence. Called
tools can maintain their own host caches outside the temporary workspace.

The command seals a claim that links the definition, capsule, result, selected
dataset, selected criterion, and scope. The definition digest binds the expected
range. The result digest binds the observed values. The claim does not copy
those values or the execution evidence.

The command returns the Repro identity, claim digest, and next `check` command.
It writes no capsule when the observed medians are outside their expected
ranges. A later `check` creates a new workspace and executes the saved recipe on
the checking machine. Both `add` and a single-Repro `check` print the observed
range, median, expected range, and unit for every metric.

## `reproit list`

Show open Repros by default.

```sh
reproit list
reproit list --all
reproit list --kept
reproit list --priority p0
reproit list --assignee USER
```

## `reproit campaign validate <path>`

Parse and validate one TOML campaign file. The command does not contact Cloud or
start a target process.

## `reproit campaign create <path>`

Validate the campaign, create a local signed grant, and start `reproit-fuzzer`.
The fuzzer runs in the campaign workspace. Cloud is not contacted to start or
track the local run.

A production campaign must set `target_environment = "production"` and supply a
separate `REPROIT_FUZZ_PRODUCTION_CAPABILITY` secret. Keep that secret outside
tracked configuration and command arguments.

## `reproit triage <id>`

Change the priority, assignment, or workflow state. Resolving a Repro requires a passing check.

## `reproit debug <id>`

Reproduce the captured Failure in an isolated replay. The command shows the debugger client and a
random local connection address.

## `reproit check <id>`

Run one Repro against the current source. The result is `PASS`, `REGRESSION`,
`UNKNOWN`, or `ERROR`.

## `reproit check`

Run all tracked Repros. The command reports each result and final totals.

## `reproit gate --config <path>`

Run a baseline command and a candidate command. The commands receive suite cases as JSON Lines on
standard input. Each command must return one JSON Lines output record for each completed case.

The configuration uses explicit executables and argument arrays. It does not run a shell command
string. All input and output paths are relative to the configuration file.

```toml
format = "reproit.release-gate-config.v1"
suite_path = "suite.json"
bundle_path = "evidence.json"

[limits]
max_execution_seconds = 300
max_records = 1024
max_stderr_bytes = 1048576
max_stdin_bytes = 41943040
max_stdout_bytes = 16777216

[baseline]
executable = "./run-model"
arguments = ["baseline"]
model_path = "baseline-model.json"

[candidate]
executable = "./run-model"
arguments = ["candidate"]
model_path = "candidate-model.json"
```

The suite uses `reproit.ml-evaluation-suite.v1`. Each model file uses
`reproit.ml-model-identity.v1`.

Each input record has this form:

```json
{"case_id":"configured-color","input_text":"State the configured color."}
```

Each completed output record has this form:

```json
{"case_id":"configured-color","output_text":"blue"}
```

The command writes one content-addressed JSON evidence bundle. The bundle contains the suite,
both ModelRuns, bounded raw outputs, the verdict, the release decision, and digest bindings.

This local bundle is not an independently signed Release Claim. Cloud confirmation must add the
second runner and its authenticated evidence before Repro It creates that Claim.

## `reproit verify <bundle-path>`

Verify a release evidence bundle without Cloud access. The command checks the raw evidence,
ModelRuns, suite, verdict, release decision, and all digest bindings.

## `reproit keep <id>`

Check the current source. After `PASS`, write a tracked reference under `.reproit/repros/`.

## `reproit remove <id>`

Remove the tracked reference from the current repository. Keep the Repro and its Cloud history.

## `reproit mcp`

Serve the bounded Repro operations through standard input and standard output.
The server adds `add_repro` only when an installed profile declares
`authored-repro`. It uses the same application operation as `reproit add`.

## Exit codes

- `0` means that the command succeeded. For `check` and `gate`, the result is `PASS`.
- `1` means that `check` or `gate` found a `REGRESSION`.
- `2` means that the command produced `UNKNOWN` or could not produce a valid result.

Use `--details` to show a stable error code and bounded technical facts. The option keeps the same
result and exit code.
