`opp` is a macOS-only local broker for user-authorized 1Password CLI access. It retains one background terminal so independent local processes can reuse authorization without automating the 1Password UI.

### Install or upgrade

Install the latest release to `~/.local/bin/opp`, or run the same command again to replace an older version:

```sh
curl -L --proto '=https' --tlsv1.2 -sSf https://github.com/dprkh/opp/releases/latest/download/install.sh | sh
```

The installer verifies the release checksum and stops a running broker before replacing the executable. Add `~/.local/bin` to `PATH` if the installer reports that it is missing.

### Build

The pinned Rust toolchain builds both Apple architectures and targets macOS 12 or later:

```sh
cargo build --release --locked --no-default-features --bin opp
```

### Use

Authorize from a user-controlled terminal, then run non-interactive `op` commands through the broker:

```sh
opp start
opp exec -- vault list --format=json
opp status
opp stop
```

Select an account consistently on both authorization and execution:

```sh
opp start --account work.1password.com
opp exec --account work.1password.com -- item get Example --format=json
```

Every same-user process that can reach the broker socket receives all authority held by the broker, including arbitrary same-user execution through `op run`. Account selectors are routing keys, not security boundaries.

