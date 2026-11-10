//! `uuremote-server login`: register this machine as a UU PC host and bind it to an account
//! by scanning a QR code with the UU mobile app, the same flow as the official clients:
//!   1. POST /api/v1/device/windows/init (anonymous, hardware summary) -> device_id
//!   2. POST /api/v1/qrcode/gen/login -> qrcode_jump_url (QR content), token (login ticket)
//!   3. POST /api/v1/qrcode/login/status {token, qrcode_jump_url, login_status}: long poll, 1..4
//!   4. POST /api/v1/login/by_qrcode {token, qrcode_jump_url} -> user_id, token (JWT carries csrf)
//! Field names: server 422 validation and the official Mac client's Codable models
//! (analysis/tools-fix-20261013/viewer-fieldmd.json). Results are written to the private
//! config file (chmod 600). Tokens and tickets are never printed or logged.

use crate::auth::{Credentials, QrRequestErrorKind, UUAuthClient};
use serde_json::{json, Value};
use std::time::Duration;

fn read(path: &str) -> String {
    std::fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn random_uuid() -> String {
    let mut b = rand::random::<[u8; 16]>();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex::encode(b);
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

/// /etc/machine-id as a GUID, the closest Linux analogue of Windows MachineGuid.
fn machine_guid() -> String {
    let id = read("/etc/machine-id");
    if id.len() == 32 && id.bytes().all(|c| c.is_ascii_hexdigit()) {
        format!(
            "{}-{}-{}-{}-{}",
            &id[..8],
            &id[8..12],
            &id[12..16],
            &id[16..20],
            &id[20..]
        )
    } else {
        random_uuid()
    }
}

fn os_name() -> String {
    read("/etc/os-release")
        .lines()
        .find_map(|l| l.strip_prefix("PRETTY_NAME="))
        .map(|v| v.trim_matches('"').to_string())
        .unwrap_or_else(|| "Linux".into())
}

fn cpu_name() -> String {
    read("/proc/cpuinfo")
        .lines()
        .find_map(|l| {
            l.strip_prefix("model name")
                .and_then(|r| r.split_once(':'))
                .map(|(_, v)| v.trim().to_string())
        })
        .unwrap_or_else(|| std::env::consts::ARCH.into())
}

fn base_board() -> String {
    let v = [
        read("/sys/class/dmi/id/board_vendor"),
        read("/sys/class/dmi/id/board_name"),
    ]
    .join(" ");
    if v.trim().is_empty() {
        "Unknown".into()
    } else {
        v.trim().to_string()
    }
}

/// MAC of the first physical interface (one backed by a device), uppercase dash form like Windows.
fn mac() -> String {
    let mut names: Vec<String> = std::fs::read_dir("/sys/class/net")
        .map(|d| {
            d.flatten()
                .filter(|e| e.path().join("device").exists())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
        .iter()
        .map(|n| read(&format!("/sys/class/net/{n}/address")))
        .find(|m| !m.is_empty() && m != "00:00:00:00:00:00")
        .map(|m| m.to_uppercase().replace(':', "-"))
        .unwrap_or_default()
}

fn memory_mb() -> u64 {
    read("/proc/meminfo")
        .lines()
        .find_map(|l| l.strip_prefix("MemTotal:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
        .map(|kb| kb / 1024)
        .unwrap_or(0)
}

fn hostname() -> String {
    let h = read("/proc/sys/kernel/hostname");
    if h.is_empty() {
        "Linux".into()
    } else {
        h
    }
}

/// csrf claim inside the JWT access token.
fn csrf_from_token(token: &str) -> Option<String> {
    use base64::Engine;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    serde_json::from_slice::<Value>(&bytes)
        .ok()?
        .get("csrf")?
        .as_str()
        .map(str::to_owned)
}

fn render_qr(content: &str) -> Result<String, String> {
    let code =
        qrcode::QrCode::new(content.as_bytes()).map_err(|e| format!("QR encode failed: {e}"))?;
    Ok(code
        .render::<qrcode::render::unicode::Dense1x2>()
        .quiet_zone(true)
        .dark_color(qrcode::render::unicode::Dense1x2::Light)
        .light_color(qrcode::render::unicode::Dense1x2::Dark)
        .build())
}

/// API code for an expired login QR code.
const QR_EXPIRED: i64 = 1190;

/// QR API errors must not echo server messages/validation fields: they may contain credentials.
fn qr_data<'a>(stage: &str, status: u16, body: &'a Value) -> Result<&'a Value, String> {
    let code = body.get("code").and_then(Value::as_i64);
    if status == 200 && code == Some(0) {
        return Ok(body.get("data").unwrap_or(&Value::Null));
    }
    Err(format!("{stage}: http_status={status} api_code={code:?}"))
}

#[derive(Debug, PartialEq, Eq)]
enum QrPollAction {
    Wait(i64),
    Scanned,
    Confirmed,
    Refresh(Duration),
    Failed(&'static str),
}

/// Preserve the official refresh policy; a failed query says nothing about QR expiry.
fn qr_poll_action(last_status: i64, http_status: u16, body: &Value) -> QrPollAction {
    if body.get("code").and_then(Value::as_i64) == Some(QR_EXPIRED) {
        return QrPollAction::Failed(crate::i18n::s("二维码已失效", "QR code expired"));
    }
    let Ok(data) = qr_data("qrcode status", http_status, body) else {
        return QrPollAction::Failed(crate::i18n::s(
            "二维码状态查询失败",
            "QR status check failed",
        ));
    };
    match data.get("status").and_then(Value::as_i64) {
        Some(1) => QrPollAction::Wait(1),
        Some(2) if last_status != 2 => QrPollAction::Scanned,
        Some(2) => QrPollAction::Wait(2),
        Some(0) => QrPollAction::Refresh(Duration::from_millis(350)),
        Some(3) => QrPollAction::Refresh(Duration::ZERO),
        Some(4) => QrPollAction::Confirmed,
        _ => QrPollAction::Failed(crate::i18n::s(
            "二维码状态响应异常",
            "Unexpected QR status response",
        )),
    }
}

fn private_directory(path: &std::path::Path) -> Result<(), String> {
    use std::os::unix::fs::DirBuilderExt;
    // Permissions apply only to newly created directories, never to a user's existing
    // custom config parent such as HOME or /etc.
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|e| format!("create config directory: {e}"))
}

pub(crate) fn write_config(cfg: &Value) -> Result<std::path::PathBuf, String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let path = crate::config::path();
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    private_directory(dir)?;
    // Exclusive, unpredictable temporary file: never follow a pre-existing .tmp symlink.
    let tmp = dir.join(format!(".config-{}.tmp", random_uuid()));
    let result = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| format!("create private config: {e}"))?;
        let bytes = serde_json::to_vec_pretty(cfg).map_err(|_| "cannot serialize config")?;
        f.write_all(&bytes).map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &path).map_err(|e| format!("replace config: {e}"))?;
        Ok(path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    result
}

/// Only one explicit CLI login may own this account-binding flow.
fn single_instance() -> Result<std::fs::File, String> {
    use std::os::unix::{fs::OpenOptionsExt, io::AsRawFd};
    let path = crate::config::path().with_file_name("login.lock");
    if let Some(dir) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        private_directory(dir)?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|_| "cannot open login lock")?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(crate::i18n::s(
            "另一个登录进程正在运行",
            "Another login is already in progress",
        )
        .into());
    }
    Ok(file)
}

