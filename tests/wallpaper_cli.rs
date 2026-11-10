//! Real CLI + TLS integration with synthetic credentials and a non-forwarding CONNECT proxy.
//! Every origin request terminates on loopback; no cloud account, DNS or GUI is used.
#![cfg(unix)]

use base64::Engine;
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStringExt;
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs::{File, OpenOptions},
    os::unix::{fs::OpenOptionsExt, io::AsRawFd},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tokio_rustls::{rustls, TlsAcceptor};

const API: &str = "api.nrd.nie.163.com";
const UPLOAD: &str = "wallpaper-upload.fixture.invalid";
const IMAGE_URL: &str = "https://wallpaper-image.fixture.invalid/cover.png";
const ACCOUNT_TOKEN: &str = "synthetic-account-token";
const FILE_TOKEN: &str = "synthetic-file-token";
const LIMIT: Duration = Duration::from_secs(20);

struct Fixture {
    path: PathBuf,
    config: Vec<u8>,
    png: Vec<u8>,
}
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "headless-wallpaper-cli-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir(&path).unwrap();
        let config = serde_json::to_vec_pretty(&serde_json::json!({
            "user_id":"synthetic-user", "device_id":"synthetic-device",
            "client_id":"synthetic-client", "system_id":"synthetic-system",
            "token":ACCOUNT_TOKEN, "csrf":"synthetic-csrf", "sign_key":"synthetic-sign-key",
            "preserve_exact":42
        }))
        .unwrap();
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path.join("config.json"))
            .unwrap();
        std::io::Write::write_all(&mut file, &config).unwrap();
        // Complete synthetic one-pixel PNG, not a screenshot or a fixture from an account.
        let png = base64::engine::general_purpose::STANDARD.decode(
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR4nGNg+A8AAQIBAEK+vGgAAAAASUVORK5CYII="
        ).unwrap();
        for name in ["stdout.log", "stderr.log"] {
            File::create(path.join(name)).unwrap();
        }
        Self { path, config, png }
    }
    fn entries(&self) -> BTreeSet<OsString> {
        std::fs::read_dir(&self.path)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect()
    }
    fn command(&self, proxy: &str, image: &Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_uuremote-server"));
        command
            .env_clear()
            .current_dir(&self.path)
            .env("HOME", &self.path)
            .env("UU_CONFIG", self.path.join("config.json"))
            .env("UU_LANG", "en")
            .env("RUST_LOG", "warn")
            .env("UU_USE_PROXY", "1")
            .env("HTTPS_PROXY", proxy)
            .env("HTTP_PROXY", proxy)
            // rustls-native-certs 0.8 reads this on Linux and macOS before native roots.
            .env("SSL_CERT_FILE", self.path.join("ca.pem"))
            .arg("set-wallpaper");
        if image.as_os_str().as_encoded_bytes().starts_with(b"-") {
            command.arg("--");
        }
        command
            .arg(image)
            .stdin(Stdio::null())
            .stdout(File::create(self.path.join("stdout.log")).unwrap())
            .stderr(File::create(self.path.join("stderr.log")).unwrap());
        command
    }
    async fn run(&self, proxy: &str, image: &Path) -> (ExitStatus, String, String) {
        let mut child = Child(self.command(proxy, image).spawn().unwrap());
        let deadline = Instant::now() + LIMIT;
        let status = loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "wallpaper CLI exceeded its process deadline"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        let stdout = std::fs::read_to_string(self.path.join("stdout.log")).unwrap();
        let stderr = std::fs::read_to_string(self.path.join("stderr.log")).unwrap();
        (status, stdout, stderr)
    }
    fn unchanged(&self, entries: BTreeSet<OsString>) {
        assert_eq!(
            std::fs::read(self.path.join("config.json")).unwrap(),
            self.config
        );
        assert_eq!(
            self.entries(),
            entries,
            "CLI must not generate identity or login files"
        );
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Request {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}
async fn headers(stream: &mut (impl AsyncRead + Unpin)) -> String {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        assert!(bytes.len() < 32 * 1024, "mock HTTP header limit exceeded");
        bytes.push(stream.read_u8().await.unwrap());
    }
    String::from_utf8(bytes).unwrap()
}
async fn request(stream: &mut (impl AsyncRead + Unpin)) -> Request {
    let text = headers(stream).await;
    let mut lines = text.split("\r\n");
    let mut first = lines.next().unwrap().split_whitespace();
    let method = first.next().unwrap().to_owned();
    let path = first.next().unwrap().to_owned();
    assert_eq!(first.next(), Some("HTTP/1.1"));
    let headers: BTreeMap<_, _> = lines
        .filter(|line| !line.is_empty())
        .map(|line| {
            let (name, value) = line.split_once(':').unwrap();
            (name.to_ascii_lowercase(), value.trim().to_owned())
        })
        .collect();
    assert!(!headers.contains_key("transfer-encoding"));
    let length: usize = headers
        .get("content-length")
        .map(|length| length.parse().unwrap())
        .unwrap_or(0);
    assert!(length <= 32 * 1024);
    let mut body = vec![0; length];
    stream.read_exact(&mut body).await.unwrap();
    Request {
        method,
        path,
        headers,
        body,
    }
}

