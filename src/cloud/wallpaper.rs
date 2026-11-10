//! Explicit device-card cover upload. Protocol: upstream VallierDev/uu-remote-linux #153,
//! baseline fe052cd's src/wallpaper.rs and docs/wallpaper-protocol.md. No desktop discovery.
use crate::auth::{Credentials, UUAuthClient};
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::Value;
use std::{path::Path, time::Duration};
use tokio::io::AsyncReadExt;

const IMAGE_LIMIT: usize = 10 * 1024 * 1024;
const JSON_LIMIT: usize = 64 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const TOKEN_PATH: &str = "/api/v1/tool/fp/token?filetype=1";
const REPORT_PATH: &str = "/api/v1/device/wallpaper";

#[derive(Debug, PartialEq, Eq)]
enum Failure {
    File,
    Format,
    Size,
    Credentials,
    LoginRequired,
    Client,
    Token,
    Upload,
    Report,
}
impl Failure {
    fn message(&self) -> String {
        let (zh, en) = match self {
            Self::File => ("无法读取指定的普通图片文件", "Cannot read the specified regular image file"),
            Self::Format => ("仅支持 PNG、JPEG 或 WebP 图片", "Only PNG, JPEG or WebP images are supported"),
            Self::Size => ("图片超过本地或服务器大小限制", "Image exceeds the local or server size limit"),
            Self::Credentials => ("本地设备身份或登录凭据不完整；请先运行 uuremote-server login", "Local device identity or credentials are incomplete; run uuremote-server login first"),
            Self::LoginRequired => ("登录已失效；请重新运行 uuremote-server login", "Login has expired; run uuremote-server login again"),
            Self::Client => ("无法初始化封面上传连接", "Cannot initialize the cover upload connection"),
            Self::Token => ("获取封面上传令牌失败", "Failed to obtain a cover upload token"),
            Self::Upload => ("上传封面图片失败", "Failed to upload the cover image"),
            Self::Report => ("更新设备封面地址失败", "Failed to update the device cover URL"),
        };
        crate::i18n::s(zh, en).to_owned()
    }
}

struct Image {
    bytes: Vec<u8>,
    content_type: &'static str,
}

/// Upload exactly the supplied image for the existing device identity. Never registers a device,
/// starts QR login, changes configuration, or interacts with a running mapping host.
pub async fn run(path: &Path) -> Result<(), String> {
    async {
        let image = read_image(path).await?;
        let file = crate::config::read_fresh().map_err(|_| Failure::Credentials)?;
        let credentials = Credentials::load(file.as_ref()).map_err(|_| Failure::Credentials)?;
        validate_credentials(&credentials)?;
        let auth = UUAuthClient::new(credentials);
        let http = crate::auth::http_client_builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .map_err(|_| Failure::Client)?;
        publish_to(&auth, image, crate::auth::BASE_URL, &http, REQUEST_TIMEOUT).await
    }
    .await
    .map_err(|failure: Failure| failure.message())
}

fn validate_credentials(credentials: &Credentials) -> Result<(), Failure> {
    for value in [
        &credentials.user_id,
        &credentials.device_id,
        &credentials.client_id,
        &credentials.system_id,
        &credentials.token,
        &credentials.csrf,
    ] {
        if value.trim().is_empty() || HeaderValue::from_str(value).is_err() {
            return Err(Failure::Credentials);
        }
    }
    if credentials.sign_key.trim().is_empty() {
        return Err(Failure::Credentials);
    }
    Ok(())
}

async fn read_image(path: &Path) -> Result<Image, Failure> {
    if !tokio::fs::metadata(path)
        .await
        .map_err(|_| Failure::File)?
        .is_file()
    {
        return Err(Failure::File);
    }
    // Reject special files again after opening. O_NONBLOCK prevents a replaced FIFO from
    // parking an I/O worker between metadata and open; following an image symlink is allowed.
    let file = tokio::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)
        .await
        .map_err(|_| Failure::File)?;
    let metadata = file.metadata().await.map_err(|_| Failure::File)?;
    if !metadata.is_file() {
        return Err(Failure::File);
    }
    if metadata.len() > IMAGE_LIMIT as u64 {
        return Err(Failure::Size);
    }
    let mut bytes = Vec::new();
    file.take((IMAGE_LIMIT + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| Failure::File)?;
    if bytes.len() > IMAGE_LIMIT {
        return Err(Failure::Size);
    }
    let content_type = image_type(&bytes).ok_or(Failure::Format)?;
    Ok(Image {
        bytes,
        content_type,
    })
}

