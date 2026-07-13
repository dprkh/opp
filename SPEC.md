# `opp` Specification

Status: Proposed public preview (`v0.1.0`)

## Goals

`opp` is an independent community CLI that lets local AI agents and other programs use the 1Password CLI through one user-authorized background terminal on macOS. It must:

- retain a stable terminal identity across independent client processes;
- use the default 1Password account unless the caller selects an account;
- manage account authorization automatically within one shared broker;
- detect unusable authorization without prompting during agent commands;
- stop automated access and instruct the caller when user reauthorization is required;
- preserve raw, non-interactive `op` input, output, exit, mutation, and `op run` behavior; and
- keep broker startup, interactive authorization, and shutdown under user control.

The common path is:

```sh
opp start
opp exec -- vault list --format=json
```

To use a specific account, pass the same selector when starting and executing:

```sh
opp start --account work.1password.com
opp exec --account work.1password.com -- item get Example --format=json
```

The broker lasts until it is stopped, fails, the user logs out, or the Mac reboots. It retains one terminal and tracks 1Password authorization separately for each account selector. Individual grants remain subject to 1Password's 10-minute inactivity rule, 12-hour hard limit, and immediate revocation when 1Password locks.

The public preview targets one local macOS user and one shared local broker. Cross-platform support, remote clients, LaunchAgents, automatic restart, service accounts, Connect, and unattended reauthorization are outside this specification. `opp` is not affiliated with, endorsed by, or supported by 1Password or OpenAI.

## Supported environment and releases

`opp` supports macOS 12 Monterey or later on `arm64` and `amd64`. It requires the official signed 1Password CLI and the 1Password desktop app with CLI integration enabled.

The `v0.1.0` GitHub Release contains:

```text
opp_0.1.0_darwin_arm64.tar.gz
opp_0.1.0_darwin_amd64.tar.gz
SHA256SUMS
```

Each archive contains one executable named `opp`. `SHA256SUMS` covers both archives. Preview binaries are not signed or notarized by Apple; the checksum detects a changed download but does not authenticate the publisher independently of GitHub. macOS may therefore require the user to approve the binary in Privacy & Security before first use.

Versions follow Semantic Versioning with a `v`-prefixed Git tag. Within `0.x`, patch releases preserve the documented CLI, JSON, exit-status, and client-broker contract; a minor release may change it. Release binaries write their version and a trailing newline exactly as:

```text
opp 0.1.0
```

`opp` has no automatic updater or release-check network request. To upgrade, the user stops the broker and replaces the executable:

```sh
opp stop
```

A client that encounters an incompatible broker must fail without sending a normal request and instruct the user to stop and restart the broker. The version-independent stop handshake remains available across protocol versions.

## Public API

### Commands

```text
opp --help
opp --version
opp start [--account ACCOUNT] [--op ABSOLUTE_PATH]
opp status [--account ACCOUNT]
opp stop
opp exec [--account ACCOUNT] [--timeout DURATION] -- [OP_ARGUMENT...]
```

There are no command aliases. `--help` is accepted at the root and individual command levels; it writes help to standard output and exits `0`. `--version` writes the release version to standard output and exits `0`.

Lifecycle commands return `0` on success, `1` for an operational failure, and `2` for invalid usage. Human diagnostics go to standard error and never contain child output. Invalid usage and help do not connect to the broker.

### Account selection

`start`, `status`, and `exec` select the 1Password account in this order:

1. a non-empty `--account` value;
2. a non-empty `OP_ACCOUNT` in the invoking environment; or
3. the default account selected by `op`.

An explicit selected string is the `account_selector`. Account selectors must be valid UTF-8; a non-UTF-8 `--account` or `OP_ACCOUNT` value is invalid usage. When neither input supplies one, the selector is absent and `op` chooses its default account. The broker keeps a separate authorization record for the absent selector and for every exact selector string. It performs no account lookup, alias normalization, case folding, or equivalence detection.

### `start`

If `--op` is omitted, `opp` resolves `op` from the invoking user's `PATH`. An explicit `--op` value must be absolute. In both cases, `opp` resolves symlinks, requires the canonical target to be a regular executable with a UTF-8 path, and pins its absolute path for the broker lifetime without searching again. A canonical non-UTF-8 path is an operational failure.

