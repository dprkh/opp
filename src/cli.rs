use crate::VERSION;
use crate::account::Account;
use crate::broker;
use crate::client;
use crate::environment::os_bytes;
use std::ffi::OsString;
use std::io::{self, Write};
use std::time::Duration;

const ROOT_HELP: &str = "Usage:\n  opp --help\n  opp --version\n  opp start [--account ACCOUNT] [--op ABSOLUTE_PATH]\n  opp status [--account ACCOUNT]\n  opp stop\n  opp exec [--account ACCOUNT] [--timeout DURATION] -- [OP_ARGUMENT...]\n";
const START_HELP: &str = "Usage: opp start [--account ACCOUNT] [--op ABSOLUTE_PATH]\n";
const STATUS_HELP: &str = "Usage: opp status [--account ACCOUNT]\n";
const STOP_HELP: &str = "Usage: opp stop\n";
const EXEC_HELP: &str =
    "Usage: opp exec [--account ACCOUNT] [--timeout DURATION] -- [OP_ARGUMENT...]\n";

pub(crate) fn run<I>(arguments: I) -> i32
where
    I: IntoIterator<Item = OsString>,
{
    let mut arguments: Vec<OsString> = arguments.into_iter().collect();
    if !arguments.is_empty() {
        arguments.remove(0);
    }
    if arguments
        .first()
        .is_some_and(|value| value == broker::INTERNAL_BROKER_ARGUMENT)
    {
        return if arguments.len() == 1 {
            broker::run_internal()
        } else {
            2
        };
    }
    match parse(arguments) {
        Ok(Action::Help(text)) => write_stdout(text.as_bytes(), 0),
        Ok(Action::Version) => write_stdout(format!("opp {VERSION}\n").as_bytes(), 0),
        Ok(Action::Start { account, op }) => match Account::select(account) {
            Ok(account) => client::start(account, op),
            Err(message) => usage_error(message),
        },
        Ok(Action::Status { account }) => match Account::select(account) {
            Ok(account) => client::status(account),
            Err(message) => usage_error(message),
        },
        Ok(Action::Stop) => client::stop(),
        Ok(Action::Exec {
            account,
            timeout,
            arguments,
        }) => match Account::select(account) {
            Ok(account) => client::execute(account, timeout, arguments),
            Err(message) => usage_error(message),
        },
        Err(message) => usage_error(message),
    }
}

enum Action {
    Help(&'static str),
    Version,
    Start {
        account: Option<OsString>,
        op: Option<OsString>,
    },
    Status {
        account: Option<OsString>,
    },
    Stop,
    Exec {
        account: Option<OsString>,
        timeout: Duration,
        arguments: Vec<Vec<u8>>,
    },
}

fn parse(arguments: Vec<OsString>) -> Result<Action, &'static str> {
    let Some(command) = arguments.first() else {
        return Err("a command is required");
    };
    if command == "--help" && arguments.len() == 1 {
        return Ok(Action::Help(ROOT_HELP));
    }
    if command == "--version" && arguments.len() == 1 {
        return Ok(Action::Version);
    }
    let rest = &arguments[1..];
    match command.to_str() {
        Some("start") => parse_start(rest),
        Some("status") => parse_status(rest),
        Some("stop") => parse_stop(rest),
        Some("exec") => parse_exec(rest),
        _ => Err("unknown command or option"),
    }
}

fn parse_start(arguments: &[OsString]) -> Result<Action, &'static str> {
    if has_help(arguments) {
        return Ok(Action::Help(START_HELP));
    }
    let mut account = None;
    let mut op = None;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].to_str() {
            Some("--account") if account.is_none() => {
                index += 1;
                account = Some(
                    arguments
                        .get(index)
                        .ok_or("--account requires a value")?
                        .clone(),
                );
            }
            Some("--op") if op.is_none() => {
                index += 1;
                op = Some(arguments.get(index).ok_or("--op requires a value")?.clone());
            }
            _ => return Err("invalid start option"),
        }
        index += 1;
    }
    Ok(Action::Start { account, op })
}

fn parse_status(arguments: &[OsString]) -> Result<Action, &'static str> {
    if has_help(arguments) {
        return Ok(Action::Help(STATUS_HELP));
    }
    match arguments {
        [] => Ok(Action::Status { account: None }),
        [option, value] if option == "--account" => Ok(Action::Status {
            account: Some(value.clone()),
        }),
        _ => Err("invalid status option"),
    }
}

