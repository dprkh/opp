use crate::account::Account;
use crate::environment::ClientContext;
use nix::fcntl::{FcntlArg, FdFlag, fcntl};
use nix::sys::socket::{ControlMessage, ControlMessageOwned, MsgFlags, recvmsg, sendmsg};
use std::io::{self, IoSlice, IoSliceMut, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

pub(crate) const PROTOCOL_VERSION: u32 = 1;
pub(crate) const MAX_PAYLOAD: usize = 2 * 1024 * 1024;
pub(crate) const STOP_REQUEST: &[u8; 8] = b"OPPSTOP\0";
pub(crate) const STOP_RESPONSE: &[u8; 8] = b"OPPSTOP\x01";
const NORMAL_MAGIC: &[u8; 4] = b"OPP\0";

const REQUEST_START: u8 = 1;
const REQUEST_STATUS: u8 = 2;
const REQUEST_EXEC: u8 = 3;
const EVENT_STREAMS_CLOSED: u8 = 4;
const EVENT_CANCEL: u8 = 5;

const RESPONSE_OK: u8 = 0x80;
const RESPONSE_NEED_WARNING: u8 = 0x81;
const RESPONSE_STATUS: u8 = 0x82;
const RESPONSE_EXEC: u8 = 0x83;
const RESPONSE_ERROR: u8 = 0x84;

#[derive(Debug)]
pub(crate) enum Request {
    Start {
        warning_shown: bool,
        account: Account,
        op_path: String,
        context: ClientContext,
    },
    Status {
        account: Account,
    },
    Exec {
        account: Account,
        timeout_nanos: u64,
        arguments: Vec<Vec<u8>>,
        context: ClientContext,
        descriptors: [OwnedFd; 3],
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum ErrorCode {
    Operational = 1,
    AuthorizationRequired = 2,
    AuthorizationFailed = 3,
    OpPathMismatch = 4,
    ProcessStart = 5,
    Protocol = 6,
    QueueFull = 7,
    Cancelled = 8,
}

impl TryFrom<u8> for ErrorCode {
    type Error = io::Error;

    fn try_from(value: u8) -> io::Result<Self> {
        match value {
            1 => Ok(Self::Operational),
            2 => Ok(Self::AuthorizationRequired),
            3 => Ok(Self::AuthorizationFailed),
            4 => Ok(Self::OpPathMismatch),
            5 => Ok(Self::ProcessStart),
            6 => Ok(Self::Protocol),
            7 => Ok(Self::QueueFull),
            8 => Ok(Self::Cancelled),
            _ => Err(invalid("unknown protocol error code")),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum Diagnostic {
    None = 0,
    Timeout = 1,
    Internal = 2,
}

impl TryFrom<u8> for Diagnostic {
    type Error = io::Error;

    fn try_from(value: u8) -> io::Result<Self> {
        match value {
            0 => Ok(Self::None),
            1 => Ok(Self::Timeout),
            2 => Ok(Self::Internal),
            _ => Err(invalid("unknown completion diagnostic")),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Response {
    Ok,
    NeedWarning,
    Status(Vec<u8>),
    Exec {
        exit_code: i32,
        diagnostic: Diagnostic,
    },
    Error(ErrorCode),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ClientEvent {
    StreamsClosed,
    Cancel(i32),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ServerPreamble {
    Stop,
    Normal,
    Incompatible,
}

pub(crate) fn client_negotiate(stream: &mut UnixStream) -> io::Result<()> {
    stream.write_all(&normal_preamble(PROTOCOL_VERSION))?;
    let mut response = [0_u8; 8];
    stream.read_exact(&mut response)?;
    if response[..4] != NORMAL_MAGIC[..]
        || u32::from_be_bytes(response[4..].try_into().expect("four bytes")) != PROTOCOL_VERSION
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "incompatible broker protocol",
        ));
    }
    Ok(())
}

pub(crate) fn server_preamble(stream: &mut UnixStream) -> io::Result<ServerPreamble> {
    let mut request = [0_u8; 8];
    stream.read_exact(&mut request)?;
    if &request == STOP_REQUEST {
        return Ok(ServerPreamble::Stop);
    }
    if request[..4] != NORMAL_MAGIC[..] {
        return Err(invalid("invalid protocol preamble"));
    }
    let version = u32::from_be_bytes(request[4..].try_into().expect("four bytes"));
    stream.write_all(&normal_preamble(PROTOCOL_VERSION))?;
    if version == PROTOCOL_VERSION {
        Ok(ServerPreamble::Normal)
    } else {
        Ok(ServerPreamble::Incompatible)
    }
}

pub(crate) fn send_request(stream: &mut UnixStream, request: &Request) -> io::Result<()> {
    let (kind, payload, descriptors): (u8, Vec<u8>, Vec<RawFd>) = match request {
        Request::Start {
            warning_shown,
            account,
            op_path,
            context,
        } => {
            let mut encoder = Encoder::new();
            encoder.u8(u8::from(*warning_shown));
            encoder.account(account)?;
            encoder.string(op_path)?;
            encoder.context(context)?;
            (REQUEST_START, encoder.finish()?, Vec::new())
        }
        Request::Status { account } => {
            let mut encoder = Encoder::new();
            encoder.account(account)?;
            (REQUEST_STATUS, encoder.finish()?, Vec::new())
        }
        Request::Exec {
            account,
            timeout_nanos,
            arguments,
            context,
            descriptors,
        } => {
            let mut encoder = Encoder::new();
            encoder.account(account)?;
            encoder.u64(*timeout_nanos);
            encoder.u16(u16::try_from(arguments.len()).map_err(|_| invalid("too many arguments"))?);
            for argument in arguments {
                encoder.bytes(argument)?;
            }
            encoder.context(context)?;
            (
                REQUEST_EXEC,
                encoder.finish()?,
                descriptors.iter().map(AsRawFd::as_raw_fd).collect(),
            )
        }
    };
    send_frame(stream, kind, &payload, &descriptors)
}

pub(crate) fn receive_request(stream: &mut UnixStream) -> io::Result<Request> {
    let (kind, payload, descriptors) = receive_frame(stream)?;
    let mut decoder = Decoder::new(&payload);
    let request = match kind {
        REQUEST_START => {
            require_no_descriptors(descriptors)?;
            let warning_shown = decoder.boolean()?;
            let account = decoder.account()?;
            let op_path = decoder.string()?;
            let context = decoder.context()?;
            Request::Start {
                warning_shown,
                account,
                op_path,
                context,
            }
        }
        REQUEST_STATUS => {
            require_no_descriptors(descriptors)?;
            Request::Status {
                account: decoder.account()?,
            }
        }
        REQUEST_EXEC => {
            let descriptors: [OwnedFd; 3] =
                descriptors
                    .try_into()
                    .map_err(|descriptors: Vec<OwnedFd>| {
                        drop(descriptors);
                        invalid("exec requires exactly three descriptors")
                    })?;
            let account = decoder.account()?;
            let timeout_nanos = decoder.u64()?;
            if !(1_000_000_000..=600_000_000_000).contains(&timeout_nanos) {
                return Err(invalid("exec timeout is out of range"));
            }
            let count = usize::from(decoder.u16()?);
            if count > 256 {
                return Err(invalid("too many arguments"));
            }
            let mut arguments = Vec::with_capacity(count);
            let mut total = 0_usize;
            for _ in 0..count {
                let argument = decoder.bytes()?.to_vec();
                if argument.contains(&0) {
                    return Err(invalid("argument contains NUL"));
                }
                total = total.saturating_add(argument.len());
                arguments.push(argument);
            }
            if total > 65_536 {
                return Err(invalid("argument payload is too large"));
            }
            let context = decoder.context()?;
            Request::Exec {
                account,
                timeout_nanos,
                arguments,
                context,
                descriptors,
            }
        }
        _ => return Err(invalid("unknown request kind")),
    };
    decoder.end()?;
    Ok(request)
}

pub(crate) fn send_response(stream: &mut UnixStream, response: &Response) -> io::Result<()> {
    let (kind, payload) = match response {
        Response::Ok => (RESPONSE_OK, Vec::new()),
        Response::NeedWarning => (RESPONSE_NEED_WARNING, Vec::new()),
        Response::Status(json) => (RESPONSE_STATUS, json.clone()),
        Response::Exec {
            exit_code,
            diagnostic,
        } => {
            let mut payload = exit_code.to_be_bytes().to_vec();
            payload.push(*diagnostic as u8);
            (RESPONSE_EXEC, payload)
        }
        Response::Error(code) => (RESPONSE_ERROR, vec![*code as u8]),
    };
    send_frame(stream, kind, &payload, &[])
}

pub(crate) fn receive_response(stream: &mut UnixStream) -> io::Result<Response> {
    let (kind, payload, descriptors) = receive_frame(stream)?;
    require_no_descriptors(descriptors)?;
    match kind {
        RESPONSE_OK if payload.is_empty() => Ok(Response::Ok),
        RESPONSE_NEED_WARNING if payload.is_empty() => Ok(Response::NeedWarning),
        RESPONSE_STATUS => Ok(Response::Status(payload)),
        RESPONSE_EXEC if payload.len() == 5 => {
            let exit_code = i32::from_be_bytes(payload[..4].try_into().expect("four bytes"));
            if !(0..=255).contains(&exit_code) {
                return Err(invalid("invalid command exit code"));
            }
            Ok(Response::Exec {
                exit_code,
                diagnostic: Diagnostic::try_from(payload[4])?,
            })
        }
        RESPONSE_ERROR if payload.len() == 1 => {
            Ok(Response::Error(ErrorCode::try_from(payload[0])?))
        }
        _ => Err(invalid("invalid response frame")),
    }
}

pub(crate) fn send_client_event(stream: &mut UnixStream, event: ClientEvent) -> io::Result<()> {
    match event {
        ClientEvent::StreamsClosed => send_frame(stream, EVENT_STREAMS_CLOSED, &[], &[]),
        ClientEvent::Cancel(signal) if is_caller_signal(signal) => {
            send_frame(stream, EVENT_CANCEL, &signal.to_be_bytes(), &[])
        }
        ClientEvent::Cancel(_) => Err(invalid("invalid cancellation signal")),
    }
}

pub(crate) fn receive_client_event(stream: &mut UnixStream) -> io::Result<ClientEvent> {
    let (kind, payload, descriptors) = receive_frame(stream)?;
    require_no_descriptors(descriptors)?;
    match kind {
        EVENT_STREAMS_CLOSED if payload.is_empty() => Ok(ClientEvent::StreamsClosed),
        EVENT_CANCEL if payload.len() == 4 => {
            let signal = i32::from_be_bytes(payload.try_into().expect("four bytes"));
            if !is_caller_signal(signal) {
                return Err(invalid("invalid cancellation signal"));
            }
            Ok(ClientEvent::Cancel(signal))
        }
        _ => Err(invalid("invalid client event")),
    }
}

fn normal_preamble(version: u32) -> [u8; 8] {
    let mut preamble = [0_u8; 8];
    preamble[..4].copy_from_slice(NORMAL_MAGIC);
    preamble[4..].copy_from_slice(&version.to_be_bytes());
    preamble
}

fn is_caller_signal(signal: i32) -> bool {
    matches!(
        signal,
        libc::SIGHUP | libc::SIGINT | libc::SIGQUIT | libc::SIGTERM
    )
}

fn send_frame(
    stream: &mut UnixStream,
    kind: u8,
    payload: &[u8],
    descriptors: &[RawFd],
) -> io::Result<()> {
    if payload.len() > MAX_PAYLOAD {
        return Err(invalid("protocol payload is too large"));
    }
    let mut header = [0_u8; 8];
    header[0] = kind;
    header[4..].copy_from_slice(
        &u32::try_from(payload.len())
            .map_err(|_| invalid("protocol payload is too large"))?
            .to_be_bytes(),
    );
    let iov = [IoSlice::new(&header)];
    let sent = if descriptors.is_empty() {
        sendmsg::<()>(stream.as_raw_fd(), &iov, &[], MsgFlags::empty(), None)
    } else {
        let rights = [ControlMessage::ScmRights(descriptors)];
        sendmsg::<()>(stream.as_raw_fd(), &iov, &rights, MsgFlags::empty(), None)
    }
    .map_err(nix_error)?;
    if sent < header.len() {
        stream.write_all(&header[sent..])?;
    }
    stream.write_all(payload)
}

fn receive_frame(stream: &mut UnixStream) -> io::Result<(u8, Vec<u8>, Vec<OwnedFd>)> {
    let mut header = [0_u8; 8];
    let (received, descriptors) = {
        let mut iov = [IoSliceMut::new(&mut header)];
        let mut control = nix::cmsg_space!([RawFd; 8]);
        let message = recvmsg::<()>(
            stream.as_raw_fd(),
            &mut iov,
            Some(&mut control),
            MsgFlags::empty(),
        )
        .map_err(nix_error)?;
        if message.bytes == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "broker disconnected",
            ));
        }
        if message.flags.contains(MsgFlags::MSG_CTRUNC) {
            return Err(invalid("truncated descriptor message"));
        }
        let received = message.bytes;
        let mut descriptors = Vec::new();
        for control_message in message.cmsgs().map_err(nix_error)? {
            match control_message {
                ControlMessageOwned::ScmRights(rights) => {
                    for descriptor in rights {
                        // SAFETY: Each descriptor in an SCM_RIGHTS message is newly owned by this process.
                        let owned = unsafe { OwnedFd::from_raw_fd(descriptor) };
                        fcntl(&owned, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).map_err(nix_error)?;
                        descriptors.push(owned);
                    }
                }
                _ => return Err(invalid("unexpected control message")),
            }
        }
        (received, descriptors)
    };
    if received < header.len() {
        stream.read_exact(&mut header[received..])?;
    }
    if header[1..4] != [0, 0, 0] {
        return Err(invalid("nonzero protocol header flags"));
    }
    let length = usize::try_from(u32::from_be_bytes(
        header[4..].try_into().expect("four bytes"),
    ))
    .expect("u32 fits usize");
    if length > MAX_PAYLOAD {
        return Err(invalid("protocol payload is too large"));
    }
    let mut payload = vec![0_u8; length];
    stream.read_exact(&mut payload)?;
    Ok((header[0], payload, descriptors))
}

fn require_no_descriptors(descriptors: Vec<OwnedFd>) -> io::Result<()> {
    if descriptors.is_empty() {
        Ok(())
    } else {
        drop(descriptors);
        Err(invalid("unexpected descriptors"))
    }
}

struct Encoder {
    bytes: Vec<u8>,
}

impl Encoder {
    fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn u16(&mut self, value: u16) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn bytes(&mut self, value: &[u8]) -> io::Result<()> {
        self.u32(u32::try_from(value.len()).map_err(|_| invalid("field is too large"))?);
        self.bytes.extend_from_slice(value);
        Ok(())
    }

    fn string(&mut self, value: &str) -> io::Result<()> {
        self.bytes(value.as_bytes())
    }

    fn account(&mut self, account: &Account) -> io::Result<()> {
        match account.explicit() {
            Some(selector) => {
                self.u8(1);
                self.string(selector)
            }
            None => {
                self.u8(0);
                Ok(())
            }
        }
    }

    fn context(&mut self, context: &ClientContext) -> io::Result<()> {
        self.bytes(&context.cwd)?;
        self.u32(
            u32::try_from(context.environment.len())
                .map_err(|_| invalid("too many environment entries"))?,
        );
        for (key, value) in &context.environment {
            self.bytes(key)?;
            self.bytes(value)?;
        }
        Ok(())
    }

    fn finish(self) -> io::Result<Vec<u8>> {
        if self.bytes.len() > MAX_PAYLOAD {
            Err(invalid("protocol payload is too large"))
        } else {
            Ok(self.bytes)
        }
    }
}

struct Decoder<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, length: usize) -> io::Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or_else(|| invalid("protocol field overflow"))?;
        if end > self.bytes.len() {
            return Err(invalid("truncated protocol payload"));
        }
        let result = &self.bytes[self.offset..end];
        self.offset = end;
        Ok(result)
    }

    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn boolean(&mut self) -> io::Result<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(invalid("invalid protocol boolean")),
        }
    }

    fn u16(&mut self) -> io::Result<u16> {
        Ok(u16::from_be_bytes(
            self.take(2)?.try_into().expect("two bytes"),
        ))
    }

    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_be_bytes(
            self.take(4)?.try_into().expect("four bytes"),
        ))
    }

    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_be_bytes(
            self.take(8)?.try_into().expect("eight bytes"),
        ))
    }

    fn bytes(&mut self) -> io::Result<&'a [u8]> {
        let length = usize::try_from(self.u32()?).expect("u32 fits usize");
        self.take(length)
    }

    fn string(&mut self) -> io::Result<String> {
        String::from_utf8(self.bytes()?.to_vec()).map_err(|_| invalid("invalid protocol UTF-8"))
    }

    fn account(&mut self) -> io::Result<Account> {
        match self.u8()? {
            0 => Ok(Account(None)),
            1 => Ok(Account(Some(self.string()?))),
            _ => Err(invalid("invalid account presence flag")),
        }
    }

    fn context(&mut self) -> io::Result<ClientContext> {
        let cwd = self.bytes()?.to_vec();
        if cwd.is_empty() || cwd.contains(&0) || cwd.first() != Some(&b'/') {
            return Err(invalid("invalid working directory"));
        }
        let count = usize::try_from(self.u32()?).expect("u32 fits usize");
        if count > self.bytes.len().saturating_sub(self.offset) / 8 {
            return Err(invalid("invalid environment count"));
        }
        let mut environment = Vec::with_capacity(count);
        for _ in 0..count {
            let key = self.bytes()?.to_vec();
            let value = self.bytes()?.to_vec();
            if key.is_empty() || key.contains(&0) || key.contains(&b'=') || value.contains(&0) {
                return Err(invalid("invalid environment entry"));
            }
            environment.push((key, value));
        }
        Ok(ClientContext { cwd, environment })
    }

    fn end(&self) -> io::Result<()> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(invalid("trailing protocol data"))
        }
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn nix_error(error: nix::errno::Errno) -> io::Error {
    io::Error::from_raw_os_error(error as i32)
}