When the broker is not running, `start` writes this warning before creating it:

```text
opp: warning: the broker gives every process that can reach its socket the full authorized 1Password CLI authority for every account added to it, including same-user command execution through 'op run'.
```

It creates the detached broker, waits up to five seconds for its socket and retained terminal, and immediately starts explicit authorization for the selected account. The 1Password system prompt is the only consent interaction; `opp` adds no confirmation prompt or acknowledgement flag.

If the broker already exists, `start` succeeds immediately when the selected account is active. When that account is unknown or requires reauthorization, it writes the warning and starts explicit authorization. This lets one broker acquire authorization for multiple accounts through repeated user-run `start` commands.

A running broker with a different canonical `op` path is an error and must be stopped explicitly. Failed or cancelled authorization leaves the broker running and the selected account in `reauthorization_required` so the user can retry.

### `status`

`status` reports only the selected account. It never invokes `op`, refreshes authorization, or displays an authorization prompt. It writes one compact JSON object followed by a newline and no other standard output. `schema_version` is the integer `1`; consumers must ignore unrecognized object members.

A stopped broker is:

```json
{"schema_version":1,"running":false}
```

An active selected account is equivalent to this formatted example:

```json
{
  "schema_version": 1,
  "running": true,
  "authorization": "active",
  "account_selector": "work.1password.com",
  "op_path": "/opt/homebrew/bin/op",
  "broker_version": "0.1.0",
  "started_at": "2026-07-12T20:00:00Z",
  "authorized_at": "2026-07-12T20:01:00Z",
  "hard_expires_at": "2026-07-13T08:01:00Z",
  "next_probe_at": "2026-07-12T20:06:00Z"
}
```

`authorization` is `active` or `reauthorization_required`. An unknown selected account is `reauthorization_required`. `account_selector` is present only for an explicit selector. The three authorization timestamps are present only while active. Times are UTC RFC 3339 values. `active` means the latest explicit authorization or automatic check succeeded and the tracked hard limit has not passed; it does not prove that authorization remained usable afterward or identify the 1Password app's current lock state.

A status or protocol failure returns exit `1`, writes no JSON, and emits a diagnostic.

### `stop`

`stop` stops accepting clients, cancels queued work, terminates active process groups, closes the retained terminal, removes the socket, clears all authorization records, and exits the broker. Stopping an absent broker succeeds.

### `exec`

`exec` requires the `--` separator. Arguments after it are passed unchanged after the `op` executable name and may be empty. They accept at most 256 elements and 65,536 total bytes; an element must not contain NUL.

Before connecting to the broker, the client scans arguments through, but not including, the first inner argument equal to `--`. It rejects an argument equal to `--account` or beginning with `--account=` and exits `2`. This prevents a proxied `op` command from overriding the account selected by `opp`; arguments after the inner `--` belong to an `op` child command and remain unchanged.

`--timeout` defaults to `2m` and accepts Go `time.ParseDuration` syntax whose value is from `1s` through `10m`, inclusive. It limits the requested command only; authorization checks have separate limits.

The client sends its absolute current directory and environment to the broker. The broker connects the command to three non-terminal pipes, and the client copies raw binary standard input, output, and error with backpressure. There is no application-output encoding, capture, or size limit.

`exec` does not start a missing broker or perform explicit authorization. A missing broker, unknown selected account, expired grant, or failed automatic check returns exit `77`, empty standard output, and this exact standard-error text:

```text
opp: the selected 1Password account requires authorization. Stop and inform the user. Wait for the user to run 'opp start' with the same account selection and confirm completion. Do not retry or run 'opp start' yourself.
```

Other command completion maps to the client exit status:

- normal `op` exit: the same exit code;
- signal: `128` plus the signal number;
- timeout: `124`;
- caller signal: `128` plus that signal number;
- broker, protocol, or process-start failure: `125`; and
- invalid `opp exec` usage: `2`.

Operational statuses may overlap a native `op` exit code and are distinguished by an `opp:` diagnostic.

## Behavior

### Broker and transport

