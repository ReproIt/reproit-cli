# Command reference

Use these commands from an application repository.

## `reproit login`

Sign in through the browser. The CLI stores the session in the native credential store.

## `reproit init`

Connect the current repository to one service and SDK. The command writes
`.reproit/project.toml` and prints the SDK setup.

Run it again to change the current setup. The command shows the file change before it writes it.

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

## `reproit keep <id>`

Check the current source. After `PASS`, write a tracked reference under `.reproit/repros/`.

## `reproit remove <id>`

Remove the tracked reference from the current repository. Keep the Repro and its Cloud history.

## Exit codes

- `0` means that the command succeeded. For `check`, all evaluated Repros passed.
- `1` means that `check` found a regression.
- `2` means that the command could not produce a valid result.

Use `--details` to show a stable error code and bounded technical facts. The option keeps the same
result and exit code.
