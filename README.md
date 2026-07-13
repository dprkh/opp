# opp

`opp` is a macOS-only local broker for user-authorized 1Password CLI access. It retains one background terminal so independent local processes can reuse authorization without automating the 1Password UI.

This is a proposed public preview. Read [SPEC.md](SPEC.md) before using it, especially the shared-socket and `op run` security model.

## Build

The pinned Rust toolchain builds both Apple architectures and targets macOS 12 or later:

```sh
cargo build --release --locked --no-default-features --bin opp
```

## Use

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

## Test

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets --all-features
cargo test --locked --all-targets --all-features -- --test-threads=1
```

The `test-support` feature and `opp-test-op` binary exist only for isolated automated broker tests. Release builds disable that feature.
