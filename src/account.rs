use std::ffi::{OsStr, OsString};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct Account(pub(crate) Option<String>);

impl Account {
    pub(crate) fn select(explicit: Option<OsString>) -> Result<Self, &'static str> {
        if let Some(value) = explicit.filter(|value| !value.is_empty()) {
            return utf8(value.as_os_str()).map(|value| Self(Some(value.to_owned())));
        }

        match std::env::var_os("OP_ACCOUNT").filter(|value| !value.is_empty()) {
            Some(value) => utf8(value.as_os_str()).map(|value| Self(Some(value.to_owned()))),
            None => Ok(Self(None)),
        }
    }

    pub(crate) fn explicit(&self) -> Option<&str> {
        self.0.as_deref()
    }
}

fn utf8(value: &OsStr) -> Result<&str, &'static str> {
    value
        .to_str()
        .ok_or("account selectors must be valid UTF-8")
}

#[cfg(test)]
mod tests {
    use super::Account;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn explicit_selector_is_exact() {
        assert_eq!(
            Account::select(Some(OsString::from("Work.Example")))
                .unwrap()
                .0,
            Some(String::from("Work.Example"))
        );
    }

    #[test]
    fn invalid_utf8_is_rejected() {
        let value = OsString::from_vec(vec![0xff]);
        assert!(Account::select(Some(value)).is_err());
    }
}
