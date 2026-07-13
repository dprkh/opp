use serde::Serialize;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::process::Command;
use std::thread;
use std::time::Duration;

#[derive(Serialize)]
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

fn main() {
    let code = run().unwrap_or_else(|error| {
        let _ = writeln!(io::stderr(), "fake-op: {error}");
        125
    });
    std::process::exit(code);
}

fn run() -> io::Result<i32> {
    let arguments: Vec<OsString> = std::env::args_os().skip(1).collect();
    let invocation = invocation(&arguments)?;
    append_log(&invocation)?;

    if arguments_equal(&arguments, &[b"whoami"]) {
        delay_from("FAKE_OP_WHOAMI_DELAY_MS");
        probe_output()?;
        return Ok(exit_from("FAKE_OP_WHOAMI_EXIT"));
    }
    if arguments_equal(&arguments, &[b"vault", b"list", b"--format=json"]) {
        delay_from("FAKE_OP_VAULT_DELAY_MS");
        probe_output()?;
        return Ok(exit_from("FAKE_OP_VAULT_EXIT"));
    }

    match arguments.first().map(|value| value.as_os_str().as_bytes()) {
        Some(b"fixture-streams") => fixture_streams(),
        Some(b"fixture-print-args") => {
            let encoded: Vec<_> = arguments[1..]
                .iter()
                .map(|value| hex(value.as_os_str().as_bytes()))
                .collect();
            serde_json::to_writer(io::stdout().lock(), &encoded).map_err(io::Error::other)?;
            Ok(0)
        }
        Some(b"fixture-env") => {
            serde_json::to_writer(io::stdout().lock(), &invocation).map_err(io::Error::other)?;
            Ok(0)
        }
        Some(b"fixture-exit") => Ok(argument_i32(&arguments, 1).unwrap_or(0)),
        Some(b"fixture-signal") => {
            let signal = argument_i32(&arguments, 1).unwrap_or(libc::SIGTERM);
            // SAFETY: The integration test supplies a valid signal number and expects default termination.
            unsafe {
                libc::raise(signal);
            }
            Ok(0)
        }
        Some(b"fixture-sleep") => {
            thread::sleep(Duration::from_millis(
                argument_u64(&arguments, 1).unwrap_or(100),
            ));
            Ok(0)
        }
        Some(b"fixture-ignore-term") => {
            ignore_term();
            thread::sleep(Duration::from_millis(
                argument_u64(&arguments, 1).unwrap_or(10_000),
            ));
            Ok(0)
        }
        Some(b"fixture-descendant") => {
            ignore_term();
            let child = std::env::current_exe()?;
            let mut child = Command::new(child)
                .arg("fixture-ignore-term")
                .arg("10000")
                .spawn()?;
            thread::sleep(Duration::from_secs(10));
            let _ = child.wait();
            Ok(0)
        }
        _ => Ok(0),
    }
}

