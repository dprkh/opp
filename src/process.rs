use crate::clock::{Clock, add, remaining};
use crate::darwin::{self, SpawnInput, Terminal};
use crate::environment::ClientContext;
use nix::sys::signal::{Signal, kill, killpg};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{Pid, pipe};
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::thread;
use std::time::Duration;

const POLL_INTERVAL: Duration = Duration::from_millis(10);
const TERMINATION_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Default)]
pub(crate) struct RequestControl {
    cancelled: AtomicBool,
    streams_closed: AtomicBool,
    signal: AtomicI32,
}

impl RequestControl {
    pub(crate) fn cancel(&self, signal: i32) {
        self.signal.store(signal, Ordering::Release);
        self.cancelled.store(true, Ordering::Release);
    }

    pub(crate) fn disconnect(&self) {
        self.streams_closed.store(true, Ordering::Release);
        self.cancelled.store(true, Ordering::Release);
    }

    pub(crate) fn close_streams(&self) {
        self.streams_closed.store(true, Ordering::Release);
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub(crate) fn signal(&self) -> i32 {
        self.signal.load(Ordering::Acquire)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProcessOutcome {
    Exited(i32),
    Signaled(i32),
    TimedOut,
    Cancelled(i32),
    Shutdown,
}

pub(crate) struct Runner<'a> {
    pub(crate) executable: &'a str,
    pub(crate) terminal: &'a Terminal,
    pub(crate) clock: &'a dyn Clock,
    pub(crate) shutdown: &'a AtomicBool,
}

impl Runner<'_> {
    pub(crate) fn run(
        &self,
        arguments: &[Vec<u8>],
        context: &ClientContext,
        environment: &[(Vec<u8>, Vec<u8>)],
        descriptors: [OwnedFd; 3],
        deadline: Option<u64>,
        control: &RequestControl,
    ) -> io::Result<ProcessOutcome> {
        let stdio = descriptors.each_ref().map(AsRawFd::as_raw_fd);
        let pid = darwin::spawn_suspended(&SpawnInput {
            executable: self.executable,
            arguments,
            environment,
            cwd: &context.cwd,
            stdio,
        })?;
        drop(descriptors);

        if let Err(error) = self.terminal.foreground(pid) {
            let _ = killpg(pid, Signal::SIGKILL);
            let _ = waitpid(pid, None);
            return Err(error);
        }
        if let Err(error) = kill(pid, Signal::SIGCONT) {
            let _ = killpg(pid, Signal::SIGKILL);
            let _ = waitpid(pid, None);
            let _ = self.terminal.restore_foreground();
            return Err(io::Error::from_raw_os_error(error as i32));
        }

        let outcome = self.wait(pid, deadline, control);
        if outcome.is_err() {
            let _ = killpg(pid, Signal::SIGKILL);
            let _ = waitpid(pid, None);
        }
        let foreground_result = self.terminal.restore_foreground();
        match (outcome, foreground_result) {
            (Ok(outcome), Ok(())) => Ok(outcome),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
        }
    }

    pub(crate) fn probe(
        &self,
        arguments: &[Vec<u8>],
        context: &ClientContext,
        environment: &[(Vec<u8>, Vec<u8>)],
        deadline: u64,
        external_control: &RequestControl,
    ) -> io::Result<ProcessOutcome> {
        let (stdin_read, stdin_write) = pipe().map_err(nix_error)?;
        let (stdout_read, stdout_write) = pipe().map_err(nix_error)?;
        let (stderr_read, stderr_write) = pipe().map_err(nix_error)?;
        drop(stdin_write);

        let control = Arc::new(RequestControl::default());
        let drain_control = Arc::clone(&control);
        thread::Builder::new()
            .name(String::from("opp-probe-drain"))
            .spawn(move || {
                let stdout = thread::spawn(move || discard(stdout_read));
                let stderr = thread::spawn(move || discard(stderr_read));
                let _ = stdout.join();
                let _ = stderr.join();
                drain_control.close_streams();
            })?;

        let descriptors = [stdin_read, stdout_write, stderr_write];
        let stdio = descriptors.each_ref().map(AsRawFd::as_raw_fd);
        let pid = darwin::spawn_suspended(&SpawnInput {
            executable: self.executable,
            arguments,
            environment,
            cwd: &context.cwd,
            stdio,
        })?;
        drop(descriptors);

        if let Err(error) = self.terminal.foreground(pid) {
            let _ = killpg(pid, Signal::SIGKILL);
            let _ = waitpid(pid, None);
            return Err(error);
        }
        if let Err(error) = kill(pid, Signal::SIGCONT) {
            let _ = killpg(pid, Signal::SIGKILL);
            let _ = waitpid(pid, None);
            let _ = self.terminal.restore_foreground();
            return Err(io::Error::from_raw_os_error(error as i32));
        }

        let outcome = self.wait_probe(pid, deadline, external_control, &control);
        if outcome.is_err() {
            let _ = killpg(pid, Signal::SIGKILL);
            let _ = waitpid(pid, None);
        }
        let foreground_result = self.terminal.restore_foreground();
        match (outcome, foreground_result) {
            (Ok(outcome), Ok(())) => Ok(outcome),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
        }
    }

    fn wait(
        &self,
        pid: Pid,
        deadline: Option<u64>,
        control: &RequestControl,
    ) -> io::Result<ProcessOutcome> {
        self.wait_inner(pid, deadline, control, || {
            control.streams_closed.load(Ordering::Acquire)
        })
    }

    fn wait_probe(
        &self,
        pid: Pid,
        deadline: u64,
        external: &RequestControl,
        streams: &RequestControl,
    ) -> io::Result<ProcessOutcome> {
        self.wait_inner(pid, Some(deadline), external, || {
            streams.streams_closed.load(Ordering::Acquire)
        })
    }

    fn wait_inner<F>(
        &self,
        pid: Pid,
        deadline: Option<u64>,
        control: &RequestControl,
        streams_closed: F,
    ) -> io::Result<ProcessOutcome>
    where
        F: Fn() -> bool,
    {
        let mut status = None;
        let mut termination = None;
        let mut kill_deadline = None;

        loop {
            if status.is_none() {
                status = match waitpid(pid, Some(WaitPidFlag::WNOHANG)).map_err(nix_error)? {
                    WaitStatus::StillAlive => None,
                    value => Some(value),
                };
            }

            if termination.is_none() {
                if self.shutdown.load(Ordering::Acquire) {
                    termination = Some(ProcessOutcome::Shutdown);
                } else if control.is_cancelled() {
                    termination = Some(ProcessOutcome::Cancelled(control.signal()));
                } else if deadline.is_some_and(|value| self.clock.monotonic() >= value) {
                    termination = Some(ProcessOutcome::TimedOut);
                }
                if termination.is_some() {
                    terminate_group(pid, Signal::SIGTERM)?;
                    kill_deadline = Some(add(self.clock.monotonic(), TERMINATION_GRACE));
                }
            } else if kill_deadline.is_some_and(|value| self.clock.monotonic() >= value) {
                terminate_group(pid, Signal::SIGKILL)?;
                kill_deadline = None;
            }

            if let Some(status) = status
                && (streams_closed() || self.shutdown.load(Ordering::Acquire))
                && (termination.is_none() || !process_group_exists(pid)?)
            {
                return Ok(termination.unwrap_or_else(|| map_wait_status(status)));
            }

            let sleep = deadline
                .map(|value| remaining(self.clock.monotonic(), value).min(POLL_INTERVAL))
                .unwrap_or(POLL_INTERVAL);
            thread::sleep(sleep.max(Duration::from_millis(1)));
        }
    }
}

fn discard(descriptor: OwnedFd) {
    let mut file = File::from(descriptor);
    let mut buffer = [0_u8; 8192];
    loop {
        match file.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
}

fn terminate_group(pid: Pid, signal: Signal) -> io::Result<()> {
    match killpg(pid, signal) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
        Err(error) => Err(nix_error(error)),
    }
}

fn process_group_exists(pid: Pid) -> io::Result<bool> {
    // SAFETY: Signal zero performs an existence/permission check and sends no signal.
    let result = unsafe { libc::kill(-pid.as_raw(), 0) };
    if result == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        Some(libc::EPERM) => Ok(true),
        _ => Err(error),
    }
}

fn map_wait_status(status: WaitStatus) -> ProcessOutcome {
    match status {
        WaitStatus::Exited(_, code) => ProcessOutcome::Exited(code),
        WaitStatus::Signaled(_, signal, _) => ProcessOutcome::Signaled(signal as i32),
        _ => ProcessOutcome::Signaled(libc::SIGKILL),
    }
}

fn nix_error(error: nix::errno::Errno) -> io::Error {
    io::Error::from_raw_os_error(error as i32)
}
