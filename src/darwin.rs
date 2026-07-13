use nix::fcntl::{FcntlArg, OFlag, fcntl};
use nix::pty::openpty;
use nix::sys::signal::{self, SigHandler, Signal};
use nix::sys::socket::{getsockopt, sockopt};
use nix::unistd::{Pid, getpgrp, setsid, tcsetpgrp};
use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

unsafe extern "C" {
    fn posix_spawn_file_actions_addchdir_np(
        actions: *mut libc::posix_spawn_file_actions_t,
        path: *const libc::c_char,
    ) -> libc::c_int;
}

pub(crate) fn prepare_broker_process() -> io::Result<()> {
    setsid().map_err(nix_error)?;
    // SAFETY: Changing these dispositions is process-wide and happens before broker worker threads start.
    unsafe {
        signal::signal(Signal::SIGHUP, SigHandler::SigIgn).map_err(nix_error)?;
        signal::signal(Signal::SIGTTOU, SigHandler::SigIgn).map_err(nix_error)?;
        signal::signal(Signal::SIGTTIN, SigHandler::SigIgn).map_err(nix_error)?;
        signal::signal(Signal::SIGPIPE, SigHandler::SigIgn).map_err(nix_error)?;
    }
    Ok(())
}

pub(crate) fn verify_peer(stream: &UnixStream, expected_uid: u32) -> io::Result<()> {
    let credentials = getsockopt(stream, sockopt::LocalPeerCred).map_err(nix_error)?;
    if credentials.uid() != expected_uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "broker peer user mismatch",
        ));
    }
    Ok(())
}

pub(crate) struct Terminal {
    slave: OwnedFd,
    broker_group: Pid,
    stop: Arc<AtomicBool>,
    drain: Option<JoinHandle<()>>,
}

impl Terminal {
    pub(crate) fn acquire() -> io::Result<Self> {
        let pair = openpty(None, None).map_err(nix_error)?;
        // SAFETY: The broker is a session leader without a controlling terminal, and `slave` is a PTY slave.
        if unsafe {
            libc::ioctl(
                pair.slave.as_raw_fd(),
                libc::c_ulong::from(libc::TIOCSCTTY),
                0,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        let broker_group = getpgrp();
        tcsetpgrp(&pair.slave, broker_group).map_err(nix_error)?;

        let current = fcntl(&pair.master, FcntlArg::F_GETFL).map_err(nix_error)?;
        let flags = OFlag::from_bits_truncate(current) | OFlag::O_NONBLOCK;
        fcntl(&pair.master, FcntlArg::F_SETFL(flags)).map_err(nix_error)?;

        let stop = Arc::new(AtomicBool::new(false));
        let drain_stop = Arc::clone(&stop);
        let drain = thread::Builder::new()
            .name(String::from("opp-pty-drain"))
            .spawn(move || drain_terminal(pair.master, &drain_stop))?;

        Ok(Self {
            slave: pair.slave,
            broker_group,
            stop,
            drain: Some(drain),
        })
    }

    pub(crate) fn foreground(&self, group: Pid) -> io::Result<()> {
        tcsetpgrp(&self.slave, group).map_err(nix_error)
    }

    pub(crate) fn restore_foreground(&self) -> io::Result<()> {
        self.foreground(self.broker_group)
    }

    pub(crate) fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(drain) = self.drain.take() {
            let _ = drain.join();
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn drain_terminal(master: OwnedFd, stop: &AtomicBool) {
    let mut file = File::from(master);
    let mut buffer = [0_u8; 8192];
    while !stop.load(Ordering::Acquire) {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) if error.raw_os_error() == Some(libc::EIO) => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(_) => break,
        }
    }
}

pub(crate) struct SpawnInput<'a> {
    pub(crate) executable: &'a str,
    pub(crate) arguments: &'a [Vec<u8>],
    pub(crate) environment: &'a [(Vec<u8>, Vec<u8>)],
    pub(crate) cwd: &'a [u8],
    pub(crate) stdio: [RawFd; 3],
}

pub(crate) fn spawn_suspended(input: &SpawnInput<'_>) -> io::Result<Pid> {
    let executable = CString::new(input.executable.as_bytes()).map_err(nul_error)?;
    let cwd = CString::new(input.cwd).map_err(nul_error)?;

    let mut arguments = Vec::with_capacity(input.arguments.len() + 1);
    arguments.push(CString::new(input.executable.as_bytes()).map_err(nul_error)?);
    for argument in input.arguments {
        arguments.push(CString::new(argument.as_slice()).map_err(nul_error)?);
    }
    let mut argument_pointers: Vec<*mut libc::c_char> = arguments
        .iter()
        .map(|value| value.as_ptr().cast_mut())
        .collect();
    argument_pointers.push(std::ptr::null_mut());

    let mut environment = Vec::with_capacity(input.environment.len());
    for (key, value) in input.environment {
        let mut entry = Vec::with_capacity(key.len() + value.len() + 1);
        entry.extend_from_slice(key);
        entry.push(b'=');
        entry.extend_from_slice(value);
        environment.push(CString::new(entry).map_err(nul_error)?);
    }
    let mut environment_pointers: Vec<*mut libc::c_char> = environment
        .iter()
        .map(|value| value.as_ptr().cast_mut())
        .collect();
    environment_pointers.push(std::ptr::null_mut());

    let mut actions = SpawnActions::new()?;
    for (source, target) in input.stdio.into_iter().zip(0..=2) {
        actions.dup2(source, target)?;
    }
    actions.chdir(&cwd)?;

    let mut attributes = SpawnAttributes::new()?;
    attributes.configure()?;

    let mut pid = 0;
    // SAFETY: All C strings and pointer arrays remain alive through the call and are NUL-terminated.
    let result = unsafe {
        libc::posix_spawn(
            &raw mut pid,
            executable.as_ptr(),
            actions.as_ptr(),
            attributes.as_ptr(),
            argument_pointers.as_ptr(),
            environment_pointers.as_ptr(),
        )
    };
    cvt_spawn(result)?;
    Ok(Pid::from_raw(pid))
}

struct SpawnActions {
    value: libc::posix_spawn_file_actions_t,
}

impl SpawnActions {
    fn new() -> io::Result<Self> {
        let mut value = MaybeUninit::uninit();
        // SAFETY: `value` points to uninitialized storage intended for this initializer.
        cvt_spawn(unsafe { libc::posix_spawn_file_actions_init(value.as_mut_ptr()) })?;
        // SAFETY: The initializer succeeded.
        Ok(Self {
            value: unsafe { value.assume_init() },
        })
    }