struct Mock {
    proxy: String,
    task: tokio::task::JoinHandle<Vec<Request>>,
}
impl Mock {
    async fn start(fixture: &Fixture, unauthorized: bool) -> Self {
        let mut parameters = rcgen::CertificateParams::default();
        parameters.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca = rcgen::Certificate::from_params(parameters).unwrap();
        std::fs::write(fixture.path.join("ca.pem"), ca.serialize_pem().unwrap()).unwrap();
        let leaf = rcgen::generate_simple_self_signed(vec![API.into(), UPLOAD.into()]).unwrap();
        use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(
                leaf.serialize_der_with_signer(&ca).unwrap(),
            )],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf.serialize_private_key_der())),
        )
        .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            tokio::time::timeout(LIMIT, async move {
                let mut captured = Vec::new();
                let origins = if unauthorized { vec![API] } else { vec![API, UPLOAD, API] };
                for (index, origin) in origins.into_iter().enumerate() {
                    let (mut tcp, _) = listener.accept().await.unwrap();
                    let connect = headers(&mut tcp).await;
                    assert_eq!(connect.lines().next().unwrap(), format!("CONNECT {origin}:443 HTTP/1.1"));
                    assert!(!connect.contains(ACCOUNT_TOKEN) && !connect.contains(FILE_TOKEN));
                    // This server never connects to the supplied authority or forwards any bytes.
                    tcp.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n").await.unwrap();
                    let mut tls = acceptor.accept(tcp).await.unwrap();
                    let received = request(&mut tls).await;
                    assert_eq!(received.headers.get("host").map(String::as_str), Some(origin));
                    captured.push(received);
                    let (status, body) = if unauthorized {
                        ("401 Unauthorized", serde_json::json!({"code":1120,"message":"synthetic-denial-detail"}))
                    } else { match index {
                        0 => ("200 OK", serde_json::json!({"code":0,"data":{
                            "token":FILE_TOKEN, "req_url":format!("https://{UPLOAD}/upload"),
                            "ttl":60, "max_size":10485760
                        }})),
                        1 => ("200 OK", serde_json::json!({"url":IMAGE_URL})),
                        2 => ("200 OK", serde_json::json!({"code":0})),
                        _ => unreachable!(),
                    }};
                    let body = body.to_string();
                    let response = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    tls.write_all(response.as_bytes()).await.unwrap();
                    tls.shutdown().await.unwrap();
                }
                captured
            }).await.expect("wallpaper mock exceeded its deadline")
        });
        Self { proxy, task }
    }
    async fn finish(&mut self) -> Vec<Request> {
        tokio::time::timeout(LIMIT, &mut self.task)
            .await
            .unwrap()
            .unwrap()
    }
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn account_headers(request: &Request) {
    assert_eq!(
        request.headers.get("authorization").map(String::as_str),
        Some("Bearer synthetic-account-token")
    );
    assert_eq!(
        request.headers.get("x-param-device-id").map(String::as_str),
        Some("synthetic-device")
    );
    assert_eq!(
        request.headers.get("x-param-user-id").map(String::as_str),
        Some("synthetic-user")
    );
    assert_eq!(
        request.headers.get("x-csrf-token").map(String::as_str),
        Some("synthetic-csrf")
    );
    assert!(request
        .headers
        .get("x-param-sign")
        .is_some_and(|signature| !signature.is_empty()));
}

