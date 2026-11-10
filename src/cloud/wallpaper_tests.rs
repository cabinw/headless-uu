use super::*;
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
use tokio::{io::AsyncWriteExt, net::TcpListener};
use tokio_rustls::{rustls, TlsAcceptor};

const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfixture-image-bytes";
const FILE_TOKEN: &str = "fixture-file-token";
const COVER_URL: &str = "https://image.fixture.invalid/cover.png?fixture-url-secret";

fn credentials() -> Credentials {
    Credentials {
        user_id: "fixture-user".into(),
        device_id: "fixture-device".into(),
        client_id: "fixture-client".into(),
        system_id: "fixture-system".into(),
        token: "fixture-account-token".into(),
        csrf: "fixture-csrf".into(),
        sign_key: "fixture-sign-key".into(),
    }
}

fn image() -> Image {
    Image {
        bytes: PNG.to_vec(),
        content_type: "image/png",
    }
}

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("uu-wallpaper-{}", rand::random::<u64>()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn reads_only_regular_supported_images_and_bounds_file_bytes() {
    let root = Directory::new();
    let path = root.0.join("fixture-private-path.bin");
    assert!(matches!(read_image(&path).await, Err(Failure::File)));
    assert!(matches!(read_image(&root.0).await, Err(Failure::File)));
    for (bytes, mime) in [
        (PNG, "image/png"),
        (b"\xff\xd8\xfffixture".as_slice(), "image/jpeg"),
        (b"RIFF1234WEBPfixture".as_slice(), "image/webp"),
    ] {
        std::fs::write(&path, bytes).unwrap();
        let loaded = read_image(&path).await.unwrap();
        assert_eq!(loaded.bytes, bytes);
        assert_eq!(loaded.content_type, mime);
    }
    for bytes in [b"private text".as_slice(), b"", b"RIFF", b"GIF89a"] {
        std::fs::write(&path, bytes).unwrap();
        assert!(matches!(read_image(&path).await, Err(Failure::Format)));
    }
    std::fs::write(&path, PNG).unwrap();
    let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.set_len(IMAGE_LIMIT as u64).unwrap();
    assert_eq!(read_image(&path).await.unwrap().bytes.len(), IMAGE_LIMIT);
    file.set_len((IMAGE_LIMIT + 1) as u64).unwrap();
    assert!(matches!(read_image(&path).await, Err(Failure::Size)));
    file.set_len(PNG.len() as u64).unwrap();
    let link = root.0.join("image-link");
    std::os::unix::fs::symlink(&path, &link).unwrap();
    assert_eq!(read_image(&link).await.unwrap().bytes, PNG);
    let fifo = root.0.join("fifo");
    let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), read_image(&fifo))
            .await
            .unwrap(),
        Err(Failure::File)
    ));
}

#[test]
fn rejects_incomplete_identity_or_header_injection_without_exposing_values() {
    let valid = credentials();
    assert!(validate_credentials(&valid).is_ok());
    for field in 0..7 {
        for invalid in ["", "   ", "fixture\r\nsecret"] {
            if field == 6 && invalid.contains('\r') {
                continue;
            }
            let mut account = valid.clone();
            let target = match field {
                0 => &mut account.user_id,
                1 => &mut account.device_id,
                2 => &mut account.client_id,
                3 => &mut account.system_id,
                4 => &mut account.token,
                5 => &mut account.csrf,
                _ => &mut account.sign_key,
            };
            *target = invalid.into();
            assert_eq!(validate_credentials(&account), Err(Failure::Credentials));
        }
    }
    let auth = UUAuthClient::new(valid);
    let headers = account_headers(&auth, "GET", TOKEN_PATH, "");
    for name in ["authorization", "cookie", "x-csrf-token", "x-param-sign"] {
        assert!(headers[name].is_sensitive());
    }
}

#[test]
fn upload_and_report_urls_require_https_without_userinfo_or_fragments() {
    for url in [
        "http://fixture.invalid/upload",
        "file:///fixture-private-path",
        "relative.png",
        "https://fixture-user:fixture-password@fixture.invalid/upload",
        "https://@fixture.invalid/upload",
        "https://fixture.invalid/upload#fixture-secret",
        "https://fixture.invalid/upload#",
        "https://",
    ] {
        assert!(https_url(url).is_err(), "{url}");
    }
    assert!(https_url("https://fixture.invalid/upload?fixture-query-secret").is_ok());
}

