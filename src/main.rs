//! Dedicated, display-independent UU port-mapping host.
mod cloud;
mod runtime;
mod transport;
mod wire;
use std::{ffi::OsString, path::PathBuf};
// Internal aliases keep upstream protocol patches easy to compare and backport.
use cloud::{auth, config, login, signaling, websocket as ws_transport};
use runtime::i18n;
#[cfg(test)]
use transport::test_network;
use transport::{diagnostics as network_diagnostics, ice as session_ice, port_mapping};
use wire::{compat, initialization, protobuf as protocol};

#[derive(Debug, PartialEq)]
enum Command {
    Run,
    Login,
    Help,
    Version,
    Doctor,
    SetWallpaper(PathBuf),
    WallpaperHelp,
}
fn command(args: &[OsString]) -> Result<Command, ()> {
    let Some((name, tail)) = args.split_first() else {
        return Ok(Command::Run);
    };
    match (name.to_str(), tail) {
        (Some("run"), []) => Ok(Command::Run),
        (Some("login"), []) => Ok(Command::Login),
        (Some("login"), [flag]) if flag == "--no-gui" => Ok(Command::Login),
        (Some("help" | "--help" | "-h"), []) => Ok(Command::Help),
        (Some("version" | "--version"), []) => Ok(Command::Version),
        (Some("doctor"), []) => Ok(Command::Doctor),
        (Some("set-wallpaper"), [flag]) if flag == "--help" || flag == "-h" => {
            Ok(Command::WallpaperHelp)
        }
        (Some("set-wallpaper"), [image])
            if !image.is_empty() && !image.as_encoded_bytes().starts_with(b"-") =>
        {
            Ok(Command::SetWallpaper(image.into()))
        }
        (Some("set-wallpaper"), [flag, image]) if flag == "--" && !image.is_empty() => {
            Ok(Command::SetWallpaper(image.into()))
        }
        _ => Err(()),
    }
}
fn usage() {
    println!("{}", i18n::s("用法：uuremote-server [run | login [--no-gui] | set-wallpaper [--] <图片> | doctor | version | help]\n无参数：运行无头端口映射宿主；不提供桌面、内置终端或文件管理。",
        "Usage: uuremote-server [run | login [--no-gui] | set-wallpaper [--] <image> | doctor | version | help]\nNo arguments: run the headless port-mapping host. No desktop, built-in terminal or file manager."));
}
fn wallpaper_usage() {
    println!("{}", i18n::s(
        "用法：uuremote-server set-wallpaper [--] <图片>\n使用当前登录身份，将指定的 PNG/JPEG/WebP 图片上传到 UU 文件服务，设置本机的主机卡片封面。\n支持相对或绝对路径；含空格的路径请加引号。本地文件上限 10 MiB，并受服务端限制。\n这是一次性操作，可与宿主同时运行；不会修改本地账号配置。",
        "Usage: uuremote-server set-wallpaper [--] <image>\nUpload the selected PNG/JPEG/WebP image to UU's file service once, setting this device's card wallpaper with the current login.\nUse a relative or absolute path; quote paths containing spaces. The local limit is 10 MiB, subject to the server limit.\nCan run alongside the host; does not modify local account configuration."));
}
#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let selected = match command(&args) {
        Ok(c) => c,
        Err(()) => {
            usage();
            std::process::exit(2);
        }
    };
    let result = match selected {
        Command::Help => {
            usage();
            Ok(())
        }
        Command::Version => {
            println!("cabinw-uu-headless {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Command::Doctor => {
            let credentials = config::read_fresh()
                .ok()
                .and_then(|file| auth::Credentials::load(file.as_ref()).ok())
                .is_some();
            println!(
                "{}",
                i18n::s(
                    "模式：专用无头端口映射（无图形依赖）",
                    "Mode: dedicated headless port mapping (no graphical dependencies)"
                )
            );
            println!(
                "{}: {}",
                i18n::s(
                    "本地凭据可加载（不代表云端在线）",
                    "Local credentials loadable (not proof of cloud connectivity)"
                ),
                credentials
            );
            println!(
                "{}: {}",
                i18n::s("端口映射启用", "Port mapping enabled"),
                port_mapping::enabled()
            );
            Ok(())
        }
        Command::Login => login::run().await,
        Command::WallpaperHelp => {
            wallpaper_usage();
            Ok(())
        }
        Command::SetWallpaper(image) => cloud::wallpaper::run(&image).await.map(|()| {
            println!(
                "{}",
                i18n::s(
                    "主机卡片封面已提交；客户端显示可能需要稍后刷新。",
                    "Device card wallpaper submitted; the client may take time to refresh."
                )
            );
        }),
        Command::Run => {
            session_ice::configure_relay_transport();
            let (stop, stopped) = tokio::sync::watch::channel(false);
            tokio::spawn(async move {
                #[cfg(unix)]
                {
                    let mut term =
                        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                            .expect("SIGTERM handler");
                    tokio::select! { _ = term.recv() => {}, _ = tokio::signal::ctrl_c() => {} }
                }
                #[cfg(not(unix))]
                {
                    let _ = tokio::signal::ctrl_c().await;
                }
                stop.send_replace(true);
            });
            runtime::daemon::run(stopped).await
        }
    };
    if let Err(error) = result {
        eprintln!("{}: {error}", i18n::s("操作失败", "Operation failed"));
        std::process::exit(1);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_or_desktop_commands_never_start_the_host() {
        for args in [
            vec!["desktop-probe"],
            vec!["login", "--gui"],
            vec!["run", "extra"],
            vec!["set-encoder", "cpu"],
        ] {
            assert_eq!(
                command(&args.into_iter().map(OsString::from).collect::<Vec<_>>()),
                Err(())
            );
        }
        assert_eq!(command(&[]), Ok(Command::Run));
        assert_eq!(
            command(&["login".into(), "--no-gui".into()]),
            Ok(Command::Login)
        );
    }

    #[test]
    fn wallpaper_arguments_keep_paths_and_never_fall_through_to_run() {
        let parse = |args: &[&str]| command(&args.iter().map(OsString::from).collect::<Vec<_>>());
        for args in [
            vec!["set-wallpaper"],
            vec!["set-wallpaper", ""],
            vec!["set-wallpaper", "--"],
            vec!["set-wallpaper", "--unknown"],
            vec!["set-wallpaper", "one", "two"],
            vec!["set-wallpaper", "--help", "one"],
        ] {
            assert_eq!(parse(&args), Err(()));
        }
        for path in ["封面 image.png", "/tmp/cover.jpg", "./cover.webp"] {
            assert_eq!(
                parse(&["set-wallpaper", path]),
                Ok(Command::SetWallpaper(path.into()))
            );
        }
        assert_eq!(
            parse(&["set-wallpaper", "--", "-cover.png"]),
            Ok(Command::SetWallpaper("-cover.png".into()))
        );
        for flag in ["--help", "-h"] {
            assert_eq!(parse(&["set-wallpaper", flag]), Ok(Command::WallpaperHelp));
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let path = OsString::from_vec(b"/tmp/cover-\xff.png".to_vec());
            assert_eq!(
                command(&["set-wallpaper".into(), path.clone()]),
                Ok(Command::SetWallpaper(path.into()))
            );
        }
    }
}
