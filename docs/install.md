# Install Repro It

## Requirements

Install Git and the Rust toolchain from [rustup.rs](https://rustup.rs). Your Git
credentials must have access to the pinned Repro It dependencies.

The installer uses `Cargo.lock`. It installs only the `reproit` executable.

## Run the installer

### Linux and macOS

Run:

```sh
git clone https://github.com/ReproIt/reproit-cli.git
cd reproit-cli
./install.sh
reproit --version
```

Cargo installs the executable in `$CARGO_HOME/bin`. The default location is
`$HOME/.cargo/bin`. Add that directory to `PATH` when necessary.

### Windows

Run these commands in PowerShell:

```powershell
git clone https://github.com/ReproIt/reproit-cli.git
cd reproit-cli
.\install.ps1
reproit --version
```

Cargo installs the executable in `$env:CARGO_HOME\bin`. The default location is
`$env:USERPROFILE\.cargo\bin`. Add that directory to `PATH` when necessary.

## Build from source

Run:

```sh
git clone https://github.com/ReproIt/reproit-cli.git
cd reproit-cli
cargo build --locked --release --package reproit-cli --bin reproit
```

The executable is at `target/release/reproit` on Linux and macOS. Windows uses
`target\release\reproit.exe`.

## Login configuration

A source build does not contain the official OAuth metadata. For an authorized
test service, set `REPROIT_AUTHORITY` and `REPROIT_CLI_CLIENT_ID` before login.
Get the public OAuth authority and client ID from the service operator.

Do not use a client secret as the client ID.
