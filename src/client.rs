use crate::account::Account;
use crate::broker::{self, StartupResult};
use crate::environment::ClientContext;
use crate::protocol::{self, ClientEvent, Diagnostic, ErrorCode, Request, Response};
use crate::runtime::{RuntimePaths, is_absent_socket_error};
use crate::status::Status;
use nix::sys::signal::{SigSet, SigmaskHow, Signal, pthread_sigmask};
use nix::unistd::pipe;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub(crate) const WARNING: &str = "opp: warning: the broker gives every process that can reach its socket the full authorized 1Password CLI authority for every account added to it, including same-user command execution through 'op run'.\n";
pub(crate) const AUTHORIZATION_REQUIRED: &str = "opp: the selected 1Password account requires authorization. Stop and inform the user. Wait for the user to run 'opp start' with the same account selection and confirm completion. Do not retry or run 'opp start' yourself.\n";
const INCOMPATIBLE: &str =
    "opp: incompatible broker protocol; run 'opp stop' and restart the broker.\n";

pub(crate) fn start(account: Account, op: Option<OsString>) -> i32 {
    let op_path = match resolve_op(op.as_deref()) {
        Ok(path) => path,
        Err(_) => return lifecycle_error("opp: could not resolve a canonical executable 'op'.\n"),
    };
    let context = match ClientContext::capture() {
        Ok(context) => context,
        Err(_) => return lifecycle_error("opp: could not read the invoking process context.\n"),
    };
    let paths = match RuntimePaths::load() {
        Ok(paths) => paths,
        Err(_) => return lifecycle_error("opp: could not determine the broker runtime path.\n"),
    };

    match connect(&paths) {
        Ok(Some(mut stream)) => start_existing(&mut stream, account, op_path, context),
        Ok(None) => {
            if write_stderr(WARNING.as_bytes()).is_err() {
                return 1;
            }
            if let Err(error) = spawn_broker(&paths, &op_path) {
                return if error.kind() == io::ErrorKind::Unsupported {
                    lifecycle_error(INCOMPATIBLE)
                } else {
                    lifecycle_error("opp: the broker failed to start.\n")
                };
            }
            let mut stream = match connect_until(&paths, Instant::now() + Duration::from_secs(5)) {
                Ok(stream) => stream,
                Err(_) => return lifecycle_error("opp: the broker failed to become ready.\n"),
            };
            let request = Request::Start {
                warning_shown: true,
                account,
                op_path,
                context,
            };
            match transact_without_negotiation(&mut stream, &request) {
                Ok(response) => lifecycle_response(response),
                Err(error) if error.kind() == io::ErrorKind::Unsupported => {
                    lifecycle_error(INCOMPATIBLE)
                }
                Err(_) => lifecycle_error("opp: broker communication failed.\n"),
            }
        }
        Err(error) if error.kind() == io::ErrorKind::Unsupported => lifecycle_error(INCOMPATIBLE),
        Err(_) => lifecycle_error("opp: broker communication failed.\n"),
    }
}

fn start_existing(
    stream: &mut UnixStream,
    account: Account,
    op_path: String,
    context: ClientContext,
) -> i32 {
    let first = Request::Start {
        warning_shown: false,
        account: account.clone(),
        op_path: op_path.clone(),
        context: context.clone(),
    };
    if let Err(error) = protocol::send_request(stream, &first) {
        return if error.kind() == io::ErrorKind::Unsupported {
            lifecycle_error(INCOMPATIBLE)
        } else {
            lifecycle_error("opp: broker communication failed.\n")
        };
    }
    match protocol::receive_response(stream) {
        Ok(Response::NeedWarning) => {
            if write_stderr(WARNING.as_bytes()).is_err() {
                return 1;
            }
            let request = Request::Start {
                warning_shown: true,
                account,
                op_path,
                context,
            };
            match transact_without_negotiation(stream, &request) {
                Ok(response) => lifecycle_response(response),
                Err(_) => lifecycle_error("opp: broker communication failed.\n"),
            }
        }
        Ok(response) => lifecycle_response(response),
        Err(_) => lifecycle_error("opp: broker communication failed.\n"),
    }
}

