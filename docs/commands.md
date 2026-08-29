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

## `reproit list`

Show open Repros by default.

```sh
reproit list
reproit list --all
reproit list --kept
reproit list --priority p0
reproit list --assignee USER
```

## `reproit triage <id>`

Change the priority, assignment, or workflow state. Resolving a Repro requires a passing check.

## `reproit debug <id>`

Reproduce the captured Failure in an isolated replay. The command shows the debugger client and a
random local connection address.

## `reproit check <id>`

Run one Repro against the current source. The result is `PASS`, `REGRESSION`, or `ERROR`.

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

Serve seven bounded Repro operations to coding agents through standard input and standard output.
Use the same login, authorization, and application operations as the human commands.

## Exit codes

- `0` means that the command succeeded. For `check` and `gate`, the result is `PASS`.
- `1` means that `check` or `gate` found a `REGRESSION`.
- `2` means that the command produced `UNKNOWN` or could not produce a valid result.

Use `--details` to show a stable error code and bounded technical facts. The option keeps the same
result and exit code.