#[cfg(test)]
mod tests {
    use super::{
        ClientEvent, Diagnostic, ErrorCode, Response, receive_response, send_client_event,
        send_response,
    };
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    #[test]
    fn response_round_trip() {
        let (mut left, mut right) = UnixStream::pair().unwrap();
        let expected = Response::Exec {
            exit_code: 124,
            diagnostic: Diagnostic::Timeout,
        };
        send_response(&mut left, &expected).unwrap();
        assert_eq!(receive_response(&mut right).unwrap(), expected);
    }

    #[test]
    fn error_round_trip() {
        let (mut left, mut right) = UnixStream::pair().unwrap();
        send_response(
            &mut left,
            &Response::Error(ErrorCode::AuthorizationRequired),
        )
        .unwrap();
        assert_eq!(
            receive_response(&mut right).unwrap(),
            Response::Error(ErrorCode::AuthorizationRequired)
        );
    }

    #[test]
    fn event_encoding_is_bounded() {
        let (mut left, mut right) = UnixStream::pair().unwrap();
        send_client_event(&mut left, ClientEvent::Cancel(libc::SIGINT)).unwrap();
        assert_eq!(
            super::receive_client_event(&mut right).unwrap(),
            ClientEvent::Cancel(libc::SIGINT)
        );
    }

    #[test]
    fn rejects_invalid_cancellation_signals() {
        let (mut left, _right) = UnixStream::pair().unwrap();
        assert!(send_client_event(&mut left, ClientEvent::Cancel(i32::MAX)).is_err());
    }

    #[test]
    fn incompatible_versions_are_rejected_before_a_request() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || super::server_preamble(&mut server).unwrap());
        client
            .write_all(&super::normal_preamble(super::PROTOCOL_VERSION + 1))
            .unwrap();
        let mut response = [0_u8; 8];
        client.read_exact(&mut response).unwrap();
        assert_eq!(response, super::normal_preamble(super::PROTOCOL_VERSION));
        assert_eq!(server.join().unwrap(), super::ServerPreamble::Incompatible);
    }
}