pub(crate) fn status(account: Account) -> i32 {
    let paths = match RuntimePaths::load() {
        Ok(paths) => paths,
        Err(_) => return lifecycle_error("opp: could not determine the broker runtime path.\n"),
    };
    let mut stream = match connect(&paths) {
        Ok(Some(stream)) => stream,
        Ok(None) => return write_status(&Status::stopped()),
        Err(error) if error.kind() == io::ErrorKind::Unsupported => {
            return lifecycle_error(INCOMPATIBLE);
        }
        Err(_) => return lifecycle_error("opp: broker communication failed.\n"),
    };
    let request = Request::Status { account };
    match transact_without_negotiation(&mut stream, &request) {
        Ok(Response::Status(mut json)) => {
            if !Status::valid_json(&json) {
                return lifecycle_error("opp: status request failed.\n");
            }
            json.push(b'\n');
            if io::stdout().write_all(&json).is_ok() {
                0
            } else {
                1
            }
        }
        Ok(_) | Err(_) => lifecycle_error("opp: status request failed.\n"),
    }
}

pub(crate) fn stop() -> i32 {
    let paths = match RuntimePaths::load() {
        Ok(paths) => paths,
        Err(_) => return lifecycle_error("opp: could not determine the broker runtime path.\n"),
    };
    let mut stream = match UnixStream::connect(&paths.socket) {
        Ok(stream) => stream,
        Err(error) if is_absent_socket_error(&error) => return 0,
        Err(_) => return lifecycle_error("opp: broker communication failed.\n"),
    };
    if stream.write_all(protocol::STOP_REQUEST).is_err() {
        return lifecycle_error("opp: broker stop failed.\n");
    }
    let mut response = [0_u8; 8];
    if stream.read_exact(&mut response).is_ok() && &response == protocol::STOP_RESPONSE {
        0
    } else {
        lifecycle_error("opp: broker stop failed.\n")
    }
}

