# Linux Docker 部署 / Linux Docker deployment

从当前仓库源码构建专用无头程序。没有桌面库，也不会从上游下载旧版完整桌面二进制。此目录不代表已经发布了容器镜像。

Build the dedicated headless program from this checkout. No desktop libraries or downloaded upstream desktop executable are used. These files do not imply that a container image has been published.

## 构建与登录 / Build and login

在仓库根目录执行。以下 Compose 配置使用 **Linux host networking**，因此映射目标 `127.0.0.1:22` 是 Docker 宿主的 sshd。Docker Desktop 的宿主网络语义不同，不属于本部署示例的验证范围。

Run from the repository root. The Compose example uses **Linux host networking**: a target of `127.0.0.1:22` reaches the Linux Docker host's sshd. Docker Desktop networking differs and is outside this example's validation scope.

```sh
mkdir -p docker/config
sudo chown 10001:10001 docker/config
sudo chmod 700 docker/config
docker compose -f docker/compose.yaml build
docker compose -f docker/compose.yaml run --rm headless-uu login
# 手机 UU 远程扫描终端二维码并确认后 / After scanning and approving the terminal QR:
docker compose -f docker/compose.yaml up -d
```

二维码只显示给账号所有者，不要保存到报告或公开日志。每台服务器独立登录；不要复制其他机器的 `config.json`。容器以 UID/GID 10001 运行，只有 `/config` 持久卷保存身份和凭据；配置文件应为 0600。修改 UID/GID 时同步调整卷所有权。

Show the QR only to the account owner and exclude it from reports or public logs. Log in independently on each host; do not clone another machine's `config.json`. The container runs as UID/GID 10001. Only `/config` persists identity and credentials, and the config file should remain 0600. Keep volume ownership consistent if changing UID/GID.

## 验证与停用 / Verify and stop

```sh
docker compose -f docker/compose.yaml exec headless-uu uuremote-server doctor
docker compose -f docker/compose.yaml logs --tail=100 headless-uu
docker compose -f docker/compose.yaml down
```

`doctor` 或容器存活不证明云端连通。确认 ready 状态、实际 `ICE path`、官方客户端映射和通过该映射的真实 SSH 请求；继续使用原 SSH 密钥和严格主机指纹验证。`down` 停止容器但保留绑定的配置目录。

Neither `doctor` nor a running container proves connectivity. Verify ready status, the selected `ICE path`, the official client's mapping, and an actual SSH request over that mapping. Preserve original SSH authentication and strict host-key checking. `down` stops the container while retaining the bound configuration directory.

没有内置终端、文件管理或 `/data` 共享目录。SFTP 通过宿主 sshd 提供。本配置不打开宿主公网 SSH 或修改防火墙。

There is no built-in terminal, file manager or `/data` share. SFTP is provided by the existing host sshd. This configuration does not expose new public SSH ports or alter the firewall.

自动化构建与本地 TCP/SCTP 测试不能替代此容器的真实 UU 登录及官方客户端验收。发布修改后的镜像时，同时提供对应源码、构建说明及许可证。

Automated builds and local TCP/SCTP tests do not replace actual UU login and official-client acceptance of the container. Distribute corresponding source, build instructions and licenses with modified images.
