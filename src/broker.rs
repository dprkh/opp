use crate::account::Account;
use crate::clock::{Clock, SystemClock, add, remaining};
use crate::darwin::{self, Terminal};
use crate::environment::ClientContext;
use crate::process::{ProcessOutcome, RequestControl, Runner};
use crate::protocol::{
    self, ClientEvent, Diagnostic, ErrorCode, Request, Response, ServerPreamble,
};
use crate::runtime::{BrokerLock, EffectiveUser, RuntimePaths};
use crate::status::{Status, format_time};
use std::collections::HashMap;
use std::fs;
#[cfg(feature = "test-support")]
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

pub(crate) const INTERNAL_BROKER_ARGUMENT: &str = "__opp_internal_broker_v1";
const STARTUP_MAGIC: &[u8; 8] = b"OPPCFG1\0";
const STARTUP_READY: u8 = 1;
const STARTUP_EXISTS: u8 = 2;
const STARTUP_ERROR: u8 = 3;
const MAX_CONNECTIONS: usize = 64;
const DEFAULT_EXPLICIT_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_CHECK_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_MAINTENANCE_INTERVAL: Duration = Duration::from_secs(5 * 60);
const DEFAULT_HARD_LIMIT: Duration = Duration::from_secs(12 * 60 * 60);

#[derive(Clone, Copy)]
struct BrokerTimings {
    explicit_timeout: Duration,
    check_timeout: Duration,
    maintenance_interval: Duration,
    hard_limit: Duration,
}

impl BrokerTimings {
    #[cfg(feature = "test-support")]
    fn load() -> Self {
        Self {
            explicit_timeout: test_duration(
                "OPP_TEST_EXPLICIT_TIMEOUT_MS",
                DEFAULT_EXPLICIT_TIMEOUT,
            ),
            check_timeout: test_duration("OPP_TEST_CHECK_TIMEOUT_MS", DEFAULT_CHECK_TIMEOUT),
            maintenance_interval: test_duration(
                "OPP_TEST_MAINTENANCE_INTERVAL_MS",
                DEFAULT_MAINTENANCE_INTERVAL,
            ),
            hard_limit: test_duration("OPP_TEST_HARD_LIMIT_MS", DEFAULT_HARD_LIMIT),
        }
    }

    #[cfg(not(feature = "test-support"))]
    fn load() -> Self {
        Self {
            explicit_timeout: DEFAULT_EXPLICIT_TIMEOUT,
            check_timeout: DEFAULT_CHECK_TIMEOUT,
            maintenance_interval: DEFAULT_MAINTENANCE_INTERVAL,
            hard_limit: DEFAULT_HARD_LIMIT,
        }
    }
}

#[cfg(feature = "test-support")]
fn test_duration(name: &str, default: Duration) -> Duration {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .map(Duration::from_millis)
        .unwrap_or(default)
}

pub(crate) enum StartupResult {
    Ready,
    Existing,
    Failed,
}

pub(crate) fn write_startup_configuration(
    stream: &mut UnixStream,
    op_path: &str,
) -> io::Result<()> {
    stream.write_all(STARTUP_MAGIC)?;
    let length = u32::try_from(op_path.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "op path is too long"))?;
    stream.write_all(&length.to_be_bytes())?;
    stream.write_all(op_path.as_bytes())
}

pub(crate) fn read_startup_result(stream: &mut UnixStream) -> io::Result<StartupResult> {
    let mut value = [0_u8; 1];
    stream.read_exact(&mut value)?;
    match value[0] {
        STARTUP_READY => Ok(StartupResult::Ready),
        STARTUP_EXISTS => Ok(StartupResult::Existing),
        STARTUP_ERROR => Ok(StartupResult::Failed),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid broker startup response",
        )),
    }
}

pub(crate) fn run_internal() -> i32 {
    match run_internal_result() {
        Ok(()) => 0,
        Err(error) => {
            #[cfg(feature = "test-support")]
            report_test_error(&error);
            #[cfg(not(feature = "test-support"))]
            let _ = error;
            1
        }
    }
}

