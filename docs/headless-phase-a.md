# cabinw/headless-uu：阶段 A 研发与验证

> 历史记录：本文描述完整重构前的阶段 A 快照。当前结构与验收结果见 [headless 专用重构记录](headless-refactor.md)，下文旧路径、测试数量和保留功能不代表当前版本。

日期：2026-11-11。起点：`fe052cdca7ad90d1ab7c27d031036b1071869070`（上游 v0.2.57 后的 WOL 提交）。本阶段只修改无头运行路径和端口映射生命周期，不变更登录、云信令、ICE/TURN 策略、媒体协商或 vendored 协议补丁。

## 实现

### 无头桌面任务

`UU_DESKTOP_BACKEND=none` 时：

- 不创建全局 `uu-lock-state` 查询线程，不运行周期性 `loginctl`。主动锁屏查询仍回复“未锁定、不支持自动解锁”，意外收到解锁请求也不会调用 PAM/logind。
- 不创建 CONTROL/TEXT 的 100ms 桌面设置任务、TEXT 剪贴板升级等待、CONTROL 初始化后的 200ms 桌面升级等待，或色度更新转发任务。
- 保留工具连接所需的 CONTROL/TEXT 通道、名义 ScreenSources/CaptureChange、权限和设置应答。设置请求在消息回调里按原来的共享锁串行处理。
- 图形模式仍保留桌面升级、锁屏、剪贴板和设置处理路径。

图形库链接拆分属于阶段 B。本阶段也没有删除桌面崩溃后的恢复逻辑、文件管理或内置终端。

### 端口映射

每个 TCP 流用一个受管任务执行建连和双向读写，注册发生在异步建连之前。收到 FIN、FILE 通道关闭、PeerConnection Closed/Failed 或宿主释放 Session 时，都能取消建连、阻塞读写、排队中的 SYN_ACK/FIN。

- shutdown 是永久且可重复调用的状态转换；关闭后不能再注册流。
- 同一身份的重复 SYN 在原流仍存在时忽略；原流关闭后允许复用。任务清理带 generation 检查，旧任务不会移除新流。远端 FIN 使旧流尚在应用队列中的帧失效。
- 本地写队列满时先释放 TCP 两个方向，再按序等待 FIN 入队；不因出站队列满而静默丢弃 FIN。远端 FIN 或会话关闭仍能取消这个等待。
- SCTP 背压使用 buffered-amount-low 通知，并检查通道状态。30 秒发送期限覆盖通道就绪、背压及 `send()` 本身；失败会释放映射并关闭该 FILE 通道，让客户端观察到故障并重新连接，不主动关闭其他 peer。
- peer 关闭由状态事件触发，替换原先的一秒轮询。

保留 Message field 27、FILE_DATA_CHANNEL、身份字段 wire type、SYN_ACK→DATA→FIN 的发送顺序和原有 256 KiB 接收上限。队列仍按帧数限制；字节预算、并发限制和公平调度留给阶段 C。

## 自动化验收

新增测试位于 `src/port_mapping_tests.rs` 和 `src/desktop_control_tests.rs`，覆盖：

| 场景 | 验证内容 |
|---|---|
| 进程隔离的 headless 协议测试 | 初始化字段保留；CONTROL/TEXT 设置应答；锁屏能力为 false；未启动锁屏 watcher；端口映射仍可响应 |
| 双向 TCP 与真实 SCTP | 192 KiB 单帧（大于 64 KiB）完整性；ACK、DATA、FIN 顺序 |
| 建连中断与超时 | 已开始但未完成的连接可被 FIN/shutdown 取消；10 秒超时只产生一次失败 ACK |
| 队列满与目标不读取 | 阻塞 ACK、写入和 FIN 可取消；本地溢出先关闭 TCP，FIN 等待容量 |
| 重复 SYN、身份复用 | 不重复创建连接，旧任务清理不会影响替代流，旧排队帧失效 |
| 1000 次 TCP 流重连 | 每次读写校验、任务结束、流表归零；独立 Linux 运行另检查 FD/线程回落 |
| SCTP 丢包及恢复 | 虚拟网络临时丢弃包，恢复后逐字节校验 6 MiB 数据；有积压时关闭无需等到 buffered_amount 归零 |
| FILE/peer 关闭 | 实际 SCTP 关闭触发 TCP EOF，另一 peer 保持可用 |
| 无通道与永久关闭 | 出站期限到达后退出，重复 shutdown 安全，不能注册新流 |

SCTP 测试的细节：依赖库自己的 pending queue 在约 128 KiB 就可能阻塞 `send()`，早于本模块保留的 4 MiB 检查。因此低水位回调测试仅在测试实例使用 32 KiB 阈值；另用生产默认阈值测试取消阻塞的 `send()`。这不是生产缓冲参数调优，也不把最初等待 4 MiB 的测试超时当作产品故障。

## 执行记录

- macOS arm64，Rust 1.97.1：`cargo test --locked`，358 passed、0 failed、6 ignored。
- ThinkPad，Ubuntu 26.04.1 x86_64、内核 7.0.0-42-generic、Rust 1.93.1：最终 `cargo test --locked`，378 passed、0 failed、11 ignored。
- ThinkPad 独立 1000 次 TCP 流重连测试：通过，FD 数 10→10、线程数 2→2；每次流任务结束、流表归零。
- 非测试版 `cargo build --locked --bin uuremote-server`：通过，产物为 Linux x86_64 ELF（debug 构建，未制作 release 包）；无头环境下执行 `uuremote-server help` 正常退出。复核无新增 UU 系统服务。
- 两端五个修改/新增 Rust 文件 SHA-256 一致。测试前在 ThinkPad 安装了 Rust/Cargo、pkg-config 和 X11 开发依赖，使用独立源码目录；未安装 UU 系统服务或变更其账号状态。
- 原始 checkout 在 macOS 的第一次并行全量测试有一个既有终端测试失败（`locked_control_unlock_requests_and_tools_work_without_a_video_frame` 解析非 TERM 消息），同一原始代码单独重跑通过。本次未修改该测试的既有逻辑；最终全量结果以上述记录为准。

复现命令：

```sh
cargo test --locked
cargo test --locked port_mapping::
# Linux：只运行这一项，避免其他并行测试影响 /proc 计数。
UU_RECONNECT_RESOURCE_CHECK=1 cargo test --locked \
  port_mapping::lifecycle_tests::thousand_reconnects_release_tasks_and_stream_registrations \
  -- --exact --nocapture
cargo build --locked --bin uuremote-server
```

## 验证边界

这些测试使用本地 TCP 和 vendored WebRTC 的隔离虚拟网络，不使用 UU 账号、二维码、云端 relay 或生产服务。1000 次计数指同一 Session 的 TCP 映射流重连，不是 1000 次官方客户端/ICE/TURN 重连。

本轮未进行官方 Mac/Windows 客户端的新版本端到端验收、真实 SSH/SFTP 长传输、15 分钟空闲 A/B、跨境吞吐测试或长期泄漏测量。不能将原生产版本的连通性数据作为新构建已通过官方客户端验收的证据，也不宣称具体 CPU/PSS/RTT 改善比例。

绿云的程序、服务和登录状态均未操作。后续新版本先在测试设备完成官方客户端验证，生产部署与回滚继续由 OPS 会话负责。
