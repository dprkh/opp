use nix::unistd::{Uid, User};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub(crate) struct EffectiveUser {
    pub(crate) uid: Uid,
    pub(crate) name: String,
    pub(crate) home: PathBuf,
}

impl EffectiveUser {
    pub(crate) fn load() -> io::Result<Self> {
        let uid = Uid::effective();
        let user = User::from_uid(uid)
            .map_err(io::Error::other)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "effective user not found"))?;
        Ok(Self {
            uid,
            name: user.name,
            home: user.dir,
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RuntimePaths {
    pub(crate) directory: PathBuf,
    pub(crate) lock: PathBuf,
    pub(crate) socket: PathBuf,
}

impl RuntimePaths {
    pub(crate) fn for_user(user: &EffectiveUser) -> Self {
        let directory = user.home.join("Library/Caches/opp/run");
        Self {
            lock: directory.join("broker.lock"),
            socket: directory.join("broker.sock"),
            directory,
        }
    }

    pub(crate) fn load() -> io::Result<Self> {
        #[cfg(feature = "test-support")]
        if let Some(directory) = std::env::var_os("OPP_TEST_RUNTIME_DIR") {
            let directory = PathBuf::from(directory);
            if !directory.is_absolute() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "test runtime directory is not absolute",
                ));
            }
            return Ok(Self::from_directory(directory));
        }
        Ok(Self::for_user(&EffectiveUser::load()?))
    }

    #[cfg(feature = "test-support")]
    fn from_directory(directory: PathBuf) -> Self {
        Self {
            lock: directory.join("broker.lock"),
            socket: directory.join("broker.sock"),
            directory,
        }
    }

    pub(crate) fn prepare_directory(&self, user: &EffectiveUser) -> io::Result<()> {
        if let Ok(metadata) = fs::symlink_metadata(&self.directory) {
            verify_directory(&metadata, user.uid.as_raw())?;
        } else {
            fs::create_dir_all(&self.directory)?;
            let metadata = fs::symlink_metadata(&self.directory)?;
            verify_directory(&metadata, user.uid.as_raw())?;
        }
        fs::set_permissions(&self.directory, fs::Permissions::from_mode(0o700))
    }

    pub(crate) fn acquire_lock(&self, user: &EffectiveUser) -> io::Result<BrokerLock> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&self.lock)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.uid() != user.uid.as_raw() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe broker lock",
            ));
        }
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        // SAFETY: `file` owns a valid descriptor for the duration of the call.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(BrokerLock { _file: file })
    }

    pub(crate) fn remove_stale_socket(&self, user: &EffectiveUser) -> io::Result<()> {
        let metadata = match fs::symlink_metadata(&self.socket) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        if !metadata.file_type().is_socket() || metadata.uid() != user.uid.as_raw() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe stale broker socket",
            ));
        }
        fs::remove_file(&self.socket)
    }
}

pub(crate) struct BrokerLock {
    _file: File,
}

fn verify_directory(metadata: &fs::Metadata, uid: u32) -> io::Result<()> {
    if !metadata.is_dir() || metadata.file_type().is_symlink() || metadata.uid() != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe broker runtime directory",
        ));
    }
    Ok(())
}

pub(crate) fn is_absent_socket_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    )
}

#[cfg(test)]
mod tests {
    use super::{EffectiveUser, RuntimePaths};
    use nix::unistd::Uid;
    use std::fs;
    use std::os::unix::fs::{FileTypeExt, PermissionsExt, symlink};
    use std::os::unix::net::UnixListener;

    fn fixture() -> (tempfile::TempDir, EffectiveUser, RuntimePaths) {
        let home = tempfile::tempdir().unwrap();
        let user = EffectiveUser {
            uid: Uid::effective(),
            name: String::from("fixture"),
            home: home.path().to_path_buf(),
        };
        let paths = RuntimePaths::for_user(&user);
        (home, user, paths)
    }

    #[test]
    fn runtime_directory_and_lock_are_private_and_singleton() {
        let (_home, user, paths) = fixture();
        paths.prepare_directory(&user).unwrap();
        assert_eq!(
            fs::metadata(&paths.directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let _lock = paths.acquire_lock(&user).unwrap();
        assert_eq!(
            fs::metadata(&paths.lock).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let error = paths.acquire_lock(&user).err().expect("second lock fails");
        assert!(matches!(
            error.raw_os_error(),
            Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN
        ));
    }

    #[test]
    fn stale_socket_removal_rejects_symlinks_and_regular_files() {
        let (_home, user, paths) = fixture();
        paths.prepare_directory(&user).unwrap();
        let target = paths.directory.join("target");
        fs::write(&target, b"canary").unwrap();
        symlink(&target, &paths.socket).unwrap();
        assert!(paths.remove_stale_socket(&user).is_err());
        assert!(
            fs::symlink_metadata(&paths.socket)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        fs::remove_file(&paths.socket).unwrap();

        fs::write(&paths.socket, b"canary").unwrap();
        assert!(paths.remove_stale_socket(&user).is_err());
        assert_eq!(fs::read(&paths.socket).unwrap(), b"canary");
        fs::remove_file(&paths.socket).unwrap();

        let listener = UnixListener::bind(&paths.socket).unwrap();
        drop(listener);
        assert!(
            fs::symlink_metadata(&paths.socket)
                .unwrap()
                .file_type()
                .is_socket()
        );
        paths.remove_stale_socket(&user).unwrap();
        assert!(!paths.socket.exists());
    }

    #[test]
    fn runtime_directory_must_not_be_a_symlink() {
        let (_home, user, paths) = fixture();
        fs::create_dir_all(paths.directory.parent().unwrap()).unwrap();
        let target = tempfile::tempdir().unwrap();
        symlink(target.path(), &paths.directory).unwrap();
        assert!(paths.prepare_directory(&user).is_err());
    }
}
