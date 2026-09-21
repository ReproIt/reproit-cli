$ErrorActionPreference = "Stop"

$repositoryRoot = Split-Path -Parent $MyInvocation.MyCommand.Path

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw "Cargo is required. Install Rust, then run this script again."
}

& cargo install `
    --locked `
    --force `
    --path (Join-Path $repositoryRoot "crates/reproit-cli") `
    --bin reproit

if ($LASTEXITCODE -ne 0) {
    throw "The reproit installation failed."
}
