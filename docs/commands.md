# Command reference

## `reproit login`

Sign in through the browser. The CLI stores the session in the native credential
store.

## `reproit init`

Check for complete automatic World capture support. The current probe supports
.NET, Go, Node.js, Python, and Rust. The command stops before it writes
`.reproit/project.toml` when the required proof is absent.

Run the command again to change the current setup. The command shows the file
change before it writes the file.

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

## `reproit triage REPRO_ID`

Change the priority, assignment, or workflow state. Resolving a Repro requires a
passing check.

## `reproit debug REPRO_ID`

Reproduce the captured Failure in an isolated replay. The command shows the
debugger client and a random local connection address.

## `reproit check REPRO_ID`

Run one Repro against the current source. The result is `PASS`, `REGRESSION`,
`UNKNOWN`, or `ERROR`.

For Rust projects that use `cargo run`, install Cargo and commit `Cargo.lock`.
The checkout must be clean. The CLI prepares locked dependencies for Linux and
sends them with the committed source. The worker builds that source offline in
the isolated replay environment.

Configure private registry access in your user Cargo configuration. Dependency
preparation does not load checkout Cargo configuration on the developer host.
The isolated build can use the committed checkout configuration.

## `reproit check`

Run all tracked Repros. The command reports each result and final totals.

## `reproit keep REPRO_ID`

Check the current source. After `PASS`, write a tracked reference under
`.reproit/repros/`.

## `reproit remove REPRO_ID`

Remove the tracked reference from the current repository. Keep the Repro and
its Cloud history.

## `reproit mcp`

Serve the bounded Repro operations through standard input and standard output.
The server exposes only operations allowed by the installed profile.

## Exit codes

- `0` means that the command succeeded. For `check`, the result is `PASS`.
- `1` means that `check` found a `REGRESSION`.
- `2` means that the command produced `UNKNOWN` or no valid result.

Use `--details` to show a stable error code and bounded technical facts. The
output contains bounded diagnostic lines and does not print raw command output.

If `.reproit/project.toml` is missing, run `reproit init` from the application
repository root. If the file is invalid, restore a valid project file.