fn invocation(arguments: &[OsString]) -> io::Result<Invocation> {
    let environment: Vec<_> = std::env::vars_os().collect();
    let mut environment_keys_hex: Vec<_> = environment
        .iter()
        .map(|(key, _)| hex(key.as_os_str().as_bytes()))
        .collect();
    environment_keys_hex.sort();
    let value = |key: &[u8]| {
        environment
            .iter()
            .find(|(candidate, _)| candidate.as_os_str().as_bytes() == key)
            .map(|(_, value)| hex(value.as_os_str().as_bytes()))
    };
    let has_key = |predicate: &dyn Fn(&[u8]) -> bool| {
        environment
            .iter()
            .any(|(key, _)| predicate(key.as_os_str().as_bytes()))
    };
    let terminal = File::open("/dev/tty").ok();
    let tty_device = terminal
        .as_ref()
        .and_then(|file| file.metadata().ok())
        .map(|metadata| metadata.rdev());
    let foreground_group = terminal.as_ref().and_then(|file| {
        // SAFETY: `file` owns a valid descriptor for the controlling terminal query.
        let group = unsafe { libc::tcgetpgrp(file.as_raw_fd()) };
        (group >= 0).then_some(group)
    });
    Ok(Invocation {
        arguments_hex: arguments
            .iter()
            .map(|value| hex(value.as_os_str().as_bytes()))
            .collect(),
        cwd_hex: hex(std::env::current_dir()?.as_os_str().as_bytes()),
        environment_keys_hex,
        op_account_hex: value(b"OP_ACCOUNT"),
        biometric_hex: value(b"OP_BIOMETRIC_UNLOCK_ENABLED"),
        fixture_canary_hex: value(b"FIXTURE_CANARY"),
        has_op_session: has_key(&|key| key == b"OP_SESSION" || key.starts_with(b"OP_SESSION_")),
        has_service_account: has_key(&|key| key == b"OP_SERVICE_ACCOUNT_TOKEN"),
        has_connect: has_key(&|key| key == b"OP_CONNECT_HOST" || key == b"OP_CONNECT_TOKEN"),
        has_opp_variable: has_key(&|key| key.starts_with(b"OPP_")),
        pid: std::process::id(),
        // SAFETY: These process-query functions have no pointer arguments and cannot violate memory safety.
        process_group: unsafe { libc::getpgrp() },
        // SAFETY: A pid of zero queries the calling process's session.
        session: unsafe { libc::getsid(0) },
        tty_device: tty_device.and_then(|value| u32::try_from(value).ok()),
        foreground_group,
    })
}

fn append_log(invocation: &Invocation) -> io::Result<()> {
    let executable = std::env::current_exe()?;
    let path = executable
        .parent()
        .map(|directory| directory.join("fake-op.log"))
        .ok_or_else(|| io::Error::other("fake executable has no parent"))?;
    let mut line = serde_json::to_vec(invocation).map_err(io::Error::other)?;
    line.push(b'\n');
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(&line)
}

fn arguments_equal(arguments: &[OsString], expected: &[&[u8]]) -> bool {
    arguments.len() == expected.len()
        && arguments
            .iter()
            .zip(expected)
            .all(|(actual, expected)| actual.as_os_str().as_bytes() == *expected)
}

fn probe_output() -> io::Result<()> {
    io::stdout().write_all(b"probe-stdout-must-be-discarded\n")?;
    io::stderr().write_all(b"probe-stderr-must-be-discarded\n")
}

fn fixture_streams() -> io::Result<i32> {
    let mut input = Vec::new();
    io::stdin().read_to_end(&mut input)?;
    let stderr = thread::spawn(|| -> io::Result<()> {
        let mut stderr = io::stderr().lock();
        for _ in 0..32_768 {
            stderr.write_all(b"err\0\xff")?;
        }
        Ok(())
    });
    let mut stdout = io::stdout().lock();
    for _ in 0..32_768 {
        stdout.write_all(b"out\0\xfe")?;
    }
    stdout.write_all(&input)?;
    stderr
        .join()
        .map_err(|_| io::Error::other("stderr writer panicked"))??;
    Ok(0)
}

fn delay_from(key: &str) {
    if let Ok(value) = std::env::var(key)
        && let Ok(milliseconds) = value.parse::<u64>()
    {
        thread::sleep(Duration::from_millis(milliseconds));
    }
}

fn exit_from(key: &str) -> i32 {
    std::env::var(key)
        .ok()
        .and_then(|value| value.parse::<i32>().ok())
        .unwrap_or(0)
}

fn argument_i32(arguments: &[OsString], index: usize) -> Option<i32> {
    argument_text(arguments, index)?.parse().ok()
}

fn argument_u64(arguments: &[OsString], index: usize) -> Option<u64> {
    argument_text(arguments, index)?.parse().ok()
}

fn argument_text(arguments: &[OsString], index: usize) -> Option<&str> {
    arguments.get(index)?.to_str()
}

fn ignore_term() {
    // SAFETY: Installing `SIG_IGN` for SIGTERM is valid for this single-threaded test operation.
    unsafe {
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(DIGITS[usize::from(byte >> 4)]));
        encoded.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    encoded
}
