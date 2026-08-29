# Fix a production bug

Start with a Repro It account and a backend application in Git.

## 1. Connect the application

Run these commands from the repository root:

```sh
reproit login
reproit init
```

Select the service, SDK, service path, and direct application command. For Go,
Repro It compiles the selected package and inspects the temporary binary. It does
not run the binary. For Node.js and Python, the SDK exits from an internal probe
before application code runs. For .NET and Rust, the SDK verifies its packaged
native sentinel and exits during startup. Repro It writes `.reproit/project.toml`
only after verification succeeds.

The current probe supports .NET, Go, Node.js, Python, and Rust.

For a Go service, use its normal direct `go run` command. Repro It stores the
required internal build instrumentation. The later workflow still uses the same
`reproit debug` and `reproit check` commands.

For .NET and Rust services, use direct `dotnet run` and `cargo run` commands.
Repro It does not add a language-specific public command.

If support is absent, `reproit init` stops without a configuration change. Install a released SDK
with complete automatic capture before you continue.

## 2. Deploy capture

Store `REPROIT_MANAGED_PROJECT_TOKEN` in the deployment secret store. Deploy the application with
the normal release process. The SDK loads the project file and Git revision automatically.

Trigger the production bug. Repro It shows the Failure only after it verifies an exact replay.

## 3. Reproduce the Failure

```sh
reproit list
reproit debug <id>
```

`list` shows the Repro ID. `debug` starts an isolated replay and shows the debugger client and local
connection address.

Attach the debugger. Press Enter in the `reproit debug` terminal when you finish.

## 4. Test the fix

Change the source. Then run:

```sh
reproit check <id>
```

- `PASS` means that the captured Failure is absent.
- `REGRESSION` means that the captured Failure still occurs.
- `ERROR` means that Repro It could not produce an exact result.

Treat `ERROR` as an unresolved check.

## 5. Keep the check

After the fix passes, run:

```sh
reproit keep <id>
git add .reproit/repros/<id>.toml
git commit
```

Run all tracked Repros with:

```sh
reproit check
```

Remove one tracked reference with `reproit remove <id>`. This action keeps the Cloud history.

## Coding agents

Run `reproit mcp` to give a coding agent the same bounded operations. The MCP server uses the same
login and authorization as the human CLI.
