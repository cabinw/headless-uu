# 设备卡片封面 / Device-card cover

`set-wallpaper [--] <image>` 是显式、一次性的云操作：上传指定本地图片并更新当前登录身份的设备卡片封面。用法见 [中文 README](../README.md#设置设备卡片封面) / [English README](../README.en.md#set-a-device-card-cover)。

`set-wallpaper [--] <image>` explicitly uploads one local image and updates the current signed-in identity's device-card cover. It is independent of the persistent host.

## 无头边界 / Headless boundary

- 接受本地 PNG、JPEG、WebP 文件；相对路径基于当前工作目录，含空格时加引号，`--` 可分隔以 `-` 开头的文件名。
- 使用当前机器已有的完整登录身份：`UU_CONFIG` 选择配置路径，身份环境变量优先于文件。凭据缺失或过期时失败，由用户显式运行 `login`；不自动注册设备或启动扫码。
- 不发现系统壁纸，不访问桌面会话，不截屏、不轮询，不引入图形依赖，不改变 `run`、服务或端口映射的生命周期。
- 原样上传选定图片，不缩放、不转换、不清除内嵌元数据。本地上限为 10 MiB（10,485,760 字节），还需满足服务端的 `max_size`。
- 命令成功只说明云 API 接受更新。官方客户端的刷新时机、缓存、裁剪及封面清除尚未验证；没有清除或恢复默认的命令。

The command accepts local PNG/JPEG/WebP paths and requires a complete existing identity. `UU_CONFIG` selects the configuration; identity environment variables override it. Missing or expired credentials require explicit login. No desktop discovery, screenshot capture, polling, new graphical dependency or daemon integration is included. Image bytes and embedded metadata are uploaded without resizing or conversion, subject to both the local 10 MiB cap and the server's `max_size`. API success does not establish client refresh, cropping, caching or reset behavior.

## 协议依据 / Protocol evidence

来源为上游 [PR #153](https://github.com/VallierDev/uu-remote-linux/pull/153)，以及固定基线 `fe052cdca7ad90d1ab7c27d031036b1071869070` 的[协议记录](https://github.com/VallierDev/uu-remote-linux/blob/fe052cdca7ad90d1ab7c27d031036b1071869070/docs/wallpaper-protocol.md)和[实现](https://github.com/VallierDev/uu-remote-linux/blob/fe052cdca7ad90d1ab7c27d031036b1071869070/src/wallpaper.rs)。本次只沿用上传与报告的协议，不恢复该历史版本的桌面发现和后台定时任务。

Evidence comes from the upstream PR and pinned source above. The upstream record describes static analysis of the official Windows host, including `ApplicationManager::updateWallpaper`, the file-token parser, raw upload and wallpaper report. That evidence establishes request shapes; it does not prove every official client's display behavior. This project retains the upload/report flow only, excluding upstream desktop discovery and background polling.

| 步骤 / Step | 请求与字段 / Request and fields |
|---|---|
| 取得文件令牌 / Obtain file token | Signed `GET /api/v1/tool/fp/token?filetype=1`; API data contains `token`, `req_url`, `ttl` and optional `max_size`. |
| 上传图片 / Upload image | `POST` raw image bytes to `req_url`, with the file token directly in `Authorization` and the image MIME type in `Content-Type`. No Bearer prefix, multipart wrapper or account API headers. |
| 读取结果 / Read upload result | HTTP 200 response contains a top-level `url`. The upstream parser also records `fsize`, `md5` and `mime`; the cover report uses `url`. |
| 更新设备 / Update device | Signed `POST /api/v1/device/wallpaper` with JSON `{"wallpaper_url":"<uploaded URL>"}`, using the current identity's account headers. |

上传地址与结果 URL 必须为 HTTPS；不得把账号认证头发送给文件服务，也不得通过重定向转发凭据。`ttl` 是服务返回的文件令牌元数据，不构成后台刷新任务的依据。10 MiB 是本项目的本地限制，不是已证实的官方服务统一限额。

Upload and returned URLs must use HTTPS. Keep account authentication separate from the file token and do not forward credentials through redirects. The returned `ttl` does not imply a background token-renewal task. The 10 MiB cap is a project limit, not a claimed universal UU service limit.

## 隐私与失败 / Privacy and failures

执行命令即明确请求把所选图片内容交给 UU 文件服务用于设备卡片展示；图片可能包含内嵌元数据。不要上传不愿离开机器的内容。日志和报告不得包含图片内容、本地路径、图片哈希、账号凭据、文件令牌、返回 URL 或 API 响应正文；使用阶段名、大小或布尔状态诊断。

Executing the command requests cloud upload of the selected file, including embedded metadata. Logs and reports must omit image contents, local paths, image hashes, account credentials, file tokens, returned URLs and response bodies. Diagnose using stage names, sizes and booleans.

如果上传成功而最后的设备更新失败，不能据此声称已撤销上传；当前没有已验证的云端文件删除或封面清除流程。命令失败不自动重试、不启动登录。先处理报告的文件、凭据或网络问题，再显式重试。

If upload succeeds but the final device update fails, do not claim the upload was rolled back. No cloud-file deletion or cover-reset flow has been verified. Failure does not automatically retry or start login; resolve the reported problem before explicitly retrying.

| 退出码 / Exit code | 含义 / Meaning |
|---|---|
| `0` | 更新被 API 接受，或显示 `--help` / `-h` / API accepted the update, or help was printed. |
| `1` | 文件、凭据、网络或 API 错误 / File, credential, network or API error. |
| `2` | 参数数量、未知选项等语法错误 / Command syntax error, such as argument count or unknown option. |

## 验证范围 / Validation scope

所有自动化测试使用合成身份和本地夹具，不读取真实配置、不请求真实 UU 服务、不上传用户图片。

Automated tests must use synthetic identities and local fixtures, without reading real configuration, contacting live UU services or uploading user images.

| 范围 / Area | 需要验证 / Required checks |
|---|---|
| CLI | Relative/absolute paths, quoted spaces, `--`, `--help`/`-h`, missing/extra arguments, unknown options and exit codes. |
| 文件 / Files | Supported signatures, unsupported or truncated input, missing paths, directories, non-regular files and bounded reads at/over 10 MiB. |
| 身份 / Identity | Missing/incomplete credentials fail without login or registration; current configuration and environment precedence are preserved. |
| 云流程 / Cloud flow | Signed token request, separate raw-upload token and headers, MIME type, exact wallpaper JSON, successful stage ordering and server `max_size`. |
| 错误边界 / Failures | Invalid responses, non-HTTPS URLs, redirects, timeouts and failures at each stage stop the sequence; diagnostics contain no secrets. |
| 无头回归 / Headless regression | Existing daemon/port-mapping tests remain valid; build introduces no graphical linkage or automatic wallpaper task. |

### 当前结果 / Current results

2026-11-11 完成以下验证；两端的 `src/` 和 `tests/` 按文件内容核对一致。

The following checks passed on 2026-11-11 against matching source and test files on both hosts.

| 环境 / Environment | 命令与结果 / Commands and results |
|---|---|
| macOS，Rust 1.97.1 | `cargo test --locked`: 80 unit + 4 CLI + 2 wallpaper CLI tests passed; `cargo build --locked --bin uuremote-server` passed. |
| Ubuntu 26.04.1 ThinkPad，x86_64，Rust 1.93.1 | Same full suite: 80 + 4 + 2 passed, including Linux non-UTF-8 filenames; the debug binary build passed. |
| Linux 动态链接 / Dynamic linkage | `readelf -d` lists only `libgcc_s`, `libm`, `libc` and the dynamic loader; no graphical, media or PAM libraries. |
| 格式与文档 / Format and documentation | `cargo fmt --all -- --check`, `git diff --check`, YAML parsing and relative documentation links passed. |
| 本地打包 / Local packaging | Binary-tar smoke using the macOS debug input passed: new cover documentation, relative links, notices, executable modes and atomic installation. This is not release/arm64 or remote CI acceptance. |

新增的真实进程测试使用临时账号配置、合成 CA 和仅终止于回环的 CONNECT 服务，执行真实 `set-wallpaper` 二进制入口。验证三步 TLS 上传、原始图片字节、文件令牌与账号凭据隔离、相对路径与 `--`、带空格路径、宿主锁已占用时可独立运行、配置字节和目录不变，以及 401 失败不继续上传。另有 9 项专用单元测试覆盖签名、格式识别、大小边界、响应限制、三阶段失败及超时；原有端口映射回归也完整保留。

The real-process tests execute the CLI using temporary synthetic credentials and a test CA through a non-forwarding loopback CONNECT server. They cover the complete TLS flow, byte integrity, credential separation, relative/escaped/spaced paths, an occupied host lock, unchanged configuration and failure before upload on HTTP 401. Nine dedicated unit tests cover request signatures, format detection, size boundaries, bounded responses, stage failures and timeouts; all existing mapping regressions remain in the suite.

本次没有读取真实账号配置、向 UU 上传真实图片、验证官方客户端显示、运行 release/容器构建或部署服务。绿云生产服务未改动。

No real account configuration was read, no image was uploaded to the live UU service, no official-client display acceptance or release/container build was performed, and no service was deployed. Greencloud production remains unchanged.