One detached broker owns one controlling pseudoterminal, one lock, one Unix socket, all account authorization records, and one FIFO executor. A request's automatic check and requested command are indivisible relative to every other request, regardless of account.

The effective user's account home, obtained independently of client `HOME`, determines the runtime paths:

```text
~/Library/Caches/opp/run/broker.lock
~/Library/Caches/opp/run/broker.sock
```

The runtime directory has mode `0700`. The lock and socket have mode `0600`. The broker verifies Darwin peer credentials and accepts only its own effective user ID. It exposes no TCP or HTTP listener.

The broker holds its lock for its lifetime. After acquiring it, startup may remove a stale socket path only when `lstat` identifies a same-user Unix socket. A symlink or any other file type is an error.

The broker owns and drains one controlling pseudoterminal. Every `op` child inherits it while file descriptors `0`, `1`, and `2` remain non-terminal pipes. The child owns a new process group that temporarily becomes the terminal foreground group. Foreground-group switching is part of the global serialization boundary.

Connections use a bounded, versioned control protocol. An `exec` client passes three pipe descriptors with `SCM_RIGHTS` and keeps its control connection open until completion. Disconnecting cancels only that queued or active request. Queued cancellation starts neither an automatic check nor a command. Broker shutdown owns the final executor close.

The stop operation has a permanent version-independent prefix accepted before normal protocol negotiation. Other incompatible versions require an explicit stop and restart.

### Authorization

A new broker has no active account records. Explicit authorization uses:

```text
op vault list --format=json
```

Probe output is discarded and never retained. Explicit authorization is initiated only by `start`, may display the 1Password system prompt, and has a 120-second limit. It succeeds only when the command exits `0`. `authorized_at` is the probe start time, and `hard_expires_at` is exactly 12 hours later.

For each active account, the broker runs an automatic check before every requested command for that account. It also runs a maintenance check five minutes after that account's latest successful explicit authorization or automatic check when no request for the same account has already refreshed it. Maintenance for different accounts remains serialized by the shared executor. Each automatic check has one five-second wall-clock limit across these ordered commands:

```text
op whoami
op vault list --format=json
```

The broker runs `vault list` only when `whoami` exits `0`. Both commands' output is discarded. The check succeeds only when both commands exit `0` within the combined limit. `whoami` is the prompt-free guard for authorization that is already unavailable; `vault list` validates and refreshes a grant that was usable when checked.

Any automatic-check failure or timeout changes every account record to `reauthorization_required`, suspends maintenance, and returns exit `77` to the affected request. The broker does not parse command output or distinguish lock, expiration, revocation, app failure, and other causes. Queued requests re-check state before running anything. Unlocking 1Password alone does not reactivate an account; the user must run `start` again.

At an account's `hard_expires_at`, the broker changes that record to `reauthorization_required` without probing. A running command may finish, but no later check or command starts for that account until explicit authorization succeeds.

Locking 1Password revokes prior CLI grants. There is no documented API for reading the desktop app's precise lock state. A nonzero `whoami` result reliably classifies the selected authorization as unusable in the qualified environment, but it does not identify why. A lock after `whoami` succeeds but before `vault list` or the requested command is an unavoidable race and may produce native prompt or timeout behavior.

`opp` must not infer lock state by parsing private diagnostic logs, reading internal settings or databases, automating the 1Password UI, using hidden environment variables, or calling private signed XPC interfaces. Those mechanisms are undocumented, inaccessible, incomplete, or unstable.

When `account_selector` is present, the broker forces it through `OP_ACCOUNT` for authorization checks and requested commands. Otherwise it removes `OP_ACCOUNT` and lets `op` choose its default. The broker applies this rule per operation rather than inheriting its startup environment.

For every child, the broker starts from the client environment, forces `OP_BIOMETRIC_UNLOCK_ENABLED=true`, removes `OP_SESSION` and `OP_SESSION_*`, removes `OP_SERVICE_ACCOUNT_TOKEN`, `OP_CONNECT_HOST`, and `OP_CONNECT_TOKEN`, and removes every `OPP_*` variable. The `OPP_` namespace is reserved for `opp`.

