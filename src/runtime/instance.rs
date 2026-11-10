//! A process lock scoped to this installation, independent of the interactive login lock.
use std::os::unix::{
    fs::{DirBuilderExt, OpenOptionsExt},
    io::AsRawFd,
};
use std::{
    fs::{File, OpenOptions},
    path::Path,
};
pub struct HostLock {
    _file: File,
}
impl HostLock {
    pub fn acquire(config: &Path) -> Result<Self, String> {
        if let Some(parent) = config.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)
                .map_err(|_| {
                    crate::i18n::s("无法创建配置目录", "Cannot create configuration directory")
                        .to_owned()
                })?;
        }
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(config.with_extension("host.lock"))
            .map_err(|_| {
                crate::i18n::s("无法打开宿主锁文件", "Cannot open host lock file").to_owned()
            })?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(crate::i18n::s(
                "此配置已有宿主运行",
                "A host is already running with this configuration",
            )
            .into());
        }
        Ok(Self { _file: file })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn single_host_lock_releases_and_does_not_follow_symlinks() {
        let path = std::env::temp_dir().join(format!("uu-host-lock-{}", rand::random::<u64>()));
        let config = path.join("config.json");
        let lock = HostLock::acquire(&config).unwrap();
        assert!(HostLock::acquire(&config).is_err());
        drop(lock);
        drop(HostLock::acquire(&config).unwrap());
        let lockpath = config.with_extension("host.lock");
        std::fs::remove_file(&lockpath).unwrap();
        std::os::unix::fs::symlink(path.join("other"), lockpath).unwrap();
        assert!(HostLock::acquire(&config).is_err());
        assert!(!path.join("other").exists());
        std::fs::remove_dir_all(path).unwrap();
    }
}