fn parse_stop(arguments: &[OsString]) -> Result<Action, &'static str> {
    if has_help(arguments) {
        return Ok(Action::Help(STOP_HELP));
    }
    if arguments.is_empty() {
        Ok(Action::Stop)
    } else {
        Err("stop accepts no options")
    }
}

fn parse_exec(arguments: &[OsString]) -> Result<Action, &'static str> {
    let Some(separator) = arguments.iter().position(|argument| argument == "--") else {
        if has_help(arguments) {
            return Ok(Action::Help(EXEC_HELP));
        }
        return Err("exec requires the -- separator");
    };
    if has_help(&arguments[..separator]) {
        return Ok(Action::Help(EXEC_HELP));
    }
    let mut account = None;
    let mut timeout = Duration::from_secs(120);
    let mut timeout_set = false;
    let mut index = 0;
    while index < separator {
        match arguments[index].to_str() {
            Some("--account") if account.is_none() => {
                index += 1;
                if index >= separator {
                    return Err("--account requires a value");
                }
                account = Some(arguments[index].clone());
            }
            Some("--timeout") if !timeout_set => {
                index += 1;
                if index >= separator {
                    return Err("--timeout requires a value");
                }
                let value = arguments[index]
                    .to_str()
                    .ok_or("timeout must be valid UTF-8")?;
                timeout = parse_duration(value).ok_or("invalid timeout duration")?;
                timeout_set = true;
            }
            _ => return Err("invalid exec option"),
        }
        index += 1;
    }

    let raw = &arguments[separator + 1..];
    if raw.len() > 256 {
        return Err("exec accepts at most 256 op arguments");
    }
    let mut total = 0_usize;
    let mut proxied = Vec::with_capacity(raw.len());
    for argument in raw {
        let bytes = os_bytes(argument.as_os_str()).to_vec();
        if bytes.contains(&0) {
            return Err("op arguments must not contain NUL");
        }
        total = total.saturating_add(bytes.len());
        proxied.push(bytes);
    }
    if total > 65_536 {
        return Err("op arguments exceed 65,536 bytes");
    }
    reject_account_override(&proxied)?;
    Ok(Action::Exec {
        account,
        timeout,
        arguments: proxied,
    })
}

fn reject_account_override(arguments: &[Vec<u8>]) -> Result<(), &'static str> {
    for argument in arguments {
        if argument == b"--" {
            break;
        }
        if argument == b"--account" || argument.starts_with(b"--account=") {
            return Err("proxied op arguments may not override the selected account");
        }
    }
    Ok(())
}

fn has_help(arguments: &[OsString]) -> bool {
    arguments.iter().any(|argument| argument == "--help")
}

fn parse_duration(value: &str) -> Option<Duration> {
    let bytes = value.as_bytes();
    let mut index = 0_usize;
    let negative = match bytes.first() {
        Some(b'-') => {
            index = 1;
            true
        }
        Some(b'+') => {
            index = 1;
            false
        }
        _ => false,
    };
    if negative || index == bytes.len() {
        return None;
    }
    let mut total = 0_u128;
    let mut segments = 0_usize;
    while index < bytes.len() {
        let integer_start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        let integer_digits = &bytes[integer_start..index];
        let mut fraction_digits = &[][..];
        if bytes.get(index) == Some(&b'.') {
            index += 1;
            let fraction_start = index;
            while bytes.get(index).is_some_and(u8::is_ascii_digit) {
                index += 1;
            }
            fraction_digits = &bytes[fraction_start..index];
        }
        if integer_digits.is_empty() && fraction_digits.is_empty() {
            return None;
        }

        let units = &value[index..];
        let (unit_text, unit_nanos) = [
            ("ns", 1_u128),
            ("us", 1_000),
            ("µs", 1_000),
            ("μs", 1_000),
            ("ms", 1_000_000),
            ("s", 1_000_000_000),
            ("m", 60_000_000_000),
            ("h", 3_600_000_000_000),
        ]
        .into_iter()
        .find(|(unit, _)| units.starts_with(unit))?;
        index += unit_text.len();

        let integer = parse_ascii_integer(integer_digits)?;
        total = total.checked_add(integer.checked_mul(unit_nanos)?)?;
        if !fraction_digits.is_empty() {
            let (fraction, scale) = parse_ascii_fraction(fraction_digits);
            let fractional_nanos = (fraction as f64 * (unit_nanos as f64 / scale)).trunc() as u128;
            total = total.checked_add(fractional_nanos)?;
        }
        segments += 1;
    }
    if segments == 0 || !(1_000_000_000..=600_000_000_000).contains(&total) {
        return None;
    }
    Some(Duration::from_nanos(u64::try_from(total).ok()?))
}

