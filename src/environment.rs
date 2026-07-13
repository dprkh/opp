use crate::account::Account;
use crate::runtime::EffectiveUser;
use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;

#[derive(Clone, Debug)]
pub(crate) struct ClientContext {
    pub(crate) cwd: Vec<u8>,
    pub(crate) environment: Vec<(Vec<u8>, Vec<u8>)>,
}

impl ClientContext {
    pub(crate) fn capture() -> io::Result<Self> {
        let cwd = std::env::current_dir()?;
        if !cwd.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "current directory is not absolute",
            ));
        }
        Ok(Self {
            cwd: cwd.as_os_str().as_bytes().to_vec(),
            environment: std::env::vars_os()
                .map(|(key, value)| {
                    (
                        key.as_os_str().as_bytes().to_vec(),
                        value.as_os_str().as_bytes().to_vec(),
                    )
                })
                .collect(),
        })
    }

    pub(crate) fn sanitized(&self, account: &Account) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut environment: Vec<_> = self
            .environment
            .iter()
            .filter(|(key, _)| !is_removed(key))
            .cloned()
            .collect();
        set(&mut environment, b"OP_BIOMETRIC_UNLOCK_ENABLED", b"true");
        match account.explicit() {
            Some(selector) => set(&mut environment, b"OP_ACCOUNT", selector.as_bytes()),
            None => remove(&mut environment, b"OP_ACCOUNT"),
        }
        environment
    }

    pub(crate) fn maintenance(user: &EffectiveUser, account: &Account) -> Self {
        let mut environment = vec![
            (b"HOME".to_vec(), user.home.as_os_str().as_bytes().to_vec()),
            (b"USER".to_vec(), user.name.as_bytes().to_vec()),
            (b"LOGNAME".to_vec(), user.name.as_bytes().to_vec()),
            (b"PATH".to_vec(), system_path()),
            (b"OP_BIOMETRIC_UNLOCK_ENABLED".to_vec(), b"true".to_vec()),
        ];
        if let Some(tmpdir) = darwin_user_temp_dir() {
            environment.push((b"TMPDIR".to_vec(), tmpdir));
        }
        if let Some(selector) = account.explicit() {
            environment.push((b"OP_ACCOUNT".to_vec(), selector.as_bytes().to_vec()));
        }
        Self {
            cwd: user.home.as_os_str().as_bytes().to_vec(),
            environment,
        }
    }
}

fn is_removed(key: &[u8]) -> bool {
    key == b"OP_SESSION"
        || key.starts_with(b"OP_SESSION_")
        || key == b"OP_SERVICE_ACCOUNT_TOKEN"
        || key == b"OP_CONNECT_HOST"
        || key == b"OP_CONNECT_TOKEN"
        || key.starts_with(b"OPP_")
}

fn set(environment: &mut Vec<(Vec<u8>, Vec<u8>)>, key: &[u8], value: &[u8]) {
    remove(environment, key);
    environment.push((key.to_vec(), value.to_vec()));
}

fn remove(environment: &mut Vec<(Vec<u8>, Vec<u8>)>, key: &[u8]) {
    environment.retain(|(candidate, _)| candidate != key);
}

fn system_path() -> Vec<u8> {
    confstr(libc::_CS_PATH).unwrap_or_else(|| b"/usr/bin:/bin:/usr/sbin:/sbin".to_vec())
}

fn darwin_user_temp_dir() -> Option<Vec<u8>> {
    confstr(libc::_CS_DARWIN_USER_TEMP_DIR)
}

fn confstr(name: libc::c_int) -> Option<Vec<u8>> {
    // SAFETY: A null buffer with length zero asks `confstr` for the required size.
    let length = unsafe { libc::confstr(name, std::ptr::null_mut(), 0) };
    if length <= 1 {
        return None;
    }
    let mut buffer = vec![0_u8; length];
    // SAFETY: `buffer` has the size returned by the preceding `confstr` call.
    let written = unsafe { libc::confstr(name, buffer.as_mut_ptr().cast(), buffer.len()) };
    if written == 0 || written > buffer.len() {
        return None;
    }
    buffer.truncate(written.saturating_sub(1));
    Some(buffer)
}

pub(crate) fn os_bytes(value: &OsStr) -> &[u8] {
    value.as_bytes()
}

#[cfg(test)]
mod tests {
    use super::ClientContext;
    use crate::account::Account;

    #[test]
    fn sanitizes_sensitive_variables() {
        let context = ClientContext {
            cwd: b"/tmp".to_vec(),
            environment: vec![
                (b"KEEP".to_vec(), b"yes".to_vec()),
                (b"OP_SESSION_work".to_vec(), b"secret".to_vec()),
                (b"OPP_PRIVATE".to_vec(), b"secret".to_vec()),
                (b"OP_ACCOUNT".to_vec(), b"old".to_vec()),
            ],
        };
        let result = context.sanitized(&Account(Some(String::from("new"))));
        assert!(
            result
                .iter()
                .any(|entry| entry == &(b"KEEP".to_vec(), b"yes".to_vec()))
        );
        assert!(
            result
                .iter()
                .any(|entry| entry == &(b"OP_ACCOUNT".to_vec(), b"new".to_vec()))
        );
        assert!(!result.iter().any(|(key, _)| key.starts_with(b"OP_SESSION")));
        assert!(!result.iter().any(|(key, _)| key.starts_with(b"OPP_")));
    }
}