pub(crate) fn execute(account: Account, timeout: Duration, arguments: Vec<Vec<u8>>) -> i32 {
    let paths = match RuntimePaths::load() {
        Ok(paths) => paths,
        Err(_) => return exec_error(125, "opp: could not determine the broker runtime path.\n"),
    };
    let mut stream = match connect(&paths) {
        Ok(Some(stream)) => stream,
        Ok(None) => return authorization_required(),
        Err(error) if error.kind() == io::ErrorKind::Unsupported => {
            return exec_error(125, INCOMPATIBLE);
        }
        Err(_) => return exec_error(125, "opp: broker communication failed.\n"),
    };
    let context = match ClientContext::capture() {
        Ok(context) => context,
        Err(_) => return exec_error(125, "opp: could not read the invoking process context.\n"),
    };

    let (child_stdin, client_stdin) = match pipe() {
        Ok(pair) => pair,
        Err(_) => return exec_error(125, "opp: could not create command pipes.\n"),
    };
    let (client_stdout, child_stdout) = match pipe() {
        Ok(pair) => pair,
        Err(_) => return exec_error(125, "opp: could not create command pipes.\n"),
    };
    let (client_stderr, child_stderr) = match pipe() {
        Ok(pair) => pair,
        Err(_) => return exec_error(125, "opp: could not create command pipes.\n"),
    };

    let request = Request::Exec {
        account,
        timeout_nanos: u64::try_from(timeout.as_nanos()).unwrap_or(u64::MAX),
        arguments,
        context,
        descriptors: [child_stdin, child_stdout, child_stderr],
    };
    if protocol::send_request(&mut stream, &request).is_err() {
        return exec_error(125, "opp: broker communication failed.\n");
    }
    drop(request);

    let signals = termination_signals();
    let mut old_mask = SigSet::empty();
    let signals_blocked =
        pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&signals), Some(&mut old_mask)).is_ok();

    let writer = match stream.try_clone() {
        Ok(stream) => Arc::new(Mutex::new(stream)),
        Err(_) => return exec_error(125, "opp: broker communication failed.\n"),
    };
    let signal_received = Arc::new(AtomicI32::new(0));
    if signals_blocked {
        let signal_writer = Arc::clone(&writer);
        let signal_value = Arc::clone(&signal_received);
        let signal_set = signals;
        let _ = thread::Builder::new()
            .name(String::from("opp-signal"))
            .spawn(move || {
                if let Ok(signal) = signal_set.wait() {
                    let value = signal as i32;
                    signal_value.store(value, Ordering::Release);
                    if let Ok(mut stream) = signal_writer.lock() {
                        let _ =
                            protocol::send_client_event(&mut stream, ClientEvent::Cancel(value));
                    }
                }
            });
    }

    let _stdin = thread::Builder::new()
        .name(String::from("opp-stdin"))
        .spawn(move || copy_stdin(client_stdin));
    let stdout = thread::Builder::new()
        .name(String::from("opp-stdout"))
        .spawn(move || copy_stdout(client_stdout));
    let stderr = thread::Builder::new()
        .name(String::from("opp-stderr"))
        .spawn(move || copy_stderr(client_stderr));

    let stdout_result = join_copy_thread(stdout, "stdout thread panicked");
    let stderr_result = join_copy_thread(stderr, "stderr thread panicked");
    let stream_error = stdout_result.is_err() || stderr_result.is_err();
    if let Ok(mut stream) = writer.lock() {
        let _ = protocol::send_client_event(&mut stream, ClientEvent::StreamsClosed);
    }
    let response = protocol::receive_response(&mut stream);
    if signals_blocked {
        let _ = pthread_sigmask(SigmaskHow::SIG_SETMASK, Some(&old_mask), None);
    }
    if stream_error {
        return exec_error(125, "opp: standard-stream forwarding failed.\n");
    }
    match response {
        Ok(Response::Exec {
            exit_code,
            diagnostic,
        }) => {
            match diagnostic {
                Diagnostic::None => {}
                Diagnostic::Timeout => {
                    let _ = write_stderr(b"opp: command timed out.\n");
                }
                Diagnostic::Internal => {
                    let _ = write_stderr(b"opp: command execution failed.\n");
                }
            }
            exit_code
        }
        Ok(Response::Error(ErrorCode::AuthorizationRequired)) => authorization_required(),
        Ok(Response::Error(ErrorCode::Cancelled)) => {
            let signal = signal_received.load(Ordering::Acquire);
            if signal > 0 {
                128 + signal
            } else {
                exec_error(125, "opp: command was cancelled.\n")
            }
        }
        Ok(Response::Error(_)) => exec_error(125, "opp: command execution failed.\n"),
        Ok(_) | Err(_) => exec_error(125, "opp: broker communication failed.\n"),
    }
}

fn connect(paths: &RuntimePaths) -> io::Result<Option<UnixStream>> {
    match UnixStream::connect(&paths.socket) {
        Ok(mut stream) => {
            protocol::client_negotiate(&mut stream)?;
            Ok(Some(stream))
        }
        Err(error) if is_absent_socket_error(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

fn connect_until(paths: &RuntimePaths, deadline: Instant) -> io::Result<UnixStream> {
    loop {
        match connect(paths) {
            Ok(Some(stream)) => return Ok(stream),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "broker startup timed out",
                ));
            }
            Err(error) => return Err(error),
        }
    }
}

fn spawn_broker(paths: &RuntimePaths, op_path: &str) -> io::Result<()> {
    let (mut parent, child) = UnixStream::pair()?;
    parent.set_read_timeout(Some(Duration::from_secs(5)))?;
    parent.set_write_timeout(Some(Duration::from_secs(5)))?;
    let executable = std::env::current_exe()?;
    let mut command = Command::new(executable);
    command
        .arg(broker::INTERNAL_BROKER_ARGUMENT)
        .env_clear()
        .stdin(Stdio::from(OwnedFd::from(child)))
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(feature = "test-support")]
    for key in [
        "OPP_TEST_RUNTIME_DIR",
        "OPP_TEST_EXPLICIT_TIMEOUT_MS",
        "OPP_TEST_CHECK_TIMEOUT_MS",
        "OPP_TEST_MAINTENANCE_INTERVAL_MS",
        "OPP_TEST_HARD_LIMIT_MS",
    ] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    let _child = command.spawn()?;
    broker::write_startup_configuration(&mut parent, op_path)?;
    match broker::read_startup_result(&mut parent)? {
        StartupResult::Ready => Ok(()),
        StartupResult::Existing => {
            connect_until(paths, Instant::now() + Duration::from_secs(5)).map(drop)
        }
        StartupResult::Failed => Err(io::Error::other("broker startup failed")),
    }
}