    fn dup2(&mut self, source: RawFd, target: RawFd) -> io::Result<()> {
        // SAFETY: `self.value` is initialized and both descriptors are plain integers consumed by spawn.
        cvt_spawn(unsafe {
            libc::posix_spawn_file_actions_adddup2(&raw mut self.value, source, target)
        })
    }

    fn chdir(&mut self, path: &CString) -> io::Result<()> {
        // SAFETY: `self.value` is initialized and `path` is a live NUL-terminated string.
        cvt_spawn(unsafe {
            posix_spawn_file_actions_addchdir_np(&raw mut self.value, path.as_ptr())
        })
    }

    fn as_ptr(&self) -> *const libc::posix_spawn_file_actions_t {
        &raw const self.value
    }
}

impl Drop for SpawnActions {
    fn drop(&mut self) {
        // SAFETY: `self.value` was successfully initialized and has not been destroyed.
        let _ = unsafe { libc::posix_spawn_file_actions_destroy(&raw mut self.value) };
    }
}

struct SpawnAttributes {
    value: libc::posix_spawnattr_t,
}

impl SpawnAttributes {
    fn new() -> io::Result<Self> {
        let mut value = MaybeUninit::uninit();
        // SAFETY: `value` points to uninitialized storage intended for this initializer.
        cvt_spawn(unsafe { libc::posix_spawnattr_init(value.as_mut_ptr()) })?;
        // SAFETY: The initializer succeeded.
        Ok(Self {
            value: unsafe { value.assume_init() },
        })
    }

    fn configure(&mut self) -> io::Result<()> {
        // SAFETY: The spawn attribute object is initialized.
        cvt_spawn(unsafe { libc::posix_spawnattr_setpgroup(&raw mut self.value, 0) })?;

        let mut empty = MaybeUninit::<libc::sigset_t>::uninit();
        // SAFETY: `empty` is writable storage for a signal set.
        if unsafe { libc::sigemptyset(empty.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `sigemptyset` initialized the value.
        let empty = unsafe { empty.assume_init() };

        let mut defaults = empty;
        for signal in [
            libc::SIGHUP,
            libc::SIGINT,
            libc::SIGQUIT,
            libc::SIGPIPE,
            libc::SIGTERM,
            libc::SIGTSTP,
            libc::SIGTTIN,
            libc::SIGTTOU,
        ] {
            // SAFETY: `defaults` is an initialized signal set and `signal` is valid.
            if unsafe { libc::sigaddset(&raw mut defaults, signal) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }

        // SAFETY: All pointers reference initialized spawn attributes and signal sets.
        cvt_spawn(unsafe {
            libc::posix_spawnattr_setsigmask(&raw mut self.value, &raw const empty)
        })?;
        // SAFETY: All pointers reference initialized spawn attributes and signal sets.
        cvt_spawn(unsafe {
            libc::posix_spawnattr_setsigdefault(&raw mut self.value, &raw const defaults)
        })?;

        let flags = libc::POSIX_SPAWN_SETPGROUP
            | libc::POSIX_SPAWN_SETSIGDEF
            | libc::POSIX_SPAWN_SETSIGMASK
            | libc::POSIX_SPAWN_START_SUSPENDED
            | libc::POSIX_SPAWN_CLOEXEC_DEFAULT;
        let flags = i16::try_from(flags).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "spawn flags do not fit c_short",
            )
        })?;
        // SAFETY: The spawn attribute object is initialized.
        cvt_spawn(unsafe { libc::posix_spawnattr_setflags(&raw mut self.value, flags) })
    }

    fn as_ptr(&self) -> *const libc::posix_spawnattr_t {
        &raw const self.value
    }
}

impl Drop for SpawnAttributes {
    fn drop(&mut self) {
        // SAFETY: `self.value` was successfully initialized and has not been destroyed.
        let _ = unsafe { libc::posix_spawnattr_destroy(&raw mut self.value) };
    }
}

fn cvt_spawn(result: libc::c_int) -> io::Result<()> {
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(result))
    }
}

fn nul_error(_: std::ffi::NulError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "process value contains NUL")
}

fn nix_error(error: nix::errno::Errno) -> io::Error {
    io::Error::from_raw_os_error(error as i32)
}
