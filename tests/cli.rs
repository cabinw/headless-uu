//! Process-level checks use an empty, isolated configuration; no cloud or account is contacted.
use std::{
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "headless-cli-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_uuremote-server"));
        cmd.env_clear()
            .env("HOME", &self.0)
            .env("UU_CONFIG", self.0.join("config.json"))
            .env("UU_LANG", "en")
            .env("RUST_LOG", "warn");
        cmd
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[test]
fn cli_rejects_removed_tools_and_redirected_qr_without_creating_identity() {
    let fixture = Fixture::new();
    for args in [
        vec!["desktop-probe"],
        vec!["set-encoder", "cpu"],
        vec!["login", "--gui"],
    ] {
        assert_eq!(
            fixture.command().args(args).output().unwrap().status.code(),
            Some(2)
        );
    }
    for arg in ["help", "version", "doctor"] {
        assert!(fixture
            .command()
            .arg(arg)
            .output()
            .unwrap()
            .status
            .success());
    }
    let login = fixture.command().arg("login").output().unwrap();
    assert!(!login.status.success());
    assert!(login.stdout.is_empty());
    assert!(String::from_utf8_lossy(&login.stderr).contains("interactive terminal"));
    assert_eq!(std::fs::read_dir(&fixture.0).unwrap().count(), 0);
}
#[test]
fn empty_config_daemon_stops_on_sigterm_and_releases_instance_lock() {
    let fixture = Fixture::new();
    for _ in 0..2 {
        let log = fixture.0.join("run.log");
        let output = std::fs::File::create(&log).unwrap();
        let mut child = Child(
            fixture
                .command()
                .arg("run")
                .stdout(Stdio::null())
                .stderr(output)
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "daemon exited before waiting for login"
            );
            // The duplicate invocation is an end-to-end check that the first owns the lock.
            if std::fs::read_to_string(&log)
                .unwrap_or_default()
                .contains("UU login required")
            {
                let mut contender = Child(
                    fixture
                        .command()
                        .arg("run")
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                        .unwrap(),
                );
                let limit = Instant::now() + Duration::from_secs(2);
                let status = loop {
                    if let Some(status) = contender.0.try_wait().unwrap() {
                        break status;
                    }
                    assert!(
                        Instant::now() < limit,
                        "second host did not reject duplicate identity"
                    );
                    std::thread::sleep(Duration::from_millis(20));
                };
                assert_eq!(status.code(), Some(1));
                break;
            }
            assert!(Instant::now() < deadline, "host lock did not appear");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(unsafe { libc::kill(child.0.id() as i32, libc::SIGTERM) }, 0);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(
                Instant::now() < deadline,
                "SIGTERM did not end credential wait"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!fixture.0.join("config.json").exists());
    }
}