fn transact_without_negotiation(
    stream: &mut UnixStream,
    request: &Request,
) -> io::Result<Response> {
    protocol::send_request(stream, request)?;
    protocol::receive_response(stream)
}

fn lifecycle_response(response: Response) -> i32 {
    match response {
        Response::Ok => 0,
        Response::Error(ErrorCode::AuthorizationFailed) => {
            lifecycle_error("opp: 1Password authorization failed or was cancelled.\n")
        }
        Response::Error(ErrorCode::OpPathMismatch) => lifecycle_error(
            "opp: the running broker uses a different 'op' executable; stop it first.\n",
        ),
        Response::Error(_) => lifecycle_error("opp: broker operation failed.\n"),
        _ => lifecycle_error("opp: invalid broker response.\n"),
    }
}

fn resolve_op(explicit: Option<&OsStr>) -> io::Result<String> {
    let path = if let Some(path) = explicit {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "explicit op path is not absolute",
            ));
        }
        path
    } else {
        find_in_path(OsStr::new("op"))?
    };
    let path = fs::canonicalize(path)?;
    let metadata = fs::metadata(&path)?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "op is not a regular executable",
        ));
    }
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "op path is not UTF-8"))
}

fn find_in_path(name: &OsStr) -> io::Result<PathBuf> {
    let path = std::env::var_os("PATH")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "PATH is not set"))?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(name);
        if candidate.is_file() && is_executable(&candidate) {
            return Ok(candidate);
        }
    }
    Err(io::Error::new(io::ErrorKind::NotFound, "op not found"))
}

fn is_executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

fn copy_stdin(descriptor: OwnedFd) -> io::Result<()> {
    let mut destination = File::from(descriptor);
    io::copy(&mut io::stdin().lock(), &mut destination).map(|_| ())
}

fn copy_stdout(descriptor: OwnedFd) -> io::Result<()> {
    let mut source = File::from(descriptor);
    io::copy(&mut source, &mut io::stdout().lock()).map(|_| ())
}

fn copy_stderr(descriptor: OwnedFd) -> io::Result<()> {
    let mut source = File::from(descriptor);
    io::copy(&mut source, &mut io::stderr().lock()).map(|_| ())
}

fn join_copy_thread(
    thread: io::Result<thread::JoinHandle<io::Result<()>>>,
    panic_message: &'static str,
) -> io::Result<()> {
    thread?
        .join()
        .map_err(|_| io::Error::other(panic_message))?
}

fn termination_signals() -> SigSet {
    let mut signals = SigSet::empty();
    signals.add(Signal::SIGHUP);
    signals.add(Signal::SIGINT);
    signals.add(Signal::SIGQUIT);
    signals.add(Signal::SIGTERM);
    signals
}

fn write_status(status: &Status) -> i32 {
    match status.json() {
        Ok(mut json) => {
            json.push(b'\n');
            if io::stdout().write_all(&json).is_ok() {
                0
            } else {
                1
            }
        }
        Err(_) => lifecycle_error("opp: could not encode status.\n"),
    }
}

fn authorization_required() -> i32 {
    let _ = write_stderr(AUTHORIZATION_REQUIRED.as_bytes());
    77
}

fn lifecycle_error(message: &str) -> i32 {
    let _ = write_stderr(message.as_bytes());
    1
}

fn exec_error(code: i32, message: &str) -> i32 {
    let _ = write_stderr(message.as_bytes());
    code
}

fn write_stderr(bytes: &[u8]) -> io::Result<()> {
    io::stderr().write_all(bytes)
}