#[tokio::test]
async fn wallpaper_cli_uploads_exact_file_with_host_running_and_preserves_identity() {
    let names = vec![
        OsString::from("device card cover.png"),
        OsString::from("-cover.png"),
    ];
    // macOS filesystems reject invalid UTF-8 names at file creation; Ubuntu permits them.
    #[cfg(target_os = "linux")]
    let names = {
        let mut names = names;
        names.push(OsString::from_vec(b"cover \xff.png".to_vec()));
        names
    };
    for name in names {
        let fixture = Fixture::new();
        let image = fixture.path.join(&name);
        std::fs::write(&image, &fixture.png).unwrap();
        // Model an existing host without starting cloud signaling or taking any real host lock.
        let lock = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(fixture.path.join("config.host.lock"))
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let mut mock = Mock::start(&fixture, false).await;
        let entries = fixture.entries();
        // Exercise a relative path and explicit `--` through the actual process too.
        let argument = if name == "-cover.png" {
            Path::new(&name)
        } else {
            &image
        };
        let (status, stdout, stderr) = fixture.run(&mock.proxy, argument).await;
        assert!(status.success(), "wallpaper CLI failed: {stderr}");
        assert!(stdout.contains("Device card wallpaper"));
        assert!(stderr.is_empty());
        let requests = mock.finish().await;
        assert_eq!(requests.len(), 3);
        let token = &requests[0];
        assert_eq!(
            (&*token.method, &*token.path),
            ("GET", "/api/v1/tool/fp/token?filetype=1")
        );
        assert!(token.body.is_empty());
        account_headers(token);
        let upload = &requests[1];
        assert_eq!((&*upload.method, &*upload.path), ("POST", "/upload"));
        assert_eq!(
            upload.headers.get("authorization").map(String::as_str),
            Some(FILE_TOKEN)
        );
        assert_eq!(
            upload.headers.get("content-type").map(String::as_str),
            Some("image/png")
        );
        assert!(
            !upload.headers.contains_key("cookie") && !upload.headers.contains_key("x-csrf-token")
        );
        assert!(!upload.headers.keys().any(|key| key.starts_with("x-param-")));
        assert_eq!(upload.body, fixture.png);
        let report = &requests[2];
        assert_eq!(
            (&*report.method, &*report.path),
            ("POST", "/api/v1/device/wallpaper")
        );
        account_headers(report);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&report.body).unwrap(),
            serde_json::json!({"wallpaper_url":IMAGE_URL})
        );
        for secret in [
            ACCOUNT_TOKEN,
            FILE_TOKEN,
            IMAGE_URL,
            "synthetic-device",
            "synthetic-csrf",
        ] {
            assert!(!stdout.contains(secret) && !stderr.contains(secret));
        }
        fixture.unchanged(entries);
    }
}

#[tokio::test]
async fn wallpaper_cli_reports_expired_login_without_upload_or_config_changes() {
    let fixture = Fixture::new();
    let image = fixture.path.join("cover.png");
    std::fs::write(&image, &fixture.png).unwrap();
    let mut mock = Mock::start(&fixture, true).await;
    let entries = fixture.entries();
    let (status, stdout, stderr) = fixture.run(&mock.proxy, &image).await;
    assert_eq!(status.code(), Some(1));
    assert!(stdout.is_empty());
    assert!(stderr.contains("Login has expired"));
    for secret in [
        ACCOUNT_TOKEN,
        FILE_TOKEN,
        "synthetic-denial-detail",
        API,
        UPLOAD,
    ] {
        assert!(!stderr.contains(secret));
    }
    let requests = mock.finish().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/api/v1/tool/fp/token?filetype=1");
    fixture.unchanged(entries);
}
