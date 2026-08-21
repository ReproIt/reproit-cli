# Command reference

## `reproit login`

Starts browser login with authorization code, PKCE S256, state validation, and a loopback callback.
The CLI stores the resulting session in the native credential store. The command never prints the
session token.

## `reproit init`

Connects the current Git repository to one managed service and SDK. Interactive mode asks only for
missing choices. Non-interactive mode uses the same operation:

```sh
reproit init --non-interactive --service NAME --sdk rust --service-path . -- COMMAND ARGUMENT
```

Run `reproit init` again to edit an existing project binding. The command shows the tracked-file
change and asks for one confirmation.

## `reproit list`

Lists open managed Repros by default.

```sh
reproit list
reproit list --all
reproit list --kept
reproit list --priority p0
reproit list --assignee USER
```

## `reproit triage <id>`

Changes priority, assignment, or workflow state. Resolving a Repro first runs the changed source and
requires `PASS`.

## `reproit debug <id>`

Runs the captured subject and World in an isolated replay. It prints a random loopback endpoint and
the compatible debugger client. The debugger capability is never printed or sent to the standard
debugger client.

## `reproit check <id>`

Runs one Repro against the current source. The command uses an exact compatible replay host. It does
not use processor translation.

## `reproit check`

Runs every tracked managed reference. The command reports every finished result and then prints
totals.

## `reproit keep <id>`

Checks the changed source, creates an immutable managed reference, and writes it under
`.reproit/repros/`.

## `reproit remove <id>`

Removes the tracked reference from the current repository. It does not delete the Repro from Cloud.

## Exit codes

- `0` means that the command succeeded. For `check`, every evaluated Repro passed.
- `1` means that `check` found a regression.
- `2` means that the command could not produce a valid result.

Use `--details` for a stable error code and bounded technical facts. It does not change the command,
result, or exit code.
