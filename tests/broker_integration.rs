use serde::Deserialize;
use serde_json::Value;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const WARNING: &str = "opp: warning: the broker gives every process that can reach its socket the full authorized 1Password CLI authority for every account added to it, including same-user command execution through 'op run'.\n";
const AUTHORIZATION_REQUIRED: &str = "opp: the selected 1Password account requires authorization. Stop and inform the user. Wait for the user to run 'opp start' with the same account selection and confirm completion. Do not retry or run 'opp start' yourself.\n";

#[derive(Debug, Deserialize)]
struct Invocation {
    arguments_hex: Vec<String>,
    cwd_hex: String,
    environment_keys_hex: Vec<String>,
    op_account_hex: Option<String>,
    biometric_hex: Option<String>,
    fixture_canary_hex: Option<String>,
    has_op_session: bool,
    has_service_account: bool,
    has_connect: bool,
    has_opp_variable: bool,
    pid: u32,
    process_group: i32,
    session: i32,
    tty_device: Option<u32>,
    foreground_group: Option<i32>,
}

struct Harness {
    _root: TempDir,
    runtime: PathBuf,
    work: PathBuf,
    fake_op: PathBuf,
    log: PathBuf,
    explicit_timeout_ms: u64,
    check_timeout_ms: u64,
    maintenance_interval_ms: u64,
    hard_limit_ms: u64,
}

impl Harness {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("temporary test root");
        let bin = root.path().join("bin");
        let work = root.path().join("work");
        let runtime = root.path().join("run");
        fs::create_dir_all(&bin).expect("fixture bin directory");
        fs::create_dir_all(&work).expect("fixture working directory");
        let fake_op = bin.join("op");
        fs::copy(env!("CARGO_BIN_EXE_opp-test-op"), &fake_op).expect("copy fake op");
        fs::set_permissions(&fake_op, fs::Permissions::from_mode(0o755))
            .expect("make fake op executable");
        let log = bin.join("fake-op.log");
        Self {
            _root: root,
            runtime,
            work,
            fake_op,
            log,
            explicit_timeout_ms: 2_000,
            check_timeout_ms: 1_000,
            maintenance_interval_ms: 60_000,
            hard_limit_ms: 60_000,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_opp"));
        command
            .env_clear()
            .env("PATH", self.fake_op.parent().expect("fake op parent"))
            .env("HOME", self.work.join("untrusted-home"))
            .env("USER", "untrusted-user")
            .env("LOGNAME", "untrusted-logname")
            .env("FIXTURE_CANARY", "client-value")
            .env("OP_SESSION", "remove-me")
            .env("OP_SESSION_example", "remove-me-too")
            .env("OP_SERVICE_ACCOUNT_TOKEN", "remove-service-account")
            .env("OP_CONNECT_HOST", "remove-connect-host")
            .env("OP_CONNECT_TOKEN", "remove-connect-token")
            .env("OPP_TEST_RUNTIME_DIR", &self.runtime)
            .env(
                "OPP_TEST_EXPLICIT_TIMEOUT_MS",
                self.explicit_timeout_ms.to_string(),
            )
            .env(
                "OPP_TEST_CHECK_TIMEOUT_MS",
                self.check_timeout_ms.to_string(),
            )
            .env(
                "OPP_TEST_MAINTENANCE_INTERVAL_MS",
                self.maintenance_interval_ms.to_string(),
            )
            .env("OPP_TEST_HARD_LIMIT_MS", self.hard_limit_ms.to_string())
            .current_dir(&self.work);
        command
    }

