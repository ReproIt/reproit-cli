# Reproduce and fix a bug

This procedure starts after you have a Repro It account and a backend application in a Git
repository.

## 1. Connect the repository

Run the commands from the repository root:

```sh
reproit login
reproit init
```

Answer the short `init` questions. Select one service, one SDK, the service path, and the normal
application run command. Review the `.reproit/project.toml` change before you accept it.

Follow the printed SDK instructions. The SDK API is framework-neutral. Optional framework adapters
call the same API.

## 2. Deploy the normal application

Build and deploy the application as you normally do. The SDK records only operations that your
application handles. A capture error or service outage must not change application behavior.

Successful operations are not uploaded. An incomplete failure stops before a Cloud request.

## 3. Find the Repro

After managed Repro It verifies the Failure, list open Repros:

```sh
reproit list
```

Copy the short Repro ID from the output.

## 4. Reproduce and inspect the Failure

```sh
reproit debug <id>
```

The command starts an isolated replay of the captured subject and World. It prints a random local
endpoint and the standard debugger client to use. Attach the debugger, then press Enter in the
terminal that runs `reproit debug`.

The debugger endpoint accepts one connection. It closes when the command ends.

## 5. Test the fix

Change the source in your checkout, then run:

```sh
reproit check <id>
```

- `PASS` means that the captured Failure is absent with the changed source.
- `REGRESSION` means that the captured Failure still occurs.
- `ERROR` means that Repro It could not produce an exact result.

Do not treat `ERROR` as a pass.

## 6. Keep the regression check

After the fix passes, run:

```sh
reproit keep <id>
git add .reproit/repros/<id>.toml
git commit
```

The tracked file is a managed reference. It does not contain the captured World, credentials, or
customer data.

Run every tracked check with:

```sh
reproit check
```

Remove a tracked reference with:

```sh
reproit remove <id>
```

This removes only the local tracked reference. It does not delete Cloud history.