struct Environment(Vec<(&'static str, Option<std::ffi::OsString>)>);
impl Environment {
    fn isolated(config: &Path) -> Self {
        let names = [
            "UU_CONFIG",
            "UU_USER_ID",
            "UU_DEVICE_ID",
            "UU_CLIENT_ID",
            "UU_SYSTEM_ID",
            "UU_TOKEN",
            "UU_CSRF",
            "UU_SIGN_KEY",
        ];
        let saved = names
            .into_iter()
            .map(|name| {
                let previous = std::env::var_os(name);
                std::env::remove_var(name);
                (name, previous)
            })
            .collect();
        std::env::set_var("UU_CONFIG", config);
        Self(saved)
    }
}
impl Drop for Environment {
    fn drop(&mut self) {
        for (name, value) in &self.0 {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

#[tokio::test]
async fn missing_or_malformed_credentials_fail_locally_without_identity_writes() {
    let _guard = crate::config::test_env_lock().lock().unwrap();
    let root = Directory::new();
    let config = root.0.join("fixture-private-config.json");
    let _environment = Environment::isolated(&config);
    let path = root.0.join("fixture-private-image.png");
    std::fs::write(&path, PNG).unwrap();
    assert!(run(&path)
        .await
        .unwrap_err()
        .contains("uuremote-server login"));
    assert!(!config.exists());
    for body in [b"fixture-secret-invalid-json".as_slice(), b"{}"] {
        std::fs::write(&config, body).unwrap();
        let error = run(&path).await.unwrap_err();
        assert!(error.contains("uuremote-server login"));
        assert!(!error.contains("fixture"));
        assert_eq!(std::fs::read(&config).unwrap(), body);
    }
    let error = run(&root.0.join("fixture-missing-image"))
        .await
        .unwrap_err();
    assert!(!error.contains("login"));
    assert!(!error.contains("fixture"));
    assert_eq!(std::fs::read_dir(&root.0).unwrap().count(), 2);
}

fn tls() -> (TlsAcceptor, reqwest::Client) {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let der = cert.serialize_der().unwrap();
    let http = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .add_root_certificate(reqwest::Certificate::from_der(&der).unwrap())
        .build()
        .unwrap();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![CertificateDer::from(der)],
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.serialize_private_key_der())),
    )
    .unwrap();
    (TlsAcceptor::from(Arc::new(config)), http)
}

async fn read_request(
    stream: &mut tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
) -> (String, BTreeMap<String, String>, Vec<u8>) {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).await.unwrap();
        bytes.push(byte[0]);
        assert!(bytes.len() < 16 * 1024);
    }
    let headers = String::from_utf8(bytes).unwrap();
    let request = headers.lines().next().unwrap().to_owned();
    let headers: BTreeMap<_, _> = headers
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let length = headers
        .get("content-length")
        .map(|value| value.parse::<usize>().unwrap())
        .unwrap_or(0);
    assert!(length < 1024);
    let mut body = vec![0; length];
    stream.read_exact(&mut body).await.unwrap();
    (request, headers, body)
}

