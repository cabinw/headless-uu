//! Process-level checks use an empty, isolated configuration; no cloud or account is contacted.
use std::{
    io::Read,
    path::PathBuf,
    process::{Command, Output, Stdio},
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
fn bounded_output(mut command: Command) -> Output {
    let mut child = Child(
        command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "CLI command did not finish");
        std::thread::sleep(Duration::from_millis(20));
    };
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    child
        .0
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut stdout)
        .unwrap();
    child
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_end(&mut stderr)
        .unwrap();
    Output {
        status,
        stdout,
        stderr,
    }
}

#[test]
fn wallpaper_cli_help_and_invalid_arguments_never_create_identity() {
    let fixture = Fixture::new();
    for args in [
        vec!["set-wallpaper"],
        vec!["set-wallpaper", "--unknown"],
        vec!["set-wallpaper", "one", "two"],
        vec!["set-wallpaper", "--"],
    ] {
        let mut command = fixture.command();
        command.args(args);
        assert_eq!(bounded_output(command).status.code(), Some(2));
    }
    for flag in ["--help", "-h"] {
        let mut command = fixture.command();
        command.args(["set-wallpaper", flag]);
        let output = bounded_output(command);
        assert!(output.status.success());
        let help = String::from_utf8_lossy(&output.stdout);
        assert!(help.contains("PNG/JPEG/WebP"));
        assert!(help.contains("UU's file service"));
    }
    assert_eq!(std::fs::read_dir(&fixture.0).unwrap().count(), 0);
}

#[test]
fn wallpaper_cli_reports_local_failures_without_leaking_data_or_writing_credentials() {
    use base64::Engine;
    let fixture = Fixture::new();
    let image = fixture.0.join("private-path-封面 image.png");
    std::fs::write(&image, base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR4nGNg+A8AAQIBAEK+vGgAAAAASUVORK5CYII=").unwrap()).unwrap();
    let invalid = fixture.0.join("private-path-not-image.txt");
    std::fs::write(&invalid, b"private-file-content").unwrap();
    let large = fixture.0.join("private-path-large.png");
    std::fs::File::create(&large)
        .unwrap()
        .set_len(10 * 1024 * 1024 + 1)
        .unwrap();
    let missing = fixture.0.join("private-path-missing.png");
    for path in [&invalid, &large, &missing, &fixture.0, &image] {
        let mut command = fixture.command();
        command.arg("set-wallpaper").arg(path);
        let output = bounded_output(command);
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("Operation failed"));
        for private in ["private-path", "private-file-content"] {
            assert!(!error.contains(private));
        }
        assert!(!fixture.0.join("config.json").exists());
        assert!(!fixture.0.join("config.host.lock").exists());
    }
    // A malformed or incomplete configuration is reported without echoing its contents,
    // auto-login, or replacing the user's existing identity.
    let config = fixture.0.join("config.json");
    for bytes in [
        b"private-invalid-config".as_slice(),
        br#"{"token":"private-account-token"}"#.as_slice(),
    ] {
        std::fs::write(&config, bytes).unwrap();
        let mut command = fixture.command();
        command.arg("set-wallpaper").arg(&image);
        let output = bounded_output(command);
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(!error.contains("private-"));
        assert!(!error.contains(&fixture.0.to_string_lossy().to_string()));
        assert_eq!(std::fs::read(&config).unwrap(), bytes);
    }
    assert_eq!(std::fs::read_dir(&fixture.0).unwrap().count(), 4);
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