    fn output<I, S>(&self, arguments: I) -> Output
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.command()
            .args(arguments)
            .output()
            .expect("run opp command")
    }

    fn start(&self) -> Output {
        self.output(["start"])
    }

    fn stop(&self) -> Output {
        self.output(["stop"])
    }

    fn invocations(&self) -> Vec<Invocation> {
        match fs::read_to_string(&self.log) {
            Ok(contents) => contents
                .lines()
                .map(|line| serde_json::from_str(line).expect("valid fake-op log record"))
                .collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => panic!("read fake-op log: {error}"),
        }
    }

    fn broker_errors(&self) -> String {
        fs::read_to_string(self.runtime.join("broker-errors.log")).unwrap_or_default()
    }

    fn wait_for_arguments(&self, arguments: &[&[u8]], deadline: Instant) {
        self.wait_for_new_arguments(0, arguments, deadline);
    }

    fn wait_for_new_arguments(&self, offset: usize, arguments: &[&[u8]], deadline: Instant) {
        let expected: Vec<_> = arguments.iter().map(|argument| hex(argument)).collect();
        while Instant::now() < deadline {
            if self
                .invocations()
                .iter()
                .skip(offset)
                .any(|invocation| invocation.arguments_hex == expected)
            {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("fake op did not receive expected arguments");
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[test]
fn broker_contract() {
    let harness = Harness::new();

    let version = harness.output(["--version"]);
    assert_eq!(version.status.code(), Some(0));
    assert_eq!(version.stdout, b"opp 0.1.0\n");
    assert!(version.stderr.is_empty());
    let help = harness.output(["--help"]);
    assert_eq!(help.status.code(), Some(0));
    assert_eq!(
        help.stdout,
        b"Usage:\n  opp --help\n  opp --version\n  opp start [--account ACCOUNT] [--op ABSOLUTE_PATH]\n  opp status [--account ACCOUNT]\n  opp stop\n  opp exec [--account ACCOUNT] [--timeout DURATION] -- [OP_ARGUMENT...]\n"
    );
    assert!(help.stderr.is_empty());
    for (command, expected) in [
        (
            "start",
            b"Usage: opp start [--account ACCOUNT] [--op ABSOLUTE_PATH]\n".as_slice(),
        ),
        (
            "status",
            b"Usage: opp status [--account ACCOUNT]\n".as_slice(),
        ),
        ("stop", b"Usage: opp stop\n".as_slice()),
        (
            "exec",
            b"Usage: opp exec [--account ACCOUNT] [--timeout DURATION] -- [OP_ARGUMENT...]\n"
                .as_slice(),
        ),
    ] {
        let output = harness.output([command, "--help"]);
        assert_eq!(output.status.code(), Some(0));
        assert_eq!(output.stdout, expected);
        assert!(output.stderr.is_empty());
    }
    assert!(!harness.runtime.exists());
    let no_path = harness
        .command()
        .env_remove("PATH")
        .arg("start")
        .output()
        .expect("start without PATH");
    assert_eq!(no_path.status.code(), Some(1));
    assert!(no_path.stdout.is_empty());
    assert_eq!(
        no_path.stderr,
        b"opp: could not resolve a canonical executable 'op'.\n"
    );
    let relative_op = harness.output(["start", "--op", "relative-op"]);
    assert_eq!(relative_op.status.code(), Some(1));
    assert_eq!(
        relative_op.stderr,
        b"opp: could not resolve a canonical executable 'op'.\n"
    );
    assert!(!harness.runtime.exists());

    fs::create_dir_all(&harness.runtime).expect("invalid-status runtime directory");
    fs::set_permissions(&harness.runtime, fs::Permissions::from_mode(0o700))
        .expect("invalid-status runtime permissions");
    let incompatible_listener =
        UnixListener::bind(harness.runtime.join("broker.sock")).expect("incompatible listener");
    let incompatible_server = thread::spawn(move || {
        let (mut stream, _) = incompatible_listener
            .accept()
            .expect("incompatible status client");
        let mut preamble = [0_u8; 8];
        stream.read_exact(&mut preamble).expect("protocol preamble");
        assert_eq!(&preamble[..4], b"OPP\0");
        preamble[4..].copy_from_slice(&2_u32.to_be_bytes());
        stream
            .write_all(&preamble)
            .expect("incompatible protocol response");
        drop(stream);

        let (mut stream, _) = incompatible_listener
            .accept()
            .expect("version-independent stop client");
        let mut stop = [0_u8; 8];
        stream.read_exact(&mut stop).expect("stop request");
        assert_eq!(&stop, b"OPPSTOP\0");
        stream.write_all(b"OPPSTOP\x01").expect("stop response");
    });
    let incompatible = harness.output(["status"]);
    assert_eq!(incompatible.status.code(), Some(1));
    assert!(incompatible.stdout.is_empty());
    assert_eq!(
        incompatible.stderr,
        b"opp: incompatible broker protocol; run 'opp stop' and restart the broker.\n"
    );
    assert_eq!(harness.stop().status.code(), Some(0));
    incompatible_server
        .join()
        .expect("incompatible protocol server");
    fs::remove_file(harness.runtime.join("broker.sock")).expect("remove incompatible socket");

    let invalid_listener =
        UnixListener::bind(harness.runtime.join("broker.sock")).expect("invalid-status listener");
    let invalid_server = thread::spawn(move || {
        let (mut stream, _) = invalid_listener.accept().expect("invalid-status client");
        let mut preamble = [0_u8; 8];
        stream.read_exact(&mut preamble).expect("protocol preamble");
        assert_eq!(&preamble[..4], b"OPP\0");
        stream.write_all(&preamble).expect("protocol response");
        let mut header = [0_u8; 8];
        stream.read_exact(&mut header).expect("status header");
        let length = usize::try_from(u32::from_be_bytes(header[4..].try_into().unwrap())).unwrap();
        let mut payload = vec![0_u8; length];
        stream.read_exact(&mut payload).expect("status payload");
        let invalid_json = b"not-json";
        let mut response = [0_u8; 8];
        response[0] = 0x82;
        response[4..].copy_from_slice(
            &u32::try_from(invalid_json.len())
                .expect("invalid JSON length")
                .to_be_bytes(),
        );
        stream.write_all(&response).expect("status response header");
        stream
            .write_all(invalid_json)
            .expect("invalid status response");
    });
    let invalid_status = harness.output(["status"]);
    invalid_server.join().expect("invalid-status server");
    assert_eq!(invalid_status.status.code(), Some(1));
    assert!(invalid_status.stdout.is_empty());
    assert_eq!(invalid_status.stderr, b"opp: status request failed.\n");
    fs::remove_file(harness.runtime.join("broker.sock")).expect("remove invalid-status socket");

    let missing = harness.output(["exec", "--", "fixture-exit", "0"]);
    assert_eq!(missing.status.code(), Some(77));
    assert!(missing.stdout.is_empty());
    assert_eq!(missing.stderr, AUTHORIZATION_REQUIRED.as_bytes());

    let started = harness.start();
    assert_eq!(
        started.status.code(),
        Some(0),
        "start stderr: {}; broker errors: {}",
        String::from_utf8_lossy(&started.stderr),
        harness.broker_errors()
    );
    assert!(started.stdout.is_empty());
    assert_eq!(started.stderr, WARNING.as_bytes());
    assert_mode(&harness.runtime, 0o700);
    assert_mode(&harness.runtime.join("broker.lock"), 0o600);
    assert_mode(&harness.runtime.join("broker.sock"), 0o600);

    let status = harness.output(["status"]);
    assert_eq!(status.status.code(), Some(0));
    assert!(status.stderr.is_empty());
    let status: Value = serde_json::from_slice(&status.stdout).expect("status JSON");
    assert_eq!(status["schema_version"], 1);
    assert_eq!(status["running"], true);
    assert_eq!(status["authorization"], "active");
    assert!(status.get("account_selector").is_none());
    assert_eq!(status["op_path"], canonical_text(&harness.fake_op));

    let already_active = harness.start();
    assert_eq!(already_active.status.code(), Some(0));
    assert!(already_active.stderr.is_empty());
    assert_eq!(harness.invocations().len(), 1);
    let explicit_same_path = harness.output([
        OsString::from("start"),
        OsString::from("--op"),
        harness.fake_op.clone().into_os_string(),
    ]);
    assert_eq!(explicit_same_path.status.code(), Some(0));
    assert!(explicit_same_path.stderr.is_empty());

    let selected = harness.output(["start", "--account", "Work.Example"]);
    assert_eq!(selected.status.code(), Some(0));
    assert_eq!(selected.stderr, WARNING.as_bytes());
    let selected_status = harness.output(["status", "--account", "Work.Example"]);
    let selected_status: Value =
        serde_json::from_slice(&selected_status.stdout).expect("selected status JSON");
    assert_eq!(selected_status["authorization"], "active");
    assert_eq!(selected_status["account_selector"], "Work.Example");
    let unknown = harness.output(["status", "--account", "Unknown"]);
    let unknown: Value = serde_json::from_slice(&unknown.stdout).expect("unknown status JSON");
    assert_eq!(unknown["authorization"], "reauthorization_required");
    assert!(unknown.get("authorized_at").is_none());

    let environment_selected = harness
        .command()
        .env("OP_ACCOUNT", "Environment.Example")
        .arg("start")
        .output()
        .expect("environment-selected start");
    assert_eq!(environment_selected.status.code(), Some(0));
    assert_eq!(environment_selected.stderr, WARNING.as_bytes());
    let environment_status = harness
        .command()
        .env("OP_ACCOUNT", "Environment.Example")
        .arg("status")
        .output()
        .expect("environment-selected status");
    let environment_status: Value =
        serde_json::from_slice(&environment_status.stdout).expect("environment status JSON");
    assert_eq!(
        environment_status["account_selector"],
        "Environment.Example"
    );
    assert_eq!(environment_status["authorization"], "active");
    let explicit_precedence = harness
        .command()
        .env("OP_ACCOUNT", "Environment.Example")
        .args(["status", "--account", "Work.Example"])
        .output()
        .expect("explicit account precedence");
    let explicit_precedence: Value =
        serde_json::from_slice(&explicit_precedence.stdout).expect("precedence status JSON");
    assert_eq!(explicit_precedence["account_selector"], "Work.Example");
    let empty_falls_back = harness
        .command()
        .env("OP_ACCOUNT", "Environment.Example")
        .args(["start", "--account", ""])
        .output()
        .expect("empty account fallback");
    assert_eq!(empty_falls_back.status.code(), Some(0));
    assert!(empty_falls_back.stderr.is_empty());
    let invalid_account = harness
        .command()
        .env("OP_ACCOUNT", OsString::from_vec(vec![0xff]))
        .arg("status")
        .output()
        .expect("invalid account selector");
    assert_eq!(invalid_account.status.code(), Some(2));
    assert!(invalid_account.stdout.is_empty());

    let alternate = harness.work.join("other-op");
    fs::copy(&harness.fake_op, &alternate).expect("copy alternate fake op");
    fs::set_permissions(&alternate, fs::Permissions::from_mode(0o755))
        .expect("make alternate executable");
    let conflict = harness.output([
        OsString::from("start"),
        OsString::from("--op"),
        alternate.into_os_string(),
    ]);
    assert_eq!(conflict.status.code(), Some(1));
    assert!(conflict.stdout.is_empty());
    assert_eq!(
        conflict.stderr,
        b"opp: the running broker uses a different 'op' executable; stop it first.\n"
    );

    let before_invalid = harness.invocations().len();
    let invalid = harness.output(["exec", "--", "item", "--account=forbidden"]);
    assert_eq!(invalid.status.code(), Some(2));
    assert!(invalid.stdout.is_empty());
    assert_eq!(harness.invocations().len(), before_invalid);

    let raw = OsString::from_vec(vec![0xff, b'x']);
    let preserved = harness.output([
        OsString::from("exec"),
        OsString::from("--account"),
        OsString::from("Work.Example"),
        OsString::from("--"),
        OsString::from("fixture-print-args"),
        OsString::from("--"),
        OsString::from("--account=child"),
        raw,
    ]);
    assert_eq!(preserved.status.code(), Some(0));
    assert!(preserved.stderr.is_empty());
    let preserved: Vec<String> =
        serde_json::from_slice(&preserved.stdout).expect("preserved argument JSON");
    assert_eq!(
        preserved,
        [hex(b"--"), hex(b"--account=child"), String::from("ff78")]
    );

    let environment = harness.output(["exec", "--account", "Work.Example", "--", "fixture-env"]);
    assert_eq!(environment.status.code(), Some(0));
    assert!(environment.stderr.is_empty());
    let environment: Invocation =
        serde_json::from_slice(&environment.stdout).expect("fixture environment JSON");
    assert_eq!(environment.cwd_hex, hex_path(&harness.work));
    assert_eq!(environment.op_account_hex, Some(hex(b"Work.Example")));
    assert_eq!(environment.biometric_hex, Some(hex(b"true")));
    assert_eq!(environment.fixture_canary_hex, Some(hex(b"client-value")));
    assert!(!environment.has_op_session);
    assert!(!environment.has_service_account);
    assert!(!environment.has_connect);
    assert!(!environment.has_opp_variable);

    let raw_directory = harness.work.join("raw-context");
    fs::create_dir(&raw_directory).expect("raw working directory");
    let raw_context = harness
        .command()
        .current_dir(&raw_directory)
        .env("FIXTURE_CANARY", OsString::from_vec(vec![0xff, b'x']))
        .args(["exec", "--account", "Work.Example", "--", "fixture-env"])
        .output()
        .expect("raw client context");
    assert_eq!(raw_context.status.code(), Some(0));
    let raw_context: Invocation =
        serde_json::from_slice(&raw_context.stdout).expect("raw context JSON");
    assert_eq!(raw_context.cwd_hex, hex_path(&raw_directory));
    assert_eq!(raw_context.fixture_canary_hex, Some(String::from("ff78")));

    let input: Vec<u8> = (0_u8..=255).cycle().take(32 * 1024).collect();
    let mut streams = harness.command();
    let mut streams = streams
        .args(["exec", "--", "fixture-streams"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn stream test");
    streams
        .stdin
        .take()
        .expect("stream stdin")
        .write_all(&input)
        .expect("write stream input");
    let streams = streams.wait_with_output().expect("wait for stream test");
    assert_eq!(streams.status.code(), Some(0));
    let mut expected_stdout = b"out\0\xfe".repeat(32_768);
    expected_stdout.extend_from_slice(&input);
    assert_eq!(streams.stdout, expected_stdout);
    assert_eq!(streams.stderr, b"err\0\xff".repeat(32_768));

    let native_exit = harness.output(["exec", "--", "fixture-exit", "42"]);
    assert_eq!(native_exit.status.code(), Some(42));
    assert!(native_exit.stdout.is_empty());
    assert!(native_exit.stderr.is_empty());
    let native_signal = harness.output(["exec", "--", "fixture-signal", "15"]);
    assert_eq!(native_signal.status.code(), Some(143));
    assert!(native_signal.stdout.is_empty());
    assert!(native_signal.stderr.is_empty());
    let no_arguments = harness.output(["exec", "--"]);
    assert_eq!(no_arguments.status.code(), Some(0));
    assert!(no_arguments.stdout.is_empty());
    assert!(no_arguments.stderr.is_empty());

    let sessions = harness.invocations();
    let first_session = sessions.first().expect("at least one invocation").session;
    let first_tty = sessions
        .first()
        .and_then(|invocation| invocation.tty_device);
    assert!(first_session > 0);
    assert!(first_tty.is_some());
    assert!(
        sessions
            .iter()
            .all(|invocation| invocation.session == first_session)
    );
    assert!(
        sessions
            .iter()
            .all(|invocation| invocation.tty_device == first_tty)
    );
    assert!(sessions.iter().all(|invocation| {
        invocation.process_group == i32::try_from(invocation.pid).expect("fixture pid fits i32")
            && invocation.foreground_group == Some(invocation.process_group)
    }));

    let failed_check = harness
        .command()
        .env("FAKE_OP_WHOAMI_EXIT", "9")
        .args(["exec", "--", "fixture-exit", "0"])
        .output()
        .expect("automatic-check failure");
    assert_eq!(failed_check.status.code(), Some(77));
    assert!(failed_check.stdout.is_empty());
    assert_eq!(failed_check.stderr, AUTHORIZATION_REQUIRED.as_bytes());
    for account in [None, Some("Work.Example"), Some("Environment.Example")] {
        let mut command = harness.command();
        command.arg("status");
        if let Some(account) = account {
            command.args(["--account", account]);
        }
        let output = command.output().expect("status after global invalidation");
        let value: Value = serde_json::from_slice(&output.stdout).expect("status JSON");
        assert_eq!(value["authorization"], "reauthorization_required");
    }

    let recovered = harness.output(["start", "--account", "Work.Example"]);
    assert_eq!(recovered.status.code(), Some(0));
    assert_eq!(recovered.stderr, WARNING.as_bytes());
    let default_status = harness.output(["status"]);
    let default_status: Value =
        serde_json::from_slice(&default_status.stdout).expect("default status JSON");
    assert_eq!(default_status["authorization"], "reauthorization_required");

    let timed = Instant::now();
    let timeout = harness.output([
        "exec",
        "--account",
        "Work.Example",
        "--timeout",
        "1s",
        "--",
        "fixture-descendant",
    ]);
    assert_eq!(timeout.status.code(), Some(124));
    assert!(timeout.stdout.is_empty());
    assert_eq!(timeout.stderr, b"opp: command timed out.\n");
    assert!(timed.elapsed() < Duration::from_secs(5));

    let mut signalled = harness.command();
    let signalled = signalled
        .args([
            "exec",
            "--account",
            "Work.Example",
            "--",
            "fixture-ignore-term",
            "9000",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn signal test");
    harness.wait_for_arguments(
        &[b"fixture-ignore-term", b"9000"],
        Instant::now() + Duration::from_secs(2),
    );
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(signalled.id()).expect("child pid fits i32")),
        nix::sys::signal::Signal::SIGINT,
    )
    .expect("signal opp client");
    let signalled = signalled.wait_with_output().expect("wait for signal test");
    assert_eq!(signalled.status.code(), Some(130));
    assert!(signalled.stdout.is_empty());
    assert!(signalled.stderr.is_empty());

    let stopped = harness.stop();
    assert_eq!(
        stopped.status.code(),
        Some(0),
        "stop stderr: {}; broker errors: {}",
        String::from_utf8_lossy(&stopped.stderr),
        harness.broker_errors()
    );
    assert!(stopped.stdout.is_empty());
    assert!(stopped.stderr.is_empty());
    assert!(!harness.runtime.join("broker.sock").exists());
    let mut runtime_entries: Vec<_> = fs::read_dir(&harness.runtime)
        .expect("runtime directory")
        .map(|entry| entry.expect("runtime entry").file_name())
        .collect();
    runtime_entries.sort();
    assert_eq!(runtime_entries, [OsString::from("broker.lock")]);
    assert!(
        fs::read(harness.runtime.join("broker.lock"))
            .expect("broker lock contents")
            .is_empty()
    );
    assert_eq!(harness.stop().status.code(), Some(0));
    assert_eq!(
        harness.output(["status"]).stdout,
        b"{\"schema_version\":1,\"running\":false}\n"
    );
}

#[test]
fn authorization_timing_contract() {
    let mut maintenance = Harness::new();
    maintenance.maintenance_interval_ms = 100;
    maintenance.hard_limit_ms = 5_000;
    let started = maintenance.start();
    assert_eq!(
        started.status.code(),
        Some(0),
        "start stderr: {}; broker errors: {}",
        String::from_utf8_lossy(&started.stderr),
        maintenance.broker_errors()
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    while maintenance.invocations().len() < 3 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    let records = maintenance.invocations();
    assert!(records.len() >= 3, "maintenance probe did not run");
    let whoami = records
        .iter()
        .find(|record| record.arguments_hex == [hex(b"whoami")])
        .expect("maintenance whoami");
    let expected_keys = [
        "HOME",
        "LOGNAME",
        "OP_BIOMETRIC_UNLOCK_ENABLED",
        "PATH",
        "TMPDIR",
        "USER",
    ];
    let mut expected_keys: Vec<_> = expected_keys
        .iter()
        .map(|key| hex(key.as_bytes()))
        .collect();
    expected_keys.sort();
    assert_eq!(whoami.environment_keys_hex, expected_keys);
    let user = nix::unistd::User::from_uid(nix::unistd::Uid::effective())
        .expect("effective user lookup")
        .expect("effective user exists");
    assert_eq!(whoami.cwd_hex, hex_path(&user.dir));
    assert!(!whoami.has_opp_variable);
    assert_eq!(maintenance.stop().status.code(), Some(0));

    let mut hard_limit = Harness::new();
    hard_limit.maintenance_interval_ms = 60_000;
    hard_limit.hard_limit_ms = 200;
    assert_eq!(hard_limit.start().status.code(), Some(0));
    thread::sleep(Duration::from_millis(300));
    let status = hard_limit.output(["status"]);
    let status: Value = serde_json::from_slice(&status.stdout).expect("hard-limit status JSON");
    assert_eq!(status["authorization"], "reauthorization_required");
    assert!(status.get("authorized_at").is_none());
    let expired = hard_limit.output(["exec", "--", "fixture-exit", "0"]);
    assert_eq!(expired.status.code(), Some(77));
    assert_eq!(expired.stderr, AUTHORIZATION_REQUIRED.as_bytes());
    assert_eq!(hard_limit.stop().status.code(), Some(0));

    let mut combined = Harness::new();
    combined.check_timeout_ms = 200;
    assert_eq!(combined.start().status.code(), Some(0));
    let failed = combined
        .command()
        .env("FAKE_OP_WHOAMI_DELAY_MS", "120")
        .env("FAKE_OP_VAULT_DELAY_MS", "120")
        .args(["exec", "--", "fixture-exit", "0"])
        .output()
        .expect("combined check timeout");
    assert_eq!(failed.status.code(), Some(77));
    assert_eq!(failed.stderr, AUTHORIZATION_REQUIRED.as_bytes());
    let calls = combined.invocations();
    assert!(
        calls
            .iter()
            .any(|record| record.arguments_hex == [hex(b"whoami")])
    );
    assert!(calls.iter().any(|record| {
        record.arguments_hex == [hex(b"vault"), hex(b"list"), hex(b"--format=json")]
    }));
    assert_eq!(combined.stop().status.code(), Some(0));

    let mut explicit = Harness::new();
    explicit.explicit_timeout_ms = 150;
    let failed = explicit
        .command()
        .env("FAKE_OP_VAULT_DELAY_MS", "400")
        .arg("start")
        .output()
        .expect("explicit authorization timeout");
    assert_eq!(failed.status.code(), Some(1));
    assert_eq!(
        failed.stderr,
        format!("{WARNING}opp: 1Password authorization failed or was cancelled.\n").as_bytes()
    );
    let status = explicit.output(["status"]);
    let status: Value = serde_json::from_slice(&status.stdout).expect("failed start status JSON");
    assert_eq!(status["authorization"], "reauthorization_required");
}

#[test]
fn singleton_startup_and_global_serialization() {
    let harness = Harness::new();
    let mut first = harness.command();
    let first = first
        .arg("start")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("first concurrent start");
    let mut second = harness.command();
    let second = second
        .arg("start")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("second concurrent start");
    let first = first.wait_with_output().expect("first start output");
    let second = second.wait_with_output().expect("second start output");
    assert_eq!(first.status.code(), Some(0));
    assert_eq!(second.status.code(), Some(0));
    assert!(first.stderr == WARNING.as_bytes() || first.stderr.is_empty());
    assert!(second.stderr == WARNING.as_bytes() || second.stderr.is_empty());
    assert!(first.stderr == WARNING.as_bytes() || second.stderr == WARNING.as_bytes());
    assert_eq!(
        harness
            .invocations()
            .iter()
            .filter(|record| {
                record.arguments_hex == [hex(b"vault"), hex(b"list"), hex(b"--format=json")]
            })
            .count(),
        1
    );

    let mut slow = harness.command();
    let slow = slow
        .args(["exec", "--", "fixture-sleep", "200"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("slow concurrent exec");
    harness.wait_for_arguments(
        &[b"fixture-sleep", b"200"],
        Instant::now() + Duration::from_secs(2),
    );
    let mut fast = harness.command();
    let fast = fast
        .args(["exec", "--", "fixture-exit", "0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("fast concurrent exec");
    assert_eq!(slow.wait_with_output().unwrap().status.code(), Some(0));
    assert_eq!(fast.wait_with_output().unwrap().status.code(), Some(0));

    let calls = harness.invocations();
    let calls: Vec<_> = calls
        .iter()
        .filter(|record| {
            record.arguments_hex == [hex(b"whoami")]
                || record.arguments_hex == [hex(b"vault"), hex(b"list"), hex(b"--format=json")]
                || record.arguments_hex.first() == Some(&hex(b"fixture-sleep"))
                || record.arguments_hex.first() == Some(&hex(b"fixture-exit"))
        })
        .map(|record| record.arguments_hex.clone())
        .collect();
    assert_eq!(
        calls,
        [
            vec![hex(b"vault"), hex(b"list"), hex(b"--format=json")],
            vec![hex(b"whoami")],
            vec![hex(b"vault"), hex(b"list"), hex(b"--format=json")],
            vec![hex(b"fixture-sleep"), hex(b"200")],
            vec![hex(b"whoami")],
            vec![hex(b"vault"), hex(b"list"), hex(b"--format=json")],
            vec![hex(b"fixture-exit"), hex(b"0")],
        ]
    );
}

#[test]
fn disconnect_cancels_queued_checks_and_commands() {
    let harness = Harness::new();
    assert_eq!(harness.start().status.code(), Some(0));

    let mut active = harness.command();
    let active = active
        .args(["exec", "--", "fixture-sleep", "500"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("active request");
    harness.wait_for_arguments(
        &[b"fixture-sleep", b"500"],
        Instant::now() + Duration::from_secs(2),
    );
    let mut queued = harness.command();
    let mut queued = queued
        .args(["exec", "--", "fixture-exit", "33"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("queued request");
    thread::sleep(Duration::from_millis(50));
    queued.kill().expect("disconnect queued request");
    let _ = queued.wait();
    assert_eq!(active.wait_with_output().unwrap().status.code(), Some(0));
    let status = harness.output(["status"]);
    assert_eq!(status.status.code(), Some(0));
    assert!(
        !harness
            .invocations()
            .iter()
            .any(|record| { record.arguments_hex == [hex(b"fixture-exit"), hex(b"33")] })
    );

    let automatic_offset = harness.invocations().len();
    let mut automatic = harness.command();
    let mut automatic = automatic
        .env("FAKE_OP_WHOAMI_DELAY_MS", "10000")
        .args(["exec", "--", "fixture-exit", "44"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("automatic-check request");
    harness.wait_for_new_arguments(
        automatic_offset,
        &[b"whoami"],
        Instant::now() + Duration::from_secs(2),
    );
    automatic
        .kill()
        .expect("disconnect automatic-check request");
    let _ = automatic.wait();
    let status = harness.output(["status"]);
    let status: Value = serde_json::from_slice(&status.stdout).expect("status after cancellation");
    assert_eq!(status["authorization"], "active");
    let automatic_calls = harness.invocations();
    assert!(!automatic_calls[automatic_offset..].iter().any(|record| {
        record.arguments_hex == [hex(b"vault"), hex(b"list"), hex(b"--format=json")]
            || record.arguments_hex == [hex(b"fixture-exit"), hex(b"44")]
    }));

    let command_offset = harness.invocations().len();
    let mut command = harness.command();
    let mut command = command
        .args(["exec", "--", "fixture-ignore-term", "7000"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("active command request");
    harness.wait_for_new_arguments(
        command_offset,
        &[b"fixture-ignore-term", b"7000"],
        Instant::now() + Duration::from_secs(2),
    );
    command.kill().expect("disconnect active command request");
    let _ = command.wait();
    let status = harness.output(["status"]);
    let status: Value = serde_json::from_slice(&status.stdout).expect("status after disconnect");
    assert_eq!(status["authorization"], "active");

    let usable = harness.output(["exec", "--", "fixture-exit", "0"]);
    assert_eq!(usable.status.code(), Some(0));

    let stop_offset = harness.invocations().len();
    let mut active_at_stop = harness.command();
    let active_at_stop = active_at_stop
        .args(["exec", "--", "fixture-ignore-term", "6000"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("request active during stop");
    harness.wait_for_new_arguments(
        stop_offset,
        &[b"fixture-ignore-term", b"6000"],
        Instant::now() + Duration::from_secs(2),
    );
    let stop_started = Instant::now();
    assert_eq!(harness.stop().status.code(), Some(0));
    assert!(stop_started.elapsed() < Duration::from_secs(4));
    let active_at_stop = active_at_stop
        .wait_with_output()
        .expect("active request after stop");
    assert_eq!(active_at_stop.status.code(), Some(125));
    assert_eq!(
        active_at_stop.stderr,
        b"opp: broker communication failed.\n"
    );
}

fn assert_mode(path: &Path, expected: u32) {
    let mode = fs::symlink_metadata(path)
        .unwrap_or_else(|error| panic!("metadata for {}: {error}", path.display()))
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, expected, "mode for {}", path.display());
}

fn canonical_text(path: &Path) -> String {
    fs::canonicalize(path)
        .expect("canonical path")
        .to_str()
        .expect("UTF-8 fixture path")
        .to_owned()
}

fn hex_path(path: &Path) -> String {
    hex(fs::canonicalize(path)
        .expect("canonical path")
        .as_os_str()
        .as_bytes())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