fn image_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else {
        None
    }
}

fn https_url(value: &str) -> Result<reqwest::Url, ()> {
    let url = reqwest::Url::parse(value).map_err(|_| ())?;
    let userinfo = value.split_once("://").is_some_and(|(_, rest)| {
        rest.split(['/', '?', '#'])
            .next()
            .is_some_and(|authority| authority.contains('@'))
    });
    if url.scheme() != "https"
        || url.host_str().is_none()
        || userinfo
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(());
    }
    Ok(url)
}

fn account_headers(auth: &UUAuthClient, method: &str, path: &str, body: &str) -> HeaderMap {
    let mut headers = auth.build_headers(method, path, body);
    for name in ["authorization", "cookie", "x-csrf-token", "x-param-sign"] {
        if let Some(value) = headers.get_mut(name) {
            value.set_sensitive(true);
        }
    }
    headers
}

async fn response_json(
    request: reqwest::RequestBuilder,
    limit: Duration,
    account: bool,
) -> Result<(u16, Value), bool> {
    // The outer deadline covers response streaming and decoding as well as connection setup.
    tokio::time::timeout(limit, async {
        let mut response = request.timeout(limit).send().await.map_err(|_| false)?;
        let status = response.status().as_u16();
        if account && status == 401 {
            return Err(true);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| false)? {
            if bytes.len().saturating_add(chunk.len()) > JSON_LIMIT {
                return Err(false);
            }
            bytes.extend_from_slice(&chunk);
        }
        let body: Value = serde_json::from_slice(&bytes).map_err(|_| false)?;
        if account && body.get("code").and_then(Value::as_i64) == Some(crate::auth::TOKEN_INVALID) {
            return Err(true);
        }
        Ok((status, body))
    })
    .await
    .unwrap_or(Err(false))
}

fn response_failure(login_required: bool, stage: Failure) -> Failure {
    if login_required {
        Failure::LoginRequired
    } else {
        stage
    }
}

async fn publish_to(
    auth: &UUAuthClient,
    image: Image,
    base: &str,
    http: &reqwest::Client,
    timeout: Duration,
) -> Result<(), Failure> {
    let (status, body) = response_json(
        http.get(format!("{base}{TOKEN_PATH}"))
            .headers(account_headers(auth, "GET", TOKEN_PATH, "")),
        timeout,
        true,
    )
    .await
    .map_err(|login| response_failure(login, Failure::Token))?;
    if status != 200 || body.get("code").and_then(Value::as_i64) != Some(0) {
        return Err(Failure::Token);
    }
    let data = body.get("data").ok_or(Failure::Token)?;
    let token = data
        .get("token")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or(Failure::Token)?;
    let mut token = HeaderValue::from_str(token).map_err(|_| Failure::Token)?;
    token.set_sensitive(true);
    let upload = data
        .get("req_url")
        .and_then(Value::as_str)
        .ok_or(Failure::Token)?;
    let upload = https_url(upload).map_err(|_| Failure::Token)?;
    // The upstream host treats missing/zero/unusable limits as unspecified; the local
    // 10 MiB bound still applies, and any advertised positive bound further restricts it.
    let max = data
        .get("max_size")
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
        .unwrap_or(IMAGE_LIMIT as u64);
    if image.bytes.len() as u64 > max {
        return Err(Failure::Size);
    }
    // Official Filepicker uses raw bytes and the bare file token, never account credentials.
    let (status, body) = response_json(
        http.post(upload)
            .header("Authorization", token)
            .header("Content-Type", image.content_type)
            .body(image.bytes),
        timeout,
        false,
    )
    .await
    .map_err(|login| response_failure(login, Failure::Upload))?;
    if status != 200 {
        return Err(Failure::Upload);
    }
    let url = body
        .get("url")
        .and_then(Value::as_str)
        .ok_or(Failure::Upload)?;
    https_url(url).map_err(|_| Failure::Upload)?;
    let body = serde_json::json!({"wallpaper_url": url}).to_string();
    let (status, reply) = response_json(
        http.post(format!("{base}{REPORT_PATH}"))
            .headers(account_headers(auth, "POST", REPORT_PATH, &body))
            .body(body),
        timeout,
        true,
    )
    .await
    .map_err(|login| response_failure(login, Failure::Report))?;
    if status != 200 || reply.get("code").and_then(Value::as_i64) != Some(0) {
        return Err(Failure::Report);
    }
    Ok(())
}

#[cfg(test)]
#[path = "wallpaper_tests.rs"]
mod tests;