A maintenance check has no client context and retains none. It uses the effective user's home as its working directory and a derived environment containing `HOME`, `USER`, `LOGNAME`, the system default `PATH`, the Darwin user temporary directory when available, `OP_BIOMETRIC_UNLOCK_ENABLED=true`, and the selected `OP_ACCOUNT` when present. It contains no other variables.

### Process execution

The broker executes the canonical `op` path directly with the requested argument vector and working directory. It performs no shell invocation, expansion, interpolation, rewriting, or command classification.

Each child owns a new process group. Cancellation, timeout, automatic-check failure, or broker shutdown sends `SIGTERM` to the group, waits up to two seconds, then sends `SIGKILL` if it remains. Natural exit is reported only after the process is reaped and both output pipes close.

Short-lived `op run` processes are supported. Interactive terminal streaming and processes intended to outlive an `exec` request are not.

## Security and agent use

Every process that can reach the shared socket receives the broker's complete authorized `op` authority for every account added to it. File permissions and peer credentials exclude other macOS users but do not isolate processes running as the same user. Account selectors are routing keys, not security boundaries.

The command surface is intentionally unrestricted. The broker executes outside the caller's sandbox with the caller's requested working directory and environment. In particular, `opp exec -- run -- COMMAND` permits arbitrary same-user command execution. Allowing a sandboxed agent to reach the socket is a deliberate local sandbox escape.

Automatic checks deliberately defeat 1Password's 10-minute inactivity safeguard. They cannot extend the 12-hour hard limit or survive app lock. The retained terminal follows 1Password's documented identity inputs but is not a documented daemon integration, so real-app compatibility is release-gating.

`opp` must not persist or log credentials, environments, arguments, working directories, standard streams, probe data, command results, or account selectors. It may retain lifecycle timestamps, exact account selectors, and state categories in broker memory. It performs no telemetry or update checks. In-memory values and caller transcripts are not securely zeroized. The proxied `op` process retains its documented network and 1Password activity-log behavior.

Agents use `opp` through their normal command-execution facility:

```sh
opp exec --account work.1password.com -- item get Example --format=json
```

An agent may inspect `status` for its selected account when its runtime permits access to the socket. It must not invoke `start` or `stop`. On exit `77` or the fixed authorization diagnostic, it must stop, inform the user, and wait for confirmation. Agent instructions should also prohibit mutations and `op run` unless the user's current request explicitly requires them; `opp` does not enforce that policy.

For current Codex permission profiles, a user can extend the workspace policy and allow the one absolute broker socket:

```toml
default_permissions = "opp"

[permissions.opp]
description = "Workspace access plus the shared opp broker."
extends = ":workspace"

[permissions.opp.network]
enabled = true

[permissions.opp.network.unix_sockets]
"/Users/alice/Library/Caches/opp/run/broker.sock" = "allow"
```

With no domain allow entries, sandboxed command network requests remain blocked. The user must replace the home path with the exact value. Codex permission profiles are beta and do not compose with the older `sandbox_mode` settings; users must follow the documentation for their installed Codex version. `opp` must not recommend `:danger-full-access` or `--dangerously-bypass-approvals-and-sandbox`. The socket permission still grants all broker behavior, including every authorized account and execution outside Codex's filesystem sandbox.

Users who need unattended, remotely managed, or vault-limited authority should use a supported 1Password Service Account. Users whose task fits 1Password Environments without exposing raw secrets to the agent should prefer the official 1Password MCP Server.

## Acceptance

Automated tests with a fixture executable and controllable time must verify:

- root and command help, exact release version, `PATH` discovery, explicit `--op`, account precedence, exact-selector records, and conflicting broker starts;
- `schema_version: 1` status output, optional fields, stopped state, selected-account-only state, and invalid JSON suppression on failure;
- one stable terminal identity across every account, global FIFO execution, and indivisible automatic-check-plus-command execution;
- exact arguments, downstream account-override rejection before an inner `--`, preserved child arguments after it, cwd, sanitized environment, binary stdin, and simultaneous raw stdout and stderr;
- cancellation before and during automatic checks, cancellation during commands, timeout, signals, native exit mapping, process-group escalation, and descendants;
- independent explicit authorization records, per-command checks, five-minute maintenance, the combined five-second limit, global invalidation after any automatic failure, exact 12-hour cutoff, exit `77`, and user-driven recovery;
- effective-user runtime paths despite a changed client `HOME`, directory and socket permissions, peer-user rejection, descriptor passing, singleton startup races, stale-socket safety, version mismatch, and idempotent stop; and
- absence of request, environment, account-selector, probe, and stream canaries from diagnostics and runtime files.