fn verify_request(step: usize, request: &str, headers: &BTreeMap<String, String>, body: &[u8]) {
    let auth = UUAuthClient::new(credentials());
    if step == 1 {
        assert_eq!(request, "POST /upload HTTP/1.1");
        assert_eq!(headers["authorization"], FILE_TOKEN);
        assert_eq!(headers["content-type"], "image/png");
        assert_eq!(body, PNG);
        assert!(!headers.contains_key("cookie"));
        assert!(!headers.contains_key("x-csrf-token"));
        assert!(!headers.keys().any(|name| name.starts_with("x-param-")));
        assert!(!headers
            .values()
            .any(|value| value.contains("fixture-account") || value.contains("fixture-csrf")));
        return;
    }
    let (method, path) = if step == 0 {
        ("GET", TOKEN_PATH)
    } else {
        ("POST", REPORT_PATH)
    };
    assert_eq!(request, format!("{method} {path} HTTP/1.1"));
    assert_eq!(headers["authorization"], "Bearer fixture-account-token");
    assert_eq!(headers["x-csrf-token"], "fixture-csrf");
    assert_eq!(
        headers["cookie"],
        "nrd_access_token=fixture-account-token; nrd_csrf_access_token=fixture-csrf"
    );
    for (name, expected) in [
        ("x-param-plat", "1"),
        ("x-param-user-id", "fixture-user"),
        ("x-param-device-id", "fixture-device"),
        ("x-param-client-id", "fixture-client"),
        ("x-param-system-id", "fixture-system"),
    ] {
        assert_eq!(headers[name], expected);
    }
    assert!(!headers.values().any(|value| value.contains(FILE_TOKEN)));
    let text = std::str::from_utf8(body).unwrap();
    assert_eq!(
        headers["x-param-sign"],
        auth.sign_request(method, path, headers, text)
    );
    if step == 0 {
        assert!(body.is_empty());
        assert_ne!(
            headers["x-param-sign"],
            auth.sign_request(method, "/api/v1/tool/fp/token", headers, "")
        );
    } else {
        assert_eq!(
            serde_json::from_slice::<Value>(body).unwrap(),
            serde_json::json!({"wallpaper_url": COVER_URL})
        );
        assert_ne!(
            headers["x-param-sign"],
            auth.sign_request(method, path, headers, "")
        );
    }
}

#[derive(Clone)]
struct Reply {
    status: u16,
    body: String,
    redirect: Option<String>,
}
fn json_reply(value: Value) -> Reply {
    Reply {
        status: 200,
        body: value.to_string(),
        redirect: None,
    }
}
fn success(base: &str) -> Vec<Reply> {
    vec![
        json_reply(
            serde_json::json!({"code":0,"data":{"token":FILE_TOKEN,"req_url":format!("{base}/upload"),"ttl":60,"max_size":1024}}),
        ),
        json_reply(serde_json::json!({"url":COVER_URL,"fsize":PNG.len(),"mime":"image/png"})),
        json_reply(serde_json::json!({"code":0})),
    ]
}

async fn exchange(change: impl FnOnce(&str, &mut Vec<Reply>), expected: Result<(), Failure>) {
    let (acceptor, http) = tls();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!(
        "https://localhost:{}",
        listener.local_addr().unwrap().port()
    );
    let mut replies = success(&base);
    change(&base, &mut replies);
    let server = tokio::spawn(async move {
        for (step, reply) in replies.into_iter().enumerate() {
            let (socket, _) = listener.accept().await.unwrap();
            let mut stream = acceptor.accept(socket).await.unwrap();
            let (request, headers, body) = read_request(&mut stream).await;
            verify_request(step, &request, &headers, &body);
            let location = reply
                .redirect
                .map(|url| format!("Location: {url}\r\n"))
                .unwrap_or_default();
            let response = format!("HTTP/1.1 {} Fixture\r\n{location}Content-Length: {}\r\nConnection: close\r\n\r\n{}", reply.status, reply.body.len(), reply.body);
            // The response size guard may close the connection before all bytes are written.
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.shutdown().await;
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(120), listener.accept())
                .await
                .is_err(),
            "unexpected request after the terminal response"
        );
    });
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        publish_to(
            &UUAuthClient::new(credentials()),
            image(),
            &base,
            &http,
            Duration::from_secs(2),
        ),
    )
    .await
    .unwrap();
    assert_eq!(result, expected);
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn upload_protocol_signs_query_and_body_and_separates_file_credentials() {
    exchange(|_, _| {}, Ok(())).await;
    for limit in [
        None,
        Some(Value::Null),
        Some(Value::from(0)),
        Some(Value::from("unspecified")),
    ] {
        exchange(
            |_, replies| {
                let mut body: Value = serde_json::from_str(&replies[0].body).unwrap();
                match limit {
                    Some(value) => body["data"]["max_size"] = value,
                    None => {
                        body["data"].as_object_mut().unwrap().remove("max_size");
                    }
                }
                replies[0].body = body.to_string();
            },
            Ok(()),
        )
        .await;
    }
}

