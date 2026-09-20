# Install Repro It

Install `reproit` and `reproit-research` on Linux, macOS, or Windows from the
signed release bundle. Use a source build only for CLI development.

## Verify a release bundle

1. Download the bundle for your operating system and architecture.
2. Download the checksum manifest from the same release.
3. Calculate the SHA-256 checksum for the bundle.
4. Confirm that it matches the manifest.
5. Extract the bundle.
6. Put both executables in a directory on `PATH`.
7. Run `reproit --version`.
8. Run `reproit-research --version`.

Do not run an executable when its checksum does not match.

## Linux and macOS

The release bundle contains `reproit` and `reproit-research`.

```sh
shasum -a 256 reproit-cli-*.tar.gz
tar -xzf reproit-cli-*.tar.gz
install -m 0755 reproit "$HOME/.local/bin/reproit"
install -m 0755 reproit-research "$HOME/.local/bin/reproit-research"
reproit --version
reproit-research --version
```

Use another user-owned directory on `PATH` when `$HOME/.local/bin` is not available.

## Windows

Open PowerShell in the directory that contains the release bundle.

```powershell
Get-FileHash .\reproit-cli-*.zip -Algorithm SHA256
Expand-Archive .\reproit-cli-*.zip -DestinationPath .\reproit-cli
.\reproit-cli\reproit.exe --version
.\reproit-cli\reproit-research.exe --version
```

Move both executables to a user-owned directory on `PATH` after the checksum matches.

## Build from source

Install Git and the Rust version in `rust-toolchain.toml`. Clone this repository into a normal
project directory. Do not build it directly in your home directory.

```sh
git clone https://github.com/ReproIt/reproit-cli.git
cd reproit-cli
cargo install --locked --path crates/reproit-cli
```

The source build does not contain official OAuth metadata. It cannot replace a signed production
release for normal login.

For an authorized test service, set `REPROIT_AUTHORITY` and `REPROIT_CLI_CLIENT_ID` together before
you run `reproit login`. Get the public OAuth authority and client ID from that service's operator.
The command uses the normal browser login and stores the session in the native credential store.
Do not use a client secret as the client ID.
