# headless 专用重构记录

日期：2026-11-11。基线：`fe052cdca7ad90d1ab7c27d031036b1071869070`，包含此前阶段 A 的端口映射生命周期修复。Cargo 包改为 `cabinw-uu-headless 0.3.0`，可执行文件保留 `uuremote-server`。

## 范围与结构

现在只有无头宿主这一种产品形态，不再通过环境变量选择桌面后端。入口支持 `run`、`login`、`doctor`、`version`、`help`；旧桌面命令返回错误，不会意外启动宿主。

| 目录 | 职责 |
|---|---|
| `src/cloud/` | 本机身份、凭据、二维码登录、签名 REST、Socket.IO/Engine.IO、WebSocket |
| `src/transport/` | WebRTC peer、ICE/TURN、TCP 映射、背压与取消、无地址诊断 |
| `src/wire/` | protobuf 编解码、官方工具必要的初始化及兼容应答 |
| `src/runtime/` | 宿主生命周期、配置路径进程锁、国际化 |
| `tests/cli.rs` | 独立子进程验证 CLI、SIGTERM、重复实例和二维码输出限制 |
| `packaging/`、`docker/` | 独立无头程序的安装脚本、systemd 示例和源码构建容器 |

删除 C 引擎和 `build.rs`、GUI 与原生 viewer、桌面捕获和输入、剪贴板、音视频业务、硬件编解码、隐私屏、PAM/电源操作、远程协助、内置终端/文件管理，以及对应脚本、资源、测试和过时文档。第三方传输补丁、许可证和上游署名保留。依赖锁只清理不再使用的包，没有顺带升级保留依赖。

仍保留名义屏幕初始化、CONTROL/TEXT、FILE 数据通道、一个不发送媒体的兼容视频轨及 SDP 元数据；这些用于维持已有官方工具协议形状。不会启动桌面、采集或编码任务。媒体协商进一步裁剪需要另行做官方客户端验收。

## 运行和关闭行为

- 仅接受 `capture_type=9` 的端口映射 offer；新连接只替换相同 ICE 身份的 peer。
- TCP 建连前登记受管任务；远端 FIN、通道关闭、peer 失败/关闭和宿主退出会取消建连、读写和排队发送。旧 generation 的清理不能删除复用身份的新流。目标 EOF/拒连后，排队 DATA/FIN/失败 ACK 仍可由 FIN 或同身份新 SYN 取消；弱令牌按生命周期清理，防止持续积累。
- 本地队列拥塞先关闭 TCP，再按序等待 FIN；收到远端 FIN 会使应用队列中旧流帧失效。出站期限覆盖通道等待、背压和实际发送。
- 云信令重连保留仍连通的映射；废弃房间关闭事件接收端后，其 WebSocket、心跳和写任务会退出。维护循环回收失败 peer；退出时并发执行有期限的 peer 关闭。
- 缺失或过期凭据只等待显式 CLI 登录。新凭据与实际失败的 token 比较，避免错过已刷新的文件。`UU_TOKEN` 等环境变量仍优先于文件。
- 同一配置路径只允许一个常驻宿主；登录使用独立锁。SIGTERM/SIGINT 能打断凭据和网络等待。二维码只允许在交互 TTY 输出；配置原子保存并保持权限限制。

## 验证

验证使用隔离的空配置或合成凭据、回环 TCP/WebSocket 和本地 SCTP，不读取或复制真实账号配置。ThinkPad 使用独立测试目录，未安装或启动 UU 常驻服务。

| 检查 | 结果 |
|---|---|
| macOS `cargo test --locked` | 70 项单元测试、2 项进程测试通过 |
| macOS `cargo build --locked --bin uuremote-server` | 通过 |
| Ubuntu ThinkPad 完整测试 | 70 项单元测试、2 项进程测试通过（Ubuntu 26.04.1，x86_64，Rust 1.93.1） |
| Ubuntu ThinkPad 构建 | `cargo build --locked --bin uuremote-server` 通过 |
| Linux 1000 次 TCP 流重连资源检查 | 通过；FD 数 10 → 10，线程数 2 → 2 |
| Linux 动态链接检查 | 只链接 `libgcc_s`、`libm`、`libc` 和动态加载器；没有图形、音视频或 PAM 库 |
| 安装脚本 | 语法、含空格路径、权限、原子替换、非法参数检查通过 |
| 二进制 tar 打包 smoke | 使用 macOS debug 输入通过；校验和、README 相对链接、许可证、执行权限、实际临时安装通过。未验证尚未提交的新源码归档 |
| systemd 示例 | ThinkPad 上使用测试二进制路径进行 `systemd-analyze verify`，通过；未安装/启动服务 |
| 仓库格式 | `cargo fmt --all -- --check`、`git diff --check` 通过 |

macOS 构建使用 Rust 1.97.1；Linux 使用 Rust 1.93.1。动态链接检查针对本次实际生成的 debug 二进制；release 工作流也配置了相同检查，但本次未运行 release 构建。

本地完整验证日志保存在维护者工作区的 `Documents/headless-uu/headless-refactor-validation-2026-11-11/`。测试完成后按文件内容核对，两端 `src/` 和 `tests/` 一致。

核心回归覆盖 SYN/DATA/FIN 顺序和原始 wire 类型、建连期间取消、重复 SYN、身份复用、阻塞目标写入、队列满时关闭、低水位恢复、SCTP 丢包恢复及 6 MiB 数据完整性、192 KiB 帧、完成流排队帧失效及 1000 个不同身份的令牌回收、独立 peer 隔离、WebSocket 心跳/超时/代理路径、凭据保存和最小协议应答。1000 次重连是同一映射会话内的 TCP 流重建，不是 1000 次官方客户端或 ICE 重连。

可复现命令：

```sh
cargo test --locked
cargo build --locked --bin uuremote-server
cargo fmt --all -- --check
# 以下两项在 Linux 执行：
UU_RECONNECT_RESOURCE_CHECK=1 cargo test --locked \
  transport::port_mapping::lifecycle_tests::thousand_reconnects_release_tasks_and_stream_registrations \
  -- --exact --nocapture
readelf -d target/debug/uuremote-server
```

## 验证边界

本次未执行真实账号登录、官方 Mac/Windows 客户端连接、真实 TURN relay、通过 UU 的 SSH/SFTP、长期运行、容器镜像构建或 arm64 构建；GitHub 工作流尚未在远端运行。不能据本地自动化测试宣称完整官方客户端兼容或生产就绪，也没有进行性能提升对比。

绿云生产服务、二进制、配置及账号身份均未改动。当前结果用于下一步本地官方客户端验收；生产发布另行安排。
