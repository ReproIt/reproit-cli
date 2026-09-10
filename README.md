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
exits from an internal probe before application code runs. For .NET and Rust, the
SDK verifies its packaged native sentinel and exits during startup. If the exact
proof is absent, the command stops before it writes `.reproit/project.toml`.

The current probe supports direct .NET, Go, Node.js, Python, and Rust application commands.
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

`PASS` means that the captured Failure is absent. `REGRESSION` means that it
still occurs. `UNKNOWN` means that the evidence cannot support a decision.
`ERROR` means that Repro It could not complete or verify the operation.

`reproit mcp` serves MCP through standard input and standard output. It uses the same login,
authorization, and application operations as the human commands.

## Add an authored experiment

Use `add` on macOS, Linux, or Windows when the installed Experiments profile
declares `authored-repro`:

```sh
reproit add experiment.json
reproit check <id>
```

`add` validates a strict research recipe and executes it in a new temporary
workspace. Setup commands can acquire exact source, data, and model revisions.
After setup, Repro It verifies each declared input digest. It runs the
measurement command only when every input matches. Every command uses an
argument array and does not use a shell.

```json
{
  "claim": {
    "criterion_id": "accuracy_millipercent",
    "dataset_input_id": "dataset",
    "scope": "The declared model evaluated on the declared dataset."
  },
  "criteria": [{
    "expected": {"maximum": 86000, "minimum": 84000},
    "id": "accuracy_millipercent",
    "unit": "millipercent"
  }],
  "format": "reproit.research-recipe.v1",
  "inputs": [{
    "digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    "id": "dataset",
    "path": "research/data",
    "role": "dataset"
  }, {
    "digest": "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    "id": "model",
    "path": "research/models/example",
    "role": "model"
  }],
  "profile": "experiments",
  "run": {
    "command": {
      "arguments": ["run", "python", "evaluate.py", "--reproit-jsonl"],
      "program": "uv",
      "working_directory": "research"
    },
    "measured_runs": 7,
    "warmup_runs": 1
  },
  "setup": [{
    "arguments": [
      "clone",
      "https://example.com/research/model.git",
      "research"
    ],
    "program": "git"
  }, {
    "arguments": [
      "checkout",
      "--detach",
      "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    ],
    "program": "git",
    "working_directory": "research"
  }, {
    "arguments": [
      "download",
      "example/model",
      "--revision",
      "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
      "--local-dir",
      "research/models/example"
    ],
    "program": "hf"
  }]
}
```

A setup command can use `git`, `hf`, a package manager, a compiler, or another
required tool. The run command can use `python`, `uv run`, `cargo run`, or a
repository-relative program. Use `{exe}` where Windows needs the `.exe` suffix.

An input path is relative to the temporary workspace. A file digest is the
SHA-256 digest of its bytes. A directory digest is the SHA-256 digest of a
canonical `reproit.input-directory.v1` record. That record contains the sorted
relative path, byte size, and file digest of every regular file. Directory
metadata, timestamps, permissions, and empty directories are not part of the
digest. Repro It returns `UNKNOWN` when an input is absent, differs from its
declared digest, contains a symbolic link, or cannot be read.

Each measured run emits one JSON line per criterion. Repro It records the
observed minimum, median, and maximum. A result passes when each observed
median is inside its inclusive expected range. This allows one recipe to
describe realistic results across different hardware without claiming that
every machine produces one exact value. Measurements use positive integers, so
the unit must state any scale such as microseconds or millipercent.

The claim selects one dataset input, one criterion, and a bounded scope. Repro
It seals the claim after it verifies the definition, capsule, evidence, and
result. The claim links those objects by digest. It does not copy commands,
measurements, ranges, inputs, or environment evidence. The CLI and MCP add
results return the claim digest.

Repro It deletes the temporary workspace after the operation. It does not
store cloned repositories, downloaded data, model weights, package
environments, or compiled executables as authored evidence. Called tools can
still maintain their own host caches outside the temporary workspace.

The MCP server advertises `add_repro` only when the same capability is present.
Both surfaces call the same application operation.

## Run a distributed fuzz campaign

Validate a campaign without Cloud or target access:

```sh
reproit campaign validate campaign.toml
```

Create a local campaign and start the customer-side fuzzer:

```sh
reproit campaign create campaign.toml
```

The fuzzer sends eligible captures through the normal Repro It capture path.
Cloud stores the existing Repro and labels it as fuzz discovered. The CLI does
not create or track a Cloud campaign session.

Read the [quick start](docs/quick-start.md) for the full bug-fix loop. Use the
[command reference](docs/commands.md) for options and exit codes.

## Develop the CLI

Run the complete repository check:

```sh
./tools/test.sh
```

Read [CONTRIBUTING.md](CONTRIBUTING.md) before you change public command behavior.

The release-gate integration pins Experiments and ML to exact Git revisions.
Source builds do not require adjacent checkouts of either repository.