fn run_internal_result() -> io::Result<()> {
    darwin::prepare_broker_process()?;
    // SAFETY: Internal broker mode receives a full-duplex Unix socket as descriptor zero.
    let mut startup = unsafe { UnixStream::from_raw_fd(0) };
    let op_path = read_startup_configuration(&mut startup)?;

    let user = EffectiveUser::load()?;
    let paths = RuntimePaths::load()?;
    paths.prepare_directory(&user)?;
    let lock = match paths.acquire_lock(&user) {
        Ok(lock) => lock,
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN
            ) =>
        {
            startup.write_all(&[STARTUP_EXISTS])?;
            return Ok(());
        }
        Err(error) => {
            let _ = startup.write_all(&[STARTUP_ERROR]);
            return Err(error);
        }
    };

    let terminal = match Terminal::acquire() {
        Ok(terminal) => terminal,
        Err(error) => {
            let _ = startup.write_all(&[STARTUP_ERROR]);
            return Err(error);
        }
    };
    if let Err(error) = paths.remove_stale_socket(&user) {
        let _ = startup.write_all(&[STARTUP_ERROR]);
        return Err(error);
    }
    let listener = match UnixListener::bind(&paths.socket) {
        Ok(listener) => listener,
        Err(error) => {
            let _ = startup.write_all(&[STARTUP_ERROR]);
            return Err(error);
        }
    };
    fs::set_permissions(&paths.socket, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;

    let shutdown = Arc::new(AtomicBool::new(false));
    let stop_requested = Arc::new(AtomicBool::new(false));
    let complete = Arc::new((Mutex::new(false), Condvar::new()));
    let socket_removed = Arc::new((Mutex::new(false), Condvar::new()));
    let stop_acknowledged = Arc::new((Mutex::new(false), Condvar::new()));
    let (sender, receiver) = mpsc::sync_channel(MAX_CONNECTIONS);
    let actor_shutdown = Arc::clone(&shutdown);
    let actor_complete = Arc::clone(&complete);
    let actor_user = user.clone();
    let actor_op_path = op_path.clone();
    let actor = thread::Builder::new()
        .name(String::from("opp-executor"))
        .spawn(move || {
            BrokerActor::new(
                actor_op_path,
                actor_user,
                terminal,
                lock,
                actor_shutdown,
                actor_complete,
            )
            .run(receiver);
        })?;

    startup.write_all(&[STARTUP_READY])?;
    drop(startup);

    let active_connections = Arc::new(AtomicUsize::new(0));
    while !shutdown.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false)?;
                if active_connections.fetch_add(1, Ordering::AcqRel) >= MAX_CONNECTIONS {
                    active_connections.fetch_sub(1, Ordering::AcqRel);
                    drop(stream);
                    continue;
                }
                let context = ConnectionContext {
                    expected_uid: user.uid.as_raw(),
                    sender: sender.clone(),
                    shutdown: Arc::clone(&shutdown),
                    stop_requested: Arc::clone(&stop_requested),
                    complete: Arc::clone(&complete),
                    socket_removed: Arc::clone(&socket_removed),
                    stop_acknowledged: Arc::clone(&stop_acknowledged),
                };
                let handler_connections = Arc::clone(&active_connections);
                thread::Builder::new()
                    .name(String::from("opp-client"))
                    .spawn(move || {
                        let result = handle_connection(stream, &context);
                        #[cfg(feature = "test-support")]
                        if let Err(error) = &result {
                            report_test_error(error);
                        }
                        let _ = result;
                        handler_connections.fetch_sub(1, Ordering::AcqRel);
                    })?;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                shutdown.store(true, Ordering::Release);
                let _ = sender.send(ActorMessage::Stop);
                return Err(error);
            }
        }
    }

    drop(listener);
    let _ = fs::remove_file(&paths.socket);
    set_flag(&socket_removed);
    wait_flag(&complete);
    if stop_requested.load(Ordering::Acquire) {
        wait_flag(&stop_acknowledged);
    }
    let _ = actor.join();
    Ok(())
}

fn read_startup_configuration(stream: &mut UnixStream) -> io::Result<String> {
    let mut magic = [0_u8; 8];
    stream.read_exact(&mut magic)?;
    if &magic != STARTUP_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid broker startup configuration",
        ));
    }
    let mut length = [0_u8; 4];
    stream.read_exact(&mut length)?;
    let length = usize::try_from(u32::from_be_bytes(length)).expect("u32 fits usize");
    if length == 0 || length > protocol::MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid broker op path length",
        ));
    }
    let mut path = vec![0_u8; length];
    stream.read_exact(&mut path)?;
    String::from_utf8(path)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid broker op path"))
}