/// The daemon never creates or displays a QR. A separate human-invoked login updates this file.
/// Credentials::load preserves environment precedence: an expired UU_TOKEN supplied by the
/// service environment must be updated there; rewriting the file cannot override it.
pub async fn wait_for_credentials(stale_token: Option<String>) -> Credentials {
    log::warn!("UU login required: run `uuremote-server login` in a terminal; waiting for refreshed credentials");
    loop {
        if let Ok(creds) = crate::config::read_fresh().and_then(|f| Credentials::load(f.as_ref())) {
            if stale_token.as_deref() != Some(creds.token.as_str()) {
                log::info!("UU login credentials refreshed");
                return creds;
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

pub async fn run() -> Result<(), String> {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        return Err(crate::i18n::s("请在交互终端登录（SSH 使用 -t）；二维码不能写入日志", "Log in from an interactive terminal (use ssh -t); QR credentials must not be written to logs").into());
    }
    let _lock = single_instance()?;
    flow().await
}

/// Preserve the installation identity across logins, including pre-existing upstream configs.
pub fn prepare_identity() -> Result<(Value, Credentials), String> {
    let mut cfg = crate::config::read_fresh()?.unwrap_or_else(|| json!({}));
    if !cfg.is_object() {
        return Err("config must be a JSON object".into());
    }
    let get = |cfg: &Value, k: &str| {
        std::env::var(format!("UU_{}", k.to_uppercase()))
            .ok()
            .filter(|v| !v.is_empty())
            .or_else(|| cfg.get(k).and_then(|v| v.as_str()).map(str::to_owned))
            .unwrap_or_default()
    };
    let sign_key = Some(get(&cfg, "sign_key"))
        .filter(|k| !k.is_empty())
        .unwrap_or_else(|| crate::auth::DEFAULT_SIGN_KEY.into());
    for (key, make) in [
        ("client_id", random_uuid as fn() -> String),
        ("system_id", || random_uuid().to_uppercase()),
        ("machine_guid", machine_guid),
    ] {
        if get(&cfg, key).is_empty() {
            cfg[key] = json!(make());
        }
    }
    let creds = Credentials {
        client_id: get(&cfg, "client_id"),
        system_id: get(&cfg, "system_id"),
        device_id: get(&cfg, "device_id"),
        sign_key,
        ..Default::default()
    };
    write_config(&cfg)?;
    Ok((cfg, creds))
}

struct IdentityLock(std::fs::File);
impl IdentityLock {
    fn acquire() -> Result<Self, String> {
        use std::os::unix::{fs::OpenOptionsExt, io::AsRawFd};
        let path = crate::config::path().with_extension("lock");
        if let Some(dir) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            private_directory(dir)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(path)
            .map_err(|_| "Cannot open the identity lock")?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err("Cannot lock the device identity".into());
        }
        Ok(Self(file))
    }
}
impl Drop for IdentityLock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

pub async fn prepare_device() -> Result<(Value, Credentials), String> {
    let _lock = tokio::task::spawn_blocking(IdentityLock::acquire)
        .await
        .map_err(|_| "Identity lock task failed")??;
    let (mut cfg, mut creds) = prepare_identity()?;
    let get = |cfg: &Value, k: &str| {
        std::env::var(format!("UU_{}", k.to_uppercase()))
            .ok()
            .filter(|v| !v.is_empty())
            .or_else(|| cfg.get(k).and_then(|v| v.as_str()).map(str::to_owned))
            .unwrap_or_default()
    };
    if get(&cfg, "device_id").is_empty() {
        // Preserve the official Windows registration shape and upstream's unavailable-screen
        // defaults, without probing display/GPU devices or opening a capture backend.
        let body = json!({
            "name": hostname(), "client_id": creds.client_id, "system_id": creds.system_id, "machine_guid": get(&cfg, "machine_guid"),
            "os": os_name(), "base_board": base_board(), "cpu": cpu_name(), "video": ["Unknown"], "mac": mac(),
            "memory": memory_mb(), "screen": "1920x1080",
        });
        let (status, resp) = UUAuthClient::new(creds.clone())
            .post_json("/api/v1/device/windows/init", &body)
            .await?;
        let d = qr_data("device init", status, &resp)?;
        let id = d
            .get("device_id")
            .and_then(|v| v.as_str())
            .ok_or("device init returned no device_id")?;
        log::info!("Registered local device identity");
        cfg["device_id"] = json!(id);
        write_config(&cfg)?;
    }
    creds.device_id = get(&cfg, "device_id");
    Ok((cfg, creds))
}

async fn flow() -> Result<(), String> {
    let (mut cfg, creds) = prepare_device().await?;
    let client = UUAuthClient::new(creds);
    // Official Mac 4.42 LoginManager: status_query_ticket under token, last status in
    // every long poll; 0/3 regenerate, 4 exchanges the ticket for account credentials.
    let timeout = Duration::from_secs(60);
    loop {
        let (status, resp) = client
            .post_with_timeout("/api/v1/qrcode/gen/login", None, timeout)
            .await
            .map_err(|e| format!("QR generation failed: {e}"))?;
        let d = qr_data("qrcode gen", status, &resp)?;
        let url = d
            .get("qrcode_jump_url")
            .and_then(Value::as_str)
            .ok_or("qrcode gen returned no QR content")?;
        let ticket = qr_ticket(d).ok_or("qrcode gen returned no status ticket")?;
        println!(
            "{}\n{}",
            crate::i18n::s(
                "请用手机网易 UU 远程 App 扫码登录",
                "Scan with the UU Remote mobile app"
            ),
            render_qr(url)?
        );
        let mut login_status = 1;
        loop {
            let query =
                json!({"token": ticket, "qrcode_jump_url": url, "login_status": login_status});
            let (status, reply) = client
                .post_with_timeout("/api/v1/qrcode/login/status", Some(&query), timeout)
                .await
                .map_err(|e| match e.kind {
                    QrRequestErrorKind::Timeout => crate::i18n::s(
                        "查询超时，不代表二维码已失效；请重新运行 login",
                        "Status query timed out; this does not imply QR expiry. Run login again",
                    )
                    .to_owned(),
                    _ => format!("QR status request failed: {e}"),
                })?;
            match qr_poll_action(login_status, status, &reply) {
                QrPollAction::Wait(next) => login_status = next,
                QrPollAction::Scanned => {
                    login_status = 2;
                    println!(
                        "{}",
                        crate::i18n::s(
                            "已扫码，请在手机上确认登录",
                            "Scanned; confirm sign-in on your phone"
                        )
                    );
                }
                QrPollAction::Refresh(delay) => {
                    tokio::time::sleep(delay).await;
                    break;
                }
                QrPollAction::Confirmed => {
                    let body = json!({"token": ticket, "qrcode_jump_url": url});
                    let (status, reply) = client
                        .post_with_timeout("/api/v1/login/by_qrcode", Some(&body), timeout)
                        .await
                        .map_err(|e| format!("QR login exchange failed: {e}"))?;
                    let d = qr_data("qrcode login", status, &reply)?;
                    let token = d
                        .get("token")
                        .and_then(Value::as_str)
                        .ok_or("login returned no token")?;
                    let user = d
                        .get("user_id")
                        .and_then(Value::as_str)
                        .ok_or("login returned no user_id")?;
                    cfg["token"] = json!(token);
                    cfg["user_id"] = json!(user);
                    cfg["csrf"] =
                        json!(csrf_from_token(token).ok_or("token carries no csrf claim")?);
                    if let Some(nickname) = d
                        .get("nickname")
                        .and_then(Value::as_str)
                        .filter(|n| !n.is_empty())
                    {
                        cfg["account_name"] = json!(nickname);
                    }
                    if let Ok(Some(Value::Object(fresh))) = crate::config::read_fresh() {
                        for (k, v) in fresh {
                            if cfg.get(&k).is_none() {
                                cfg[k] = v;
                            }
                        }
                    }
                    write_config(&cfg)?;
                    println!(
                        "{}",
                        crate::i18n::s(
                            "登录成功，凭据已保存在本机",
                            "Signed in; credentials saved locally"
                        )
                    );
                    return Ok(());
                }
                QrPollAction::Failed(reason) => {
                    return Err(crate::tr!(
                        "{reason}；请重新运行 login",
                        "{reason}; run login again"
                    ))
                }
            }
        }
    }
}

fn qr_ticket(gen: &Value) -> Option<&str> {
    gen.get("status_query_ticket")
        .or_else(|| gen.get("token"))
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn qr_poll_keeps_waiting_scanned_and_confirmed_distinct() {
        let reply = |status| json!({"code": 0, "data": {"status": status}});
        assert_eq!(qr_poll_action(1, 200, &reply(1)), QrPollAction::Wait(1));
        assert_eq!(qr_poll_action(1, 200, &reply(2)), QrPollAction::Scanned);
        assert_eq!(qr_poll_action(2, 200, &reply(2)), QrPollAction::Wait(2));
        assert_eq!(qr_poll_action(2, 200, &reply(4)), QrPollAction::Confirmed);
        assert_eq!(
            qr_poll_action(1, 200, &reply(0)),
            QrPollAction::Refresh(Duration::from_millis(350))
        );
        assert_eq!(
            qr_poll_action(2, 200, &reply(3)),
            QrPollAction::Refresh(Duration::ZERO)
        );
        assert_eq!(
            qr_poll_action(2, 400, &json!({"code": 1190})),
            QrPollAction::Failed(crate::i18n::s("二维码已失效", "QR code expired"))
        );
    }

    #[test]
    fn qr_poll_failures_do_not_imply_expiry_or_confirmation() {
        for last in [1, 2] {
            for body in [
                json!(null),
                json!({"code": 1}),
                json!({"code": 0, "data": {"status": 4}}),
            ] {
                assert_eq!(
                    qr_poll_action(last, 503, &body),
                    QrPollAction::Failed(crate::i18n::s(
                        "二维码状态查询失败",
                        "QR status check failed"
                    ))
                );
            }
            assert_eq!(
                qr_poll_action(last, 200, &json!({"code": 1})),
                QrPollAction::Failed(crate::i18n::s(
                    "二维码状态查询失败",
                    "QR status check failed"
                ))
            );
            for data in [
                json!(null),
                json!({}),
                json!({"status": "4"}),
                json!({"status": 99}),
                json!({"status": -1}),
            ] {
                assert_eq!(
                    qr_poll_action(last, 200, &json!({"code": 0, "data": data})),
                    QrPollAction::Failed(crate::i18n::s(
                        "二维码状态响应异常",
                        "Unexpected QR status response"
                    ))
                );
            }
        }
    }

    #[test]
    fn qr_api_errors_do_not_echo_server_content() {
        let body =
            json!({"code": 1001, "msg": "fixture-token", "data": [{"loc": "fixture-ticket"}]});
        assert_eq!(
            qr_data("qrcode gen", 400, &body).unwrap_err(),
            "qrcode gen: http_status=400 api_code=Some(1001)"
        );
    }

    #[test]
    fn status_ticket_prefers_status_query_ticket() {
        assert_eq!(
            qr_ticket(&json!({"token": "old", "status_query_ticket": "new"})),
            Some("new")
        );
        assert_eq!(qr_ticket(&json!({"token": "old"})), Some("old"));
        assert_eq!(qr_ticket(&json!({"status_query_ticket": ""})), None);
    }
    #[test]
    fn service_waits_until_a_new_token_is_saved() {
        let _env = crate::config::test_env_lock().lock().unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
        let dir = std::env::temp_dir().join(format!("uu-login-wait-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let creds = |token: &str| json!({"user_id": "u", "device_id": "d", "client_id": "c", "system_id": "s", "token": token, "csrf": "x"});
        std::fs::write(&path, creds("old").to_string()).unwrap();
        let previous = std::env::var_os("UU_CONFIG");
        std::env::set_var("UU_CONFIG", &path);
        let writer = path.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(2500)).await;
            std::fs::write(&writer, creds("new").to_string()).unwrap();
        });
        let got = tokio::time::timeout(Duration::from_secs(10), wait_for_credentials(Some("old".into()))).await.unwrap();
        assert_eq!(got.token, "new"); // the expired token in the file is not accepted
        if let Some(previous) = previous { std::env::set_var("UU_CONFIG", previous); } else { std::env::remove_var("UU_CONFIG"); }
        let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn csrf_is_read_from_jwt_payload() {
        use base64::Engine;
        let payload =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"sub":"u","csrf":"c-1"}"#);
        assert_eq!(
            csrf_from_token(&format!("h.{payload}.s")),
            Some("c-1".into())
        );
        assert_eq!(csrf_from_token("bad"), None);
    }
    #[test]
    fn generated_ids_are_uuid_shaped() {
        let u = random_uuid();
        assert_eq!(u.len(), 36);
        assert_eq!(u.as_bytes()[14], b'4');
        assert!(render_qr("https://example.com/x").unwrap().lines().count() > 10);
    }

    #[test]
    fn private_atomic_config_replacement_preserves_identity_and_reads_fresh_changes() {
        use std::os::unix::fs::PermissionsExt;
        let _env = crate::config::test_env_lock().lock().unwrap();
        let dir = std::env::temp_dir().join(format!(
            "uu-private-config-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let path = dir.join("config.json");
        let keys = [
            "UU_CONFIG",
            "UU_CLIENT_ID",
            "UU_SYSTEM_ID",
            "UU_MACHINE_GUID",
            "UU_DEVICE_ID",
            "UU_SIGN_KEY",
        ];
        let saved: Vec<_> = keys
            .iter()
            .map(|key| (*key, std::env::var_os(key)))
            .collect();
        for key in keys {
            std::env::remove_var(key);
        }
        std::env::set_var("UU_CONFIG", &path);
        let initial = json!({"client_id":"fixture-client", "system_id":"fixture-system", "machine_guid":"fixture-machine", "device_id":"fixture-device", "token":"fixture-old"});
        write_config(&initial).unwrap();
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let mut revised = initial.clone();
        revised["token"] = json!("fixture-new");
        write_config(&revised).unwrap();
        let (cfg, identity) = prepare_identity().unwrap();
        assert_eq!(identity.device_id, "fixture-device");
        assert_eq!(identity.client_id, "fixture-client");
        assert_eq!(cfg, revised);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        for (key, value) in saved {
            if let Some(value) = value {
                std::env::set_var(key, value);
            } else {
                std::env::remove_var(key);
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
