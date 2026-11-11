# headless-uu: maintainer guide

Read this file before changing the project. Human documentation is in [README.md](README.md) and [README.en.md](README.en.md).

## Scope

This is `cabinw/headless-uu`, a GPL-3.0-or-later fork of `VallierDev/uu-remote-linux`, based on commit `fe052cdca7ad90d1ab7c27d031036b1071869070`. The Cargo package is `cabinw-uu-headless`; the executable remains `uuremote-server` for compatibility with existing deployments.

The product is a dedicated headless host for official UU port mapping. Keep per-machine device identity, CLI QR login, cloud signaling/heartbeats/reconnects, cloud ICE/TURN policy, DTLS/SCTP, and TCP forwarding. Authentication and file access belong to the target service, normally the existing sshd providing SSH/SFTP.

The explicit `set-wallpaper [--] <image>` CLI command is an allowed, independent cloud feature: upload one user-selected PNG/JPEG/WebP and update the current signed-in identity's device-card cover. Require an existing complete identity; do not register a device or start login automatically. Keep it outside the daemon lifecycle, with no desktop discovery, screenshot capture, wallpaper polling or graphical dependencies. See [the cover protocol and validation notes](docs/wallpaper.md).

Do not reintroduce desktop capture, input injection, clipboard, GUI, codecs/encoders, native viewer, remote assistance, power operations, PAM, or UU's built-in terminal/file manager. Inert media negotiation and nominal screen messages may remain where official tool clients require them; that does not authorize adding a desktop subsystem.

## Working rules

- Never log tokens, passwords, QR URLs, TURN credentials, terminal text or forwarded bytes. Use sizes, counts, field numbers and booleans. QR codes are credentials: show only to the account owner during explicit login, never include in logs, issues, reports or commits.
- For cover uploads, also keep image contents, local paths, file tokens, returned URLs and API response bodies out of logs and reports. Account API headers must never accompany requests to the file-upload service. Tests use synthetic credentials and local fixtures, never real account configuration or live uploads unless explicitly authorized.
- Do not read or copy real account configuration as part of tests. Each machine owns its identity. Default configuration remains `~/.config/uuremote/config.json`, overridden by `UU_CONFIG`; preserve file `0600` and directory `0700`.
- Keep logs in English and new user-facing CLI text bilingual through `crate::i18n::s` or `crate::tr!`. Update both READMEs for behavior changes.
- Preserve upstream attribution, GPL obligations, third-party licenses and vendored patch explanations. Do not add NetEase app binaries, icons or screenshots.
- Use observed upstream/official protocol evidence for wire shapes. Cite the evidence in comments when changing protocol behavior. Do not advertise capabilities whose business handlers were removed.
- New connections must not close unrelated peers. Pending connects, blocked reads/writes, outbound waits and queued close notifications must all end on session shutdown. Never log payloads while investigating this.
- Refactor and test locally. Production deployment, service restarts, account login and live client acceptance are separate tasks; follow the user's authorized scope. Local tests do not imply official-client compatibility or production readiness.

## Code organization

- `src/cloud/`: signed API requests, configuration and device identity, login, Engine.IO/Socket.IO signaling and WebSocket transport.
- `src/transport/`: WebRTC peer lifecycle, cloud ICE configuration, address-free path diagnostics and TCP port mapping.
- `src/wire/`: protobuf primitives and the minimal official tool compatibility messages.
- `src/runtime/`: process lifecycle, command-line helpers and internationalization.
- `src/main.rs`: command routing and module wiring.
- `vendor/`: patched WebRTC/ICE/DTLS/SRTP/SCTP dependencies. Keep their licenses and patches as path dependencies unless deliberately replacing and revalidating them.
- `packaging/systemd/`: optional system-service example. It runs as an ordinary user and does not require a graphical session.
- `docker/`: optional source-built container; host networking means loopback targets the Linux Docker host.

The historical [phase A record](docs/headless-phase-a.md) explains earlier lifecycle work. Its old file paths and test counts describe that snapshot, not the reorganized project's current acceptance status.

## Protocol constraints

- The host registers as the official Windows platform (`x-param-plat=1`); keep registration/signature behavior compatible.
- `ConnectOptions.capture_type=9` is port mapping. Unsupported business tools must not launch local processes, access user files or initialize a desktop.
- Port mapping is Message field **27** on **FILE_DATA_CHANNEL**. Keep this channel even though UU's file-manager business is removed.
- Identity fields 1/2/3 (session/rule/stream) must be echoed with their original wire types. Types: SYN=0, FIN=1, DATA=2, SYN_ACK=3, DATA_ACK=4. SYN JSON uses `version`, `target_host`, `target_port`; SYN_ACK uses `version`, `ok`, optional `error`. Both ends ignore DATA_ACK; official tools do not support TCP half-close.
- CONTROL/TEXT initialization includes a nominal 1920×1080 screen, CaptureChange and tool permission replies. Official mapping clients can abort if these are missing or permissions are rejected. This is protocol compatibility, not a real display.
- Echo replies acknowledge SimpleAction 0 with action 1 and `{"seq":...}`. Only declare implemented protocol features.
- Keep the 256 KiB data-channel receive compatibility patch and the sent SDP `a=max-message-size:262144`; the stored local description cannot be altered indiscriminately. Official clients can send frames larger than 64 KiB.
- Retain cloud ICE/TURN credentials and force-relay policy handling. `UU_USE_PROXY=1` opts into proxies; it does not mean force relay. Confirm actual path with `ICE path ... route=relay|direct`.
- Device-card covers follow the observed file-token → raw image upload → signed `wallpaper_url` update flow. The 10 MiB limit is local, in addition to the server's `max_size`; it is not a claimed official limit. Do not infer automatic client refresh, cropping, cache eviction or a reset API from upload success.
- Media SDP pruning is a separate compatibility experiment. Do not remove inert tracks/codecs/interceptors merely because no media is generated.

## Build and verification

Rust stable and a C compiler are required by cryptography dependencies. The project has no native C engines, `build.rs`, desktop feature matrix or graphical library dependencies.

```sh
cargo test --locked
cargo build --locked --bin uuremote-server
cargo build --release --locked --bin uuremote-server
# Linux: inspect rather than infer dependencies from source size.
readelf -d target/debug/uuremote-server | grep NEEDED
```

Use focused tests while editing, then the relevant complete suite. Test real local TCP/SCTP, byte integrity, unknown/duplicate frames, pending connection cancellation, target not reading, full queues, closure while congested, peer isolation and reconnect cleanup. Environment-dependent tests must isolate process-wide settings. A 1000 TCP-stream reconnect test is not 1000 official-client/ICE reconnects.

A headless artifact must not link GTK/GDK, X11/XCB, Wayland, FFmpeg, SDL, PipeWire, PulseAudio, Opus or PAM. Do not "verify" this by uninstalling libraries used by an existing production binary.

Do not claim performance improvements from fewer files or a smaller package. Compare CPU time, RSS/PSS, FD/thread counts, throughput and interactive latency on the same route and workload. Official Mac/Windows clients, real relay, SSH/SFTP and restart recovery require explicit live validation beyond automated tests.