struct ConnectionContext {
    expected_uid: u32,
    sender: SyncSender<ActorMessage>,
    shutdown: Arc<AtomicBool>,
    stop_requested: Arc<AtomicBool>,
    complete: Arc<(Mutex<bool>, Condvar)>,
    socket_removed: Arc<(Mutex<bool>, Condvar)>,
    stop_acknowledged: Arc<(Mutex<bool>, Condvar)>,
}

fn handle_connection(mut stream: UnixStream, context: &ConnectionContext) -> io::Result<()> {
    darwin::verify_peer(&stream, context.expected_uid)?;
    match protocol::server_preamble(&mut stream)? {
        ServerPreamble::Stop => {
            context.stop_requested.store(true, Ordering::Release);
            context.shutdown.store(true, Ordering::Release);
            let _ = context.sender.send(ActorMessage::Stop);
            wait_flag(&context.complete);
            wait_flag(&context.socket_removed);
            let response = stream.write_all(protocol::STOP_RESPONSE);
            set_flag(&context.stop_acknowledged);
            return response;
        }
        ServerPreamble::Incompatible => return Ok(()),
        ServerPreamble::Normal => {}
    }

    let request = protocol::receive_request(&mut stream)?;
    if matches!(
        request,
        Request::Start {
            warning_shown: false,
            ..
        }
    ) {
        let response = submit_without_monitor(&context.sender, request)?;
        protocol::send_response(&mut stream, &response)?;
        if response != Response::NeedWarning {
            return Ok(());
        }
        let request = protocol::receive_request(&mut stream)?;
        if !matches!(
            request,
            Request::Start {
                warning_shown: true,
                ..
            }
        ) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected warned start request",
            ));
        }
        return submit_with_monitor(stream, &context.sender, request);
    }
    submit_with_monitor(stream, &context.sender, request)
}

fn submit_without_monitor(
    sender: &SyncSender<ActorMessage>,
    request: Request,
) -> io::Result<Response> {
    let (response_sender, response_receiver) = mpsc::channel();
    let control = Arc::new(RequestControl::default());
    submit(
        sender,
        RequestEnvelope {
            request,
            response: response_sender,
            control,
        },
    )?;
    response_receiver
        .recv()
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "broker actor stopped"))
}

fn submit_with_monitor(
    mut stream: UnixStream,
    sender: &SyncSender<ActorMessage>,
    request: Request,
) -> io::Result<()> {
    let (response_sender, response_receiver) = mpsc::channel();
    let control = Arc::new(RequestControl::default());
    let monitor_control = Arc::clone(&control);
    let mut monitor_stream = stream.try_clone()?;
    thread::Builder::new()
        .name(String::from("opp-client-monitor"))
        .spawn(move || {
            loop {
                match protocol::receive_client_event(&mut monitor_stream) {
                    Ok(ClientEvent::StreamsClosed) => monitor_control.close_streams(),
                    Ok(ClientEvent::Cancel(signal)) => monitor_control.cancel(signal),
                    Err(_) => {
                        monitor_control.disconnect();
                        break;
                    }
                }
            }
        })?;

    submit(
        sender,
        RequestEnvelope {
            request,
            response: response_sender,
            control,
        },
    )?;
    let response = response_receiver
        .recv()
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "broker actor stopped"))?;
    protocol::send_response(&mut stream, &response)
}

fn submit(sender: &SyncSender<ActorMessage>, envelope: RequestEnvelope) -> io::Result<()> {
    match sender.try_send(ActorMessage::Request(envelope)) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(ActorMessage::Request(envelope))) => {
            let _ = envelope
                .response
                .send(Response::Error(ErrorCode::QueueFull));
            Ok(())
        }
        Err(TrySendError::Disconnected(_)) => Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "broker actor stopped",
        )),
        Err(TrySendError::Full(ActorMessage::Stop)) => unreachable!(),
    }
}

struct RequestEnvelope {
    request: Request,
    response: mpsc::Sender<Response>,
    control: Arc<RequestControl>,
}

enum ActorMessage {
    Request(RequestEnvelope),
    Stop,
}

enum AuthorizationRecord {
    Required,
    Active(ActiveAuthorization),
}

struct ActiveAuthorization {
    authorized_at: SystemTime,
    hard_expires_at: SystemTime,
    hard_deadline: u64,
    next_probe_at: SystemTime,
    next_probe_deadline: u64,
}

