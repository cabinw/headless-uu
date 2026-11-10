# headless-uu

**简体中文** · [English](README.en.md)

为 Linux 服务器提供常驻的 UU 端口映射宿主，让官方 UU 客户端的本地映射端口承载标准 SSH、SFTP 和其他 TCP 服务。

项目派生自 [VallierDev/uu-remote-linux](https://github.com/VallierDev/uu-remote-linux)，现由 [cabinw/headless-uu](https://github.com/cabinw/headless-uu) 维护。与网易无关，不包含网易客户端二进制或品牌素材。许可证为 GPL-3.0-or-later。

## 工作方式

```text
OpenSSH / SFTP / VS Code Remote-SSH
  → 官方 UU 客户端的本地回环映射端口
  → UU 云信令 + ICE/TURN + DTLS/SCTP
  → headless-uu
  → 服务器原有 sshd 或其他 TCP 服务
```

UU 负责建立通道，原有 sshd 继续验证 SSH 用户和密钥，客户端继续验证原主机指纹。实际数据路径由 UU 云策略和 ICE 协商决定，可能是直连，也可能是 relay；设备在线或取得 TURN 参数不代表已经走中继。

只保留：

- 设备身份、终端二维码登录和本机凭据保存。
- 云信令、心跳、重连和会话管理。
- 官方端口映射协议及其必要的 CONTROL/TEXT 初始化。
- 多 TCP 流转发、背压、取消、关闭和诊断日志。

已移除远程桌面、键鼠、剪贴板、音视频采集与编解码、隐私屏、PAM 解锁、GUI、原生控制端、远程协助，以及 UU 内置终端和文件管理。**SSH/SFTP 通过端口映射使用系统服务。** 部分媒体协商元数据仍为官方协议兼容保留，不会采集桌面或生成音视频。

## 构建

需要 Rust stable、C 编译器和系统 CA 证书；部分 Rust 加密依赖仍需 C 编译器。项目本身不再编译 C 引擎，也不依赖 GTK、X11、Wayland、FFmpeg、SDL 或桌面会话。

Ubuntu/Debian 示例：

```sh
sudo apt-get install --no-install-recommends build-essential ca-certificates
# 按 Rust 官方方式安装 stable 工具链后：
git clone https://github.com/cabinw/headless-uu.git
cd headless-uu
cargo test --locked
cargo build --release --locked --bin uuremote-server
./target/release/uuremote-server help
```

Cargo 包名为 `cabinw-uu-headless`。可执行文件保留 `uuremote-server` 名称，便于迁移现有服务。macOS 可用于开发和本地通信测试；以下常驻部署说明针对 Linux/systemd。

## 登录与运行

每台机器单独登录，**不要复制另一台机器的配置、token 或设备身份**。

```sh
./target/release/uuremote-server login
./target/release/uuremote-server run
```

用手机官方 UU 远程 App 扫描终端二维码并确认。二维码是临时凭据，只向账号所有者显示，不要收入日志、截图报告或 Git。`login --no-gui` 作为兼容写法继续接受；项目没有 GUI 登录路径。

`login` 要求标准输出连接到交互终端（TTY），拒绝重定向或管道输出，避免二维码进入日志。通过 SSH 登录时分配终端，例如 `ssh -t your-server 'uuremote-server login'`；专用服务用户也要在交互终端中执行下面的 `sudo -u cabinw ... login` 命令。

默认配置仍为 `~/.config/uuremote/config.json`，`UU_CONFIG` 可以指定其他路径。同一台机器迁移时可继续使用其原配置，保持文件 `0600`、目录 `0700`。无参数启动等同于 `run`。服务端没有登录凭据时，先显式执行 `login`；不要在无人值守日志中等待扫码。

宿主对配置路径持有进程锁；第二个使用同一配置路径的宿主会退出，避免重复上线。交互登录使用独立锁，因此运行中的宿主可以等待另一终端完成登录并更新配置。锁文件位于配置旁，进程退出时自动释放锁，无需手动删除文件。

身份环境变量优先于配置文件。如果服务环境里的 `UU_TOKEN` 已过期，仅重新扫码写入配置无法覆盖它：需移除或更新相应服务环境设置并重启宿主。不要把 token 写进命令历史、日志或问题反馈。

| 命令 | 用途 |
|---|---|
| `login [--no-gui]` | 在终端完成二维码登录 |
| `run` 或无参数 | 启动常驻宿主 |
| `doctor` | 输出不含凭据的本地诊断；不代表云端或 SSH 已连通 |
| `version` / `--version` | 输出版本 |
| `help` / `--help` | 输出用法 |

## Linux 常驻服务

建议使用普通专用账号。下面是新部署示例；如果已经有同名用户或 UU 服务，先核实身份与路径，避免新增重复实例。安装过程不需要、更不应修改 sshd 的现有认证方式。

```sh
sudo useradd --system --create-home --home-dir /var/lib/cabinw-uu \
  --shell /usr/sbin/nologin cabinw
sudo install -m 755 target/release/uuremote-server /usr/local/bin/uuremote-server
sudo install -d -m 700 -o cabinw -g cabinw /var/lib/cabinw-uu
sudo -u cabinw env UU_CONFIG=/var/lib/cabinw-uu/config.json \
  /usr/local/bin/uuremote-server login
sudo install -m 644 packaging/systemd/cabinw-uuremote.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now cabinw-uuremote.service
```

示例 unit 使用 `User=cabinw`、`Restart=always`、`WantedBy=multi-user.target`，不依赖图形登录或用户 linger。通过日志中的 ready 状态确认云端上线，再进行实际映射与 SSH 验收：

```sh
systemctl status cabinw-uuremote.service
journalctl -u cabinw-uuremote.service --since '10 minutes ago' --no-pager
```

升级时先验证新程序，把文件复制到临时名称后 `mv` 替换，再重启服务；不要覆盖正在执行的文件。保留旧程序和 unit 以便回滚。重启会中断已有映射连接。不要把新部署示例直接套到生产系统。

[Docker 部署](docker/README.md) 从本仓库源码构建，同样不需要桌面库。

## 官方客户端与 SSH

在官方客户端为对应设备创建端口映射，例如：

| 字段 | 示例 |
|---|---|
| 本地监听 | `127.0.0.1:22314`，选未占用端口 |
| 目标地址 | `127.0.0.1` |
| 目标端口 | 服务器实际 SSH 端口，例如 `22` |

确认客户端只监听本地回环地址。目标必须是宿主可访问的服务；本项目沿用上游按请求转发 `target_host:target_port` 的行为，**目前没有目标白名单**。

为映射建立独立 SSH 别名，保留原有管理通道。复用已有用户、认证方式和已验证的主机指纹，设置 `HostKeyAlias` 为原可信主机记录，而不是接受 `localhost` 上的新指纹。不要使用 `StrictHostKeyChecking=no`。原 SSH 使用跳板或代理时，应在映射别名中明确 `ProxyJump none` 和 `ProxyCommand none`，避免重复转发。

判断中继使用当前连接的 `ICE path: ... route=relay` 日志。`route=direct` 就是直连。`UU_USE_PROXY=1` 不是强制中继开关，也不能保证更低延迟。端口映射涉及账号、客户端版本和云策略兼容性，应在自己的环境端到端验证。

## 配置

| 环境变量 | 含义 |
|---|---|
| `UU_CONFIG` | 本机配置文件路径，默认 `~/.config/uuremote/config.json` |
| `UU_LANG=zh` / `en` | 命令行界面语言；默认按区域设置选择 |
| `UU_PORT_MAPPING=0` | 禁用端口映射目标连接 |
| `UU_USE_PROXY=1` | 允许 REST/WebSocket 和 TURN TCP/TLS 使用现有代理环境配置；默认不使用 |
| `RUST_LOG` | 日志级别，通常使用 `info` |

项目始终无头运行，无需 `UU_DESKTOP_BACKEND=none`、`UU_AUDIO=0` 或 `UU_LOGIN_WINDOW=0`。保留旧环境变量不会恢复已删除的桌面功能。

## 代码结构与验证

代码按职责分为 `src/cloud/`（账号与云协议）、`src/transport/`（WebRTC 与 TCP 映射）、`src/wire/`（最小兼容消息）和 `src/runtime/`（进程生命周期与命令行辅助）。`vendor/` 保留传输兼容补丁及其许可证。

```sh
cargo test --locked
cargo build --locked --bin uuremote-server
readelf -d target/debug/uuremote-server | grep NEEDED
```

CI 执行 Linux/macOS 测试，并检查 Linux 产物不链接图形或音视频库。打包工作流只生成可下载的构建产物，不自动发布 Release 或容器镜像。

本次结构调整、测试结果和验收边界见[重构记录](docs/headless-refactor.md)。

本地 TCP、真实 SCTP、队列关闭、丢包恢复和重连回归用于自动化验证。它们不能替代官方 UU 客户端、真实 relay、SSH/SFTP 或长期运行验收。原项目以及[阶段 A 记录](docs/headless-phase-a.md)的通过项是对应历史版本的证据，不代表本次完整重构已完成官方客户端验收。

## 来源与许可

派生基线：上游 `fe052cdca7ad90d1ab7c27d031036b1071869070`，接近 v0.2.57。保留原作者署名、[GPL 许可证](LICENSE)、vendored 组件的独立许可证与 `PATCHES.md`。分发修改后的二进制时应同时提供对应源码和构建说明。协议主干继续参考上游，避免通过自定义协议破坏官方客户端兼容性。
