# headless-uu

[简体中文](README.md) · **English**

A persistent Linux host for official UU Remote port mapping, carrying standard SSH, SFTP and other TCP services.

Derived from [VallierDev/uu-remote-linux](https://github.com/VallierDev/uu-remote-linux), maintained at [cabinw/headless-uu](https://github.com/cabinw/headless-uu). Unofficial and unaffiliated with NetEase. No official binaries or brand assets are included. Licensed under GPL-3.0-or-later.

## How it works

```text
OpenSSH / SFTP / VS Code Remote-SSH
  → official UU client's loopback mapping port
  → UU signaling + ICE/TURN + DTLS/SCTP
  → headless-uu
  → the server's existing sshd or another TCP service
```

UU supplies the transport. The existing sshd still authenticates SSH users and keys, and the SSH client still verifies the original host key. The actual route can be direct or relayed, according to cloud policy and ICE negotiation. An online device or available TURN credentials do not prove relay use.

The project retains terminal QR login, per-machine identity and credentials, cloud signaling and reconnects, port mapping and its required CONTROL/TEXT initialization, and TCP lifecycle/backpressure handling.

An explicit, one-shot device-card cover upload is also available without a desktop.

Desktop capture, keyboard/mouse injection, clipboard, audio/video processing, privacy screens, PAM unlock, GUI, the native viewer, remote assistance, and UU's built-in terminal/file manager have been removed. **Use the system SSH/SFTP service through port mapping.** Some inert media negotiation metadata remains for official protocol compatibility; no desktop or audio/video stream is captured or generated.

## Build

Install Rust stable, a C compiler and CA certificates. Some Rust cryptography dependencies still need a C compiler; the project itself no longer builds native C engines. GTK, X11, Wayland, FFmpeg, SDL and graphical sessions are not required.

Ubuntu/Debian example:

```sh
sudo apt-get install --no-install-recommends build-essential ca-certificates
# Install the stable toolchain using the official Rust instructions, then:
git clone https://github.com/cabinw/headless-uu.git
cd headless-uu
cargo test --locked
cargo build --release --locked --bin uuremote-server
./target/release/uuremote-server help
```

Use a release build for live concurrency and throughput acceptance and record the build profile; stripping debug symbols does not provide release optimizations, and debug results are for development diagnostics rather than deployment acceptance.

The Cargo package is `cabinw-uu-headless`. The executable remains `uuremote-server` for deployment compatibility. macOS is useful for development and local transport tests; the service instructions below target Linux/systemd.

## Login and run

Log in independently on each machine. **Never copy another machine's configuration, token or device identity.**

```sh
./target/release/uuremote-server login
./target/release/uuremote-server run
```

Scan and approve the terminal QR code using the official UU Remote mobile app. The QR code is a temporary credential: show it only to the account owner, and never include it in logs, reports or Git. `login --no-gui` remains accepted for compatibility; there is no GUI login path.

`login` requires stdout to be an interactive terminal (TTY) and rejects redirected or piped output so the QR cannot enter logs. Allocate a terminal over SSH, for example `ssh -t your-server 'uuremote-server login'`. Run the dedicated service account’s `sudo -u cabinw ... login` command below in an interactive terminal as well.

The default configuration remains `~/.config/uuremote/config.json`; override it with `UU_CONFIG`. A migration on the same machine may reuse its existing configuration. Keep the file at `0600` and its directory at `0700`. Invoking the executable without a command is equivalent to `run`. Complete login explicitly before unattended service operation.

The host holds a process lock beside its configuration; a second host using the same configuration path exits instead of registering a competing instance. Interactive login has a separate lock, so a running host can wait for login in another terminal to update the file. The process releases the lock when it exits; the lock file does not need manual deletion.

Identity environment variables take precedence over the file. If the service environment contains an expired `UU_TOKEN`, scanning a new QR and rewriting the file cannot override it: remove or update the service environment setting and restart the host. Keep tokens out of shell history, logs and issue reports.

| Command | Purpose |
|---|---|
| `login [--no-gui]` | Terminal QR login |
| `run` or no argument | Run the persistent host |
| `doctor` | Local diagnostics without credentials; not a cloud/SSH connectivity test |
| `set-wallpaper [--] <image>` | Upload an image and update the current identity's device-card cover |
| `version` / `--version` | Print version |
| `help` / `--help` | Print usage |

## Set a device-card cover

`set-wallpaper` is a one-shot cloud operation: it uploads your chosen local PNG, JPEG or WebP to the NetEase UU file service, then updates the current signed-in identity's device card. It does not set the Linux desktop wallpaper, discover a desktop background, capture a screenshot or poll for changes. It leaves `run` and the persistent service behavior unchanged.

```sh
uuremote-server set-wallpaper ./cover.png
uuremote-server set-wallpaper "/srv/pictures/server cover.jpg"
# For a filename starting with -:
uuremote-server set-wallpaper -- -cover.webp
uuremote-server set-wallpaper --help
```

Both relative and absolute paths are accepted. Relative paths use the command's working directory; quote paths containing spaces. The local limit is **10 MiB (10,485,760 bytes)**, also constrained by the service's returned `max_size`. The command does not resize or convert images.

A complete, valid login identity is required. The command uses the current user's default configuration or the path selected by `UU_CONFIG`; identity environment variables still override the file. To update a service's device card, use its user, the same machine's configuration path and the corresponding identity environment. Never copy another machine's configuration. The following paths are examples; the executing user must be able to read the image:

```sh
sudo -u cabinw env UU_CONFIG=/var/lib/cabinw-uu/config.json \
  /usr/local/bin/uuremote-server set-wallpaper /var/lib/cabinw-uu/cover.png
```

Missing or expired credentials cause failure without starting QR login automatically. Run `login` explicitly in an interactive terminal using the same identity, then retry. Remove or update any expired `UU_TOKEN` environment override as well. `set-wallpaper --help` (or `-h`) prints usage without uploading an image.

**Running this command uploads the image contents, including any embedded metadata.** Choose an image you intend to send to UU's cloud service for device-card display; the command does not strip metadata. Success means the cloud API accepted the update, not that every client has refreshed. Client caching, cropping and cover removal remain unverified; there is no clear/reset command.

Exit codes: `0` for success or help; `1` for file, credential, network or API errors; `2` for command syntax errors. See the [cover documentation](docs/wallpaper.md) for protocol evidence and validation scope.

## Linux service

Use a dedicated unprivileged account. This is a new-install example; inspect any existing account or UU service first to avoid conflicting device instances. Keep existing SSH authentication unchanged.

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

The sample uses `User=cabinw`, `Restart=always` and `WantedBy=multi-user.target`, with no graphical login or user linger requirement. Confirm cloud ready status in the log, then verify the mapping and a real SSH connection:

```sh
systemctl status cabinw-uuremote.service
journalctl -u cabinw-uuremote.service --since '10 minutes ago' --no-pager
```

For upgrades, validate the new binary, copy it to a temporary name, then rename it over the old path and restart. Do not overwrite an executing file in place. Keep the previous binary and unit for rollback. Restarting interrupts active mappings. Review existing production paths before adapting this example.

[Docker instructions](docker/README.md) build this source tree without desktop libraries.

## Official client and SSH

Create a mapping on the official client for the correct device:

| Field | Example |
|---|---|
| Local listener | An unused loopback port, such as `127.0.0.1:22314` |
| Target host | `127.0.0.1` |
| Target port | The server's actual SSH port, such as `22` |

Verify that the client binds only to loopback. Targets must be reachable from the host process. Forwarding retains upstream's requested `target_host:target_port` behavior; **a target allowlist is not implemented**.

Create a separate SSH alias and preserve the original management route. Reuse the existing user, authentication method and trusted known-hosts entry. Set `HostKeyAlias` to that original trusted host record; do not accept a new localhost fingerprint or disable strict checking. For aliases normally using a jump host or proxy, explicitly set `ProxyJump none` and `ProxyCommand none` on the mapped alias.

A current `ICE path: ... route=relay` log is evidence of relay use; `route=direct` means direct. `UU_USE_PROXY=1` does not force a relay or guarantee lower latency. Validate account, client-version and cloud-policy compatibility in your own environment.

## Configuration

| Environment variable | Meaning |
|---|---|
| `UU_CONFIG` | Configuration path; default `~/.config/uuremote/config.json` |
| `UU_LANG=zh` / `en` | CLI language; otherwise selected from locale |
| `UU_PORT_MAPPING=0` | Disable target connections |
| `UU_USE_PROXY=1` | Allow REST/WebSocket and TURN TCP/TLS to use existing proxy environment settings; disabled by default |
| `RUST_LOG` | Log level, normally `info` |

The application is always headless. `UU_DESKTOP_BACKEND=none`, `UU_AUDIO=0` and `UU_LOGIN_WINDOW=0` are unnecessary. Keeping those old variables does not restore deleted functionality.

## Structure and validation

`src/cloud/` owns accounts and cloud protocol, `src/transport/` owns WebRTC/TCP forwarding, `src/wire/` owns minimal compatibility messages, and `src/runtime/` owns process lifecycle and CLI support. `vendor/` retains transport patches and their licenses.

```sh
cargo test --locked
cargo build --locked --bin uuremote-server
readelf -d target/debug/uuremote-server | grep NEEDED
```

CI runs Linux/macOS tests and checks the Linux executable for graphical/media library dependencies. The packaging workflow produces downloadable build artifacts only; it does not publish Releases or container images automatically.

See the [refactor record](docs/headless-refactor.md) (Chinese) for the architecture changes, test results and acceptance boundaries.

The [2026-11-11 live validation record](docs/live-validation-2026-11-11.md) (Chinese) covers the official macOS client, SSH/SFTP and synthetic TCP over observed direct and relay paths. File hashes matched in both directions, but new-stream recovery after abnormal disconnects and 64-stream concurrency still had failures. These results do not establish full compatibility or long-term stability.

The [concurrency and early-close validation](docs/extreme-validation-2026-11-11.md) (Chinese), using official macOS client 4.46.0 (630), records a local host head-of-line blocking fix. Release passed application-ACK controls at 64×512 KiB over ThinkPad's direct path and 64×64 KiB over Tencent relay. Both paths still showed truncation during recovery with immediate target closure and slow reads followed by FIN; Tencent also failed the 64×512 KiB burst. Application ACKs are diagnostic only, and these results do not establish that all issues are resolved.

The subsequent [SCTP delayed-ACK fix and validation](docs/sctp-ack-timer-2026-11-11.md) (Chinese) repairs timer rearming after expiry and cancellation while waiting for the observer lock. Lifecycle and real-SCTP regressions fail against the original crate and pass with the patch. Mapping close semantics remain unchanged; see that record for live results and remaining limitations.

Automated validation covers local TCP, real SCTP, queue closure, packet-loss recovery and reconnects. This does not replace official-client, real-relay, SSH/SFTP or long-duration validation. Upstream results and the [phase A record](docs/headless-phase-a.md) describe their historical versions, not official-client acceptance of this complete refactor.

## Attribution and license

The fork starts from upstream commit `fe052cdca7ad90d1ab7c27d031036b1071869070`, near v0.2.57. Original attribution, the [GPL license](LICENSE), independent vendored licenses and `PATCHES.md` records are retained. Distributions of modified binaries must provide corresponding source and build instructions. Continue tracking upstream protocol evidence instead of inventing incompatible message shapes.