struct BrokerActor {
    op_path: String,
    user: EffectiveUser,
    terminal: Terminal,
    _lock: BrokerLock,
    clock: Arc<dyn Clock>,
    timings: BrokerTimings,
    shutdown: Arc<AtomicBool>,
    complete: Arc<(Mutex<bool>, Condvar)>,
    started_at: SystemTime,
    records: HashMap<Account, AuthorizationRecord>,
}

impl BrokerActor {
    fn new(
        op_path: String,
        user: EffectiveUser,
        terminal: Terminal,
        lock: BrokerLock,
        shutdown: Arc<AtomicBool>,
        complete: Arc<(Mutex<bool>, Condvar)>,
    ) -> Self {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let started_at = clock.wall();
        Self {
            op_path,
            user,
            terminal,
            _lock: lock,
            clock,
            timings: BrokerTimings::load(),
            shutdown,
            complete,
            started_at,
            records: HashMap::new(),
        }
    }

    fn run(mut self, receiver: Receiver<ActorMessage>) {
        while !self.shutdown.load(Ordering::Acquire) {
            self.expire_records();
            match receiver.try_recv() {
                Ok(ActorMessage::Request(request)) => self.handle(request),
                Ok(ActorMessage::Stop) | Err(TryRecvError::Disconnected) => break,
                Err(TryRecvError::Empty) => {
                    if let Some(account) = self.due_maintenance() {
                        self.maintain(account);
                        continue;
                    }
                    let wait = self.next_wait().min(Duration::from_millis(100));
                    match receiver.recv_timeout(wait) {
                        Ok(ActorMessage::Request(request)) => self.handle(request),
                        Ok(ActorMessage::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                }
            }
        }
        self.shutdown.store(true, Ordering::Release);
        while let Ok(message) = receiver.try_recv() {
            if let ActorMessage::Request(request) = message {
                let _ = request
                    .response
                    .send(Response::Error(ErrorCode::Operational));
            }
        }
        self.records.clear();
        self.terminal.shutdown();
        set_flag(&self.complete);
    }

    fn handle(&mut self, envelope: RequestEnvelope) {
        if envelope.control.is_cancelled() {
            let response = if matches!(&envelope.request, Request::Exec { .. }) {
                cancel_response(&envelope.control)
            } else {
                Response::Error(ErrorCode::Cancelled)
            };
            let _ = envelope.response.send(response);
            return;
        }
        let response = match envelope.request {
            Request::Start {
                warning_shown,
                account,
                op_path,
                context,
            } => self.start(warning_shown, account, op_path, context, &envelope.control),
            Request::Status { account } => self.status(&account),
            Request::Exec {
                account,
                timeout_nanos,
                arguments,
                context,
                descriptors,
            } => self.execute(
                account,
                timeout_nanos,
                arguments,
                context,
                descriptors,
                &envelope.control,
            ),
        };
        if let Some(response) = response {
            let _ = envelope.response.send(response);
        }
    }

    fn start(
        &mut self,
        warning_shown: bool,
        account: Account,
        op_path: String,
        context: ClientContext,
        control: &RequestControl,
    ) -> Option<Response> {
        self.expire_records();
        if op_path != self.op_path {
            return Some(Response::Error(ErrorCode::OpPathMismatch));
        }
        if matches!(
            self.records.get(&account),
            Some(AuthorizationRecord::Active(_))
        ) {
            return Some(Response::Ok);
        }
        if !warning_shown {
            return Some(Response::NeedWarning);
        }

        self.records
            .insert(account.clone(), AuthorizationRecord::Required);
        let authorized_at = self.clock.wall();
        let authorized_mono = self.clock.monotonic();
        let deadline = add(authorized_mono, self.timings.explicit_timeout);
        let environment = context.sanitized(&account);
        let outcome = self.probe(
            &[
                b"vault".to_vec(),
                b"list".to_vec(),
                b"--format=json".to_vec(),
            ],
            &context,
            &environment,
            deadline,
            control,
        );
        match outcome {
            Ok(ProcessOutcome::Exited(0)) => {
                let checked_at = self.clock.wall();
                let checked_mono = self.clock.monotonic();
                self.records.insert(
                    account,
                    AuthorizationRecord::Active(ActiveAuthorization {
                        authorized_at,
                        hard_expires_at: authorized_at + self.timings.hard_limit,
                        hard_deadline: add(authorized_mono, self.timings.hard_limit),
                        next_probe_at: checked_at + self.timings.maintenance_interval,
                        next_probe_deadline: add(checked_mono, self.timings.maintenance_interval),
                    }),
                );
                Some(Response::Ok)
            }
            Ok(ProcessOutcome::Cancelled(_)) => Some(cancel_response(control)),
            Ok(ProcessOutcome::Shutdown) => None,
            Ok(_) | Err(_) => Some(Response::Error(ErrorCode::AuthorizationFailed)),
        }
    }

    fn status(&mut self, account: &Account) -> Option<Response> {
        self.expire_records();
        let mut status = Status::running(account.0.clone(), self.op_path.clone());
        status.started_at = format_time(self.started_at).ok();
        if let Some(AuthorizationRecord::Active(active)) = self.records.get(account) {
            status.authorization = Some("active");
            status.authorized_at = format_time(active.authorized_at).ok();
            status.hard_expires_at = format_time(active.hard_expires_at).ok();
            status.next_probe_at = format_time(active.next_probe_at).ok();
        }
        status
            .json()
            .map(Response::Status)
            .ok()
            .or(Some(Response::Error(ErrorCode::Operational)))
    }

    fn execute(
        &mut self,
        account: Account,
        timeout_nanos: u64,
        arguments: Vec<Vec<u8>>,
        context: ClientContext,
        descriptors: [OwnedFd; 3],
        control: &RequestControl,
    ) -> Option<Response> {
        self.expire_records();
        if !matches!(
            self.records.get(&account),
            Some(AuthorizationRecord::Active(_))
        ) {
            drop(descriptors);
            return Some(Response::Error(ErrorCode::AuthorizationRequired));
        }

        match self.automatic_check(&account, &context, control) {
            CheckResult::Success => {}
            CheckResult::Cancelled => {
                drop(descriptors);
                return Some(cancel_response(control));
            }
            CheckResult::Shutdown => {
                drop(descriptors);
                return None;
            }
            CheckResult::Failed => {
                drop(descriptors);
                self.invalidate_all();
                return Some(Response::Error(ErrorCode::AuthorizationRequired));
            }
        }
        self.expire_records();
        if !matches!(
            self.records.get(&account),
            Some(AuthorizationRecord::Active(_))
        ) {
            drop(descriptors);
            return Some(Response::Error(ErrorCode::AuthorizationRequired));
        }

        let environment = context.sanitized(&account);
        let deadline = add(self.clock.monotonic(), Duration::from_nanos(timeout_nanos));
        let runner = Runner {
            executable: &self.op_path,
            terminal: &self.terminal,
            clock: self.clock.as_ref(),
            shutdown: &self.shutdown,
        };
        let result = runner.run(
            &arguments,
            &context,
            &environment,
            descriptors,
            Some(deadline),
            control,
        );
        self.expire_records();
        match result {
            Ok(ProcessOutcome::Exited(code)) => Some(Response::Exec {
                exit_code: code,
                diagnostic: Diagnostic::None,
            }),
            Ok(ProcessOutcome::Signaled(signal)) => Some(Response::Exec {
                exit_code: 128 + signal,
                diagnostic: Diagnostic::None,
            }),
            Ok(ProcessOutcome::TimedOut) => Some(Response::Exec {
                exit_code: 124,
                diagnostic: Diagnostic::Timeout,
            }),
            Ok(ProcessOutcome::Cancelled(signal)) if signal > 0 => Some(Response::Exec {
                exit_code: 128 + signal,
                diagnostic: Diagnostic::None,
            }),
            Ok(ProcessOutcome::Cancelled(_)) => Some(Response::Error(ErrorCode::Cancelled)),
            Ok(ProcessOutcome::Shutdown) => None,
            Err(_) => Some(Response::Error(ErrorCode::ProcessStart)),
        }
    }

    fn automatic_check(
        &mut self,
        account: &Account,
        context: &ClientContext,
        control: &RequestControl,
    ) -> CheckResult {
        let deadline = add(self.clock.monotonic(), self.timings.check_timeout);
        let environment = context.sanitized(account);
        let whoami = self.probe(
            &[b"whoami".to_vec()],
            context,
            &environment,
            deadline,
            control,
        );
        match classify_check(whoami) {
            CheckResult::Success => {}
            result => return result,
        }
        let vault = self.probe(
            &[
                b"vault".to_vec(),
                b"list".to_vec(),
                b"--format=json".to_vec(),
            ],
            context,
            &environment,
            deadline,
            control,
        );
        match classify_check(vault) {
            CheckResult::Success => {
                let checked_at = self.clock.wall();
                let checked_mono = self.clock.monotonic();
                if let Some(AuthorizationRecord::Active(active)) = self.records.get_mut(account) {
                    active.next_probe_at = checked_at + self.timings.maintenance_interval;
                    active.next_probe_deadline =
                        add(checked_mono, self.timings.maintenance_interval);
                }
                CheckResult::Success
            }
            result => result,
        }
    }

    fn probe(
        &self,
        arguments: &[Vec<u8>],
        context: &ClientContext,
        environment: &[(Vec<u8>, Vec<u8>)],
        deadline: u64,
        control: &RequestControl,
    ) -> io::Result<ProcessOutcome> {
        Runner {
            executable: &self.op_path,
            terminal: &self.terminal,
            clock: self.clock.as_ref(),
            shutdown: &self.shutdown,
        }
        .probe(arguments, context, environment, deadline, control)
    }

    fn maintain(&mut self, account: Account) {
        if !matches!(
            self.records.get(&account),
            Some(AuthorizationRecord::Active(_))
        ) {
            return;
        }
        let context = ClientContext::maintenance(&self.user, &account);
        let control = RequestControl::default();
        match self.automatic_check(&account, &context, &control) {
            CheckResult::Success => {}
            CheckResult::Shutdown => self.shutdown.store(true, Ordering::Release),
            CheckResult::Cancelled | CheckResult::Failed => self.invalidate_all(),
        }
    }

    fn invalidate_all(&mut self) {
        for record in self.records.values_mut() {
            *record = AuthorizationRecord::Required;
        }
    }

    fn expire_records(&mut self) {
        let now = self.clock.monotonic();
        for record in self.records.values_mut() {
            if matches!(record, AuthorizationRecord::Active(active) if now >= active.hard_deadline)
            {
                *record = AuthorizationRecord::Required;
            }
        }
    }

    fn due_maintenance(&self) -> Option<Account> {
        let now = self.clock.monotonic();
        self.records
            .iter()
            .find_map(|(account, record)| match record {
                AuthorizationRecord::Active(active) if now >= active.next_probe_deadline => {
                    Some(account.clone())
                }
                _ => None,
            })
    }

    fn next_wait(&self) -> Duration {
        let now = self.clock.monotonic();
        self.records
            .values()
            .filter_map(|record| match record {
                AuthorizationRecord::Active(active) => {
                    Some(active.next_probe_deadline.min(active.hard_deadline))
                }
                AuthorizationRecord::Required => None,
            })
            .min()
            .map(|deadline| remaining(now, deadline))
            .unwrap_or(Duration::from_millis(100))
    }
}

#[derive(Clone, Copy)]
enum CheckResult {
    Success,
    Failed,
    Cancelled,
    Shutdown,
}

fn classify_check(result: io::Result<ProcessOutcome>) -> CheckResult {
    match result {
        Ok(ProcessOutcome::Exited(0)) => CheckResult::Success,
        Ok(ProcessOutcome::Cancelled(_)) => CheckResult::Cancelled,
        Ok(ProcessOutcome::Shutdown) => CheckResult::Shutdown,
        Ok(_) | Err(_) => CheckResult::Failed,
    }
}

fn cancel_response(control: &RequestControl) -> Response {
    let signal = control.signal();
    if signal > 0 {
        Response::Exec {
            exit_code: 128 + signal,
            diagnostic: Diagnostic::None,
        }
    } else {
        Response::Error(ErrorCode::Cancelled)
    }
}

fn wait_flag(flag: &Arc<(Mutex<bool>, Condvar)>) {
    let (lock, condition) = &**flag;
    let mut value = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    while !*value {
        value = condition
            .wait(value)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
}

fn set_flag(flag: &Arc<(Mutex<bool>, Condvar)>) {
    let (lock, condition) = &**flag;
    let mut value = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *value = true;
    condition.notify_all();
}

#[cfg(feature = "test-support")]
fn report_test_error(error: &io::Error) {
    let Some(directory) = std::env::var_os("OPP_TEST_RUNTIME_DIR") else {
        return;
    };
    let path = std::path::PathBuf::from(directory).join("broker-errors.log");
    if let Ok(mut log) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(log, "{error}");
    }
}
