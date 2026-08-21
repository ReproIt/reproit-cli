# Install Repro It

Use an official release bundle for normal use. Use a source build only for CLI development.

## Verify a release bundle

1. Download the bundle for your operating system and architecture.
2. Download the checksum manifest from the same release.
3. Calculate the SHA-256 checksum for the bundle.
4. Confirm that it matches the manifest.
5. Extract the bundle.
6. Put the `reproit` executable in a directory on `PATH`.
7. Run `reproit --version`.

Do not run an executable when its checksum does not match.

## Linux and macOS

The release bundle contains one `reproit` executable.

```sh
shasum -a 256 reproit-cli-*.tar.gz
tar -xzf reproit-cli-*.tar.gz
install -m 0755 reproit "$HOME/.local/bin/reproit"
reproit --version
```

Use another user-owned directory on `PATH` when `$HOME/.local/bin` is not available.

## Windows

Open PowerShell in the directory that contains the release bundle.

```powershell
Get-FileHash .\reproit-cli-*.zip -Algorithm SHA256
Expand-Archive .\reproit-cli-*.zip -DestinationPath .\reproit-cli
.\reproit-cli\reproit.exe --version
```

Move `reproit.exe` to a user-owned directory on `PATH` after the checksum matches.

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