Release validation must verify that both archives run on their matching architecture on macOS 12 or later, contain one `opp` executable, report the tagged version, and match `SHA256SUMS`. A fresh downloaded artifact must be tested through macOS's unsigned-software Gatekeeper flow.

An integration test against a supported Codex release must verify that a workspace-derived permission profile can connect only to the configured broker socket. The test must also demonstrate that work delegated through the broker is outside the Codex filesystem sandbox and can reach every account authorized in that broker.

A manual test with the real signed `op` and 1Password desktop app must verify:

1. `start` writes the full-authority warning, immediately displays one authorization prompt, and makes the default account active.
2. Independent clients reuse the retained terminal without prompting.
3. Repeating `start` for two explicit account selectors records both accounts while retaining the same terminal identity.
4. More than ten minutes without client traffic remains authorized through maintenance.
5. Locking 1Password makes `whoami` fail within the automatic-check limit without displaying an authorization prompt; the affected `exec` returns `77`, all account records become `reauthorization_required`, and there is no retry.
6. Unlocking the app alone does not restore access; user-run `start` restores only the selected account.
7. The 12-hour limit requires user-run `start` for the expired account.
8. Stop followed by start creates a new terminal identity and prompts again.
9. A disposable secret read, non-destructive JSON command, mutation in a disposable vault, and short `op run` preserve native streams and exit behavior.

The prompt-free lock test was observed with 1Password CLI `2.34.1` and 1Password for Mac `8.12.26`. Every release must repeat it against each supported CLI/app pair. A prompt, hang, or successful `whoami` after confirmed lock blocks publication because 1Password does not document prompt-free lock detection as a stable API.

Failure of terminal reuse, account routing, prompt cancellation, raw stream behavior, post-lock fail-closed behavior, shared-socket security validation, or release checksum validation blocks publication.

## Sources

- [1Password app-integration security](https://www.1password.dev/cli/app-integration-security) defines per-terminal and per-account authorization, refresh on use, 10-minute inactivity, the 12-hour hard limit, app-lock revocation, and macOS terminal identity.
- [`op whoami`](https://www.1password.dev/cli/reference/commands/whoami) returns the active account and errors when no account is authenticated; `opp` uses that documented result only to classify authorization usability.
- [1Password app integration](https://www.1password.dev/cli/app-integration) and [multiple-account selection](https://www.1password.dev/cli/use-multiple-accounts) define setup, `--account`, and `OP_ACCOUNT` behavior.
- [1Password CLI release notes](https://app-updates.agilebits.com/product_history/CLI2) and [1Password for Mac releases](https://releases.1password.com/mac/stable/) identify the initially qualified versions; later versions require the same release gate.
- [1Password CLI reference](https://www.1password.dev/cli/reference) defines the proxied command surface; [`op run`](https://www.1password.dev/cli/reference/commands/run) establishes arbitrary child execution.
- [1Password Service Accounts](https://www.1password.dev/service-accounts) document the supported unattended and vault-limited identity excluded here.
- [1Password MCP Server](https://www.1password.dev/environments/mcp-server) documents the Environments-based alternative that avoids returning raw secrets to an agent.
- [1Password community-project disclaimer](https://www.1password.dev/community/disclaimer) establishes that third-party projects are not endorsed or supported by 1Password.
- [Codex permissions](https://developers.openai.com/codex/permissions) documents workspace-derived profiles and exact Unix-socket allowlists as explicit local escape hatches.
- [Apple Gatekeeper guidance](https://support.apple.com/en-us/102445) documents the warnings and manual approval applicable to unsigned, unnotarized downloads.
- [Semantic Versioning](https://semver.org/) supplies the release-versioning model.
- [Go `time.ParseDuration`](https://pkg.go.dev/time#ParseDuration) defines the accepted `--timeout` duration syntax; `opp` applies its own narrower value range.