#[tokio::test]
async fn every_http_api_decode_or_missing_field_failure_stops_the_sequence() {
    for step in 0..3 {
        for failure in ["status", "malformed", "missing", "redirect", "code"] {
            if step == 1 && failure == "code" {
                continue;
            }
            let expected = match step {
                0 => Failure::Token,
                1 => Failure::Upload,
                _ => Failure::Report,
            };
            exchange(
                |base, replies| {
                    replies.truncate(step + 1);
                    match failure {
                        "status" => replies[step].status = 503,
                        "malformed" => replies[step].body = "fixture-secret-invalid-json".into(),
                        "missing" => replies[step].body = "{}".into(),
                        "code" => {
                            replies[step].body =
                                r#"{"code":777,"msg":"fixture-server-secret"}"#.into()
                        }
                        "redirect" => {
                            replies[step].status = 307;
                            replies[step].redirect =
                                Some(format!("{base}/fixture-redirect-secret"));
                        }
                        _ => unreachable!(),
                    }
                },
                Err(expected),
            )
            .await;
        }
    }
    for step in [0, 2] {
        for unauthorized in [true, false] {
            exchange(
                |_, replies| {
                    replies.truncate(step + 1);
                    replies[step].status = if unauthorized { 401 } else { 422 };
                    replies[step].body = if unauthorized {
                        "fixture-malformed-private-body".into()
                    } else {
                        r#"{"code":1120,"msg":"fixture-secret"}"#.into()
                    };
                },
                Err(Failure::LoginRequired),
            )
            .await;
        }
    }
}

#[tokio::test]
async fn unsafe_urls_missing_tokens_and_server_size_limits_abort_before_next_stage() {
    for (field, value, expected) in [
        ("token", Value::Null, Failure::Token),
        ("token", Value::String("".into()), Failure::Token),
        (
            "token",
            Value::String("fixture\r\nsecret".into()),
            Failure::Token,
        ),
        ("req_url", Value::Null, Failure::Token),
        (
            "req_url",
            Value::String("http://localhost/fixture-secret".into()),
            Failure::Token,
        ),
        (
            "req_url",
            Value::String("https://fixture:secret@localhost/upload".into()),
            Failure::Token,
        ),
        (
            "req_url",
            Value::String("https://localhost/upload#fixture-secret".into()),
            Failure::Token,
        ),
        ("max_size", Value::from(1), Failure::Size),
    ] {
        exchange(
            |_, replies| {
                replies.truncate(1);
                let mut body: Value = serde_json::from_str(&replies[0].body).unwrap();
                body["data"][field] = value;
                replies[0].body = body.to_string();
            },
            Err(expected),
        )
        .await;
    }
    for url in [
        "http://fixture.invalid/image",
        "https://user:secret@fixture.invalid/image",
        "https://fixture.invalid/image#secret",
    ] {
        exchange(
            |_, replies| {
                replies.truncate(2);
                replies[1].body = serde_json::json!({"url": url}).to_string();
            },
            Err(Failure::Upload),
        )
        .await;
    }
}

#[tokio::test]
async fn response_size_limit_accepts_boundary_and_rejects_larger_at_every_stage() {
    exchange(
        |_, replies| {
            for reply in replies {
                let mut body: Value = serde_json::from_str(&reply.body).unwrap();
                body["padding"] = Value::String(String::new());
                let extra = JSON_LIMIT - body.to_string().len();
                body["padding"] = Value::String("x".repeat(extra));
                reply.body = body.to_string();
                assert_eq!(reply.body.len(), JSON_LIMIT);
            }
        },
        Ok(()),
    )
    .await;
    for step in 0..3 {
        let expected = match step {
            0 => Failure::Token,
            1 => Failure::Upload,
            _ => Failure::Report,
        };
        exchange(
            |_, replies| {
                replies.truncate(step + 1);
                replies[step].body = format!("\"{}\"", "x".repeat(JSON_LIMIT));
            },
            Err(expected),
        )
        .await;
    }
}

#[tokio::test]
async fn deadline_covers_stalled_response_headers_and_streaming_body() {
    for with_headers in [false, true] {
        let (acceptor, http) = tls();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "https://localhost:{}/fixture-secret",
            listener.local_addr().unwrap().port()
        );
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut stream = acceptor.accept(socket).await.unwrap();
            let _ = read_request(&mut stream).await;
            if with_headers {
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 128\r\n\r\n{")
                    .await
                    .unwrap();
            }
            std::future::pending::<()>().await;
        });
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            response_json(http.get(url), Duration::from_millis(120), true),
        )
        .await
        .unwrap();
        assert!(matches!(result, Err(false)));
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }
}