fn parse_ascii_integer(digits: &[u8]) -> Option<u128> {
    let mut value = 0_u128;
    for digit in digits {
        value = value
            .checked_mul(10)?
            .checked_add(u128::from(*digit - b'0'))?;
    }
    Some(value)
}

fn parse_ascii_fraction(digits: &[u8]) -> (u64, f64) {
    let mut value = 0_u64;
    let mut scale = 1_f64;
    let mut overflow = false;
    for digit in digits {
        if overflow {
            continue;
        }
        if value > (i64::MAX as u64) / 10 {
            overflow = true;
            continue;
        }
        let next = value * 10 + u64::from(*digit - b'0');
        if next > (1_u64 << 63) {
            overflow = true;
            continue;
        }
        value = next;
        scale *= 10_f64;
    }
    (value, scale)
}

fn usage_error(message: &str) -> i32 {
    let _ = writeln!(io::stderr(), "opp: {message}");
    let _ = writeln!(io::stderr(), "Try 'opp --help'.");
    2
}

fn write_stdout(bytes: &[u8], success: i32) -> i32 {
    if io::stdout().write_all(bytes).is_ok() {
        success
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_duration, parse_exec, reject_account_override};
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::time::Duration;

    #[test]
    fn parses_go_style_durations() {
        assert_eq!(parse_duration("1.5s"), Some(Duration::from_millis(1500)));
        assert_eq!(parse_duration("1m30s"), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration("600s"), Some(Duration::from_secs(600)));
        assert_eq!(parse_duration("999ms"), None);
        assert_eq!(parse_duration("10m1ns"), None);
        assert_eq!(parse_duration("-2s"), None);
        assert_eq!(parse_duration("+1s"), Some(Duration::from_secs(1)));
        assert_eq!(parse_duration("1.s"), Some(Duration::from_secs(1)));
        assert_eq!(parse_duration("1000000us"), Some(Duration::from_secs(1)));
        assert_eq!(parse_duration("1000000µs"), Some(Duration::from_secs(1)));
        assert_eq!(parse_duration("1000000μs"), Some(Duration::from_secs(1)));
        assert_eq!(
            parse_duration("1.0000000000000000000000000000000000000000000000001s"),
            Some(Duration::from_secs(1))
        );
        for invalid in ["", "0", "+", ".s", "1", "1e3s", "1ss", "--1s"] {
            assert_eq!(parse_duration(invalid), None, "accepted {invalid}");
        }
    }

    #[test]
    fn rejects_only_account_options_before_inner_separator() {
        assert!(reject_account_override(&[b"item".to_vec(), b"--account=x".to_vec()]).is_err());
        assert!(
            reject_account_override(&[b"run".to_vec(), b"--".to_vec(), b"--account=x".to_vec(),])
                .is_ok()
        );
    }

    #[test]
    fn enforces_raw_argument_limits_before_connecting() {
        let mut maximum_count = vec![OsString::from("--")];
        maximum_count.extend((0..256).map(|_| OsString::new()));
        assert!(parse_exec(&maximum_count).is_ok());
        maximum_count.push(OsString::new());
        assert!(parse_exec(&maximum_count).is_err());

        let maximum_bytes = [OsString::from("--"), OsString::from_vec(vec![b'x'; 65_536])];
        assert!(parse_exec(&maximum_bytes).is_ok());
        let too_many_bytes = [OsString::from("--"), OsString::from_vec(vec![b'x'; 65_537])];
        assert!(parse_exec(&too_many_bytes).is_err());
        let nul = [OsString::from("--"), OsString::from_vec(vec![b'x', 0])];
        assert!(parse_exec(&nul).is_err());
    }
}
