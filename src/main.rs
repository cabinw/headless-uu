//! Dedicated, display-independent UU port-mapping host.
mod cloud;
mod runtime;
mod transport;
mod wire;
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
}
fn command(args: &[String]) -> Result<Command, ()> {
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] | ["run"] => Ok(Command::Run),
        ["login"] | ["login", "--no-gui"] => Ok(Command::Login),
        ["help"] | ["--help"] | ["-h"] => Ok(Command::Help),
        ["version"] | ["--version"] => Ok(Command::Version),
        ["doctor"] => Ok(Command::Doctor),
        _ => Err(()),
    }
}
fn usage() {
    println!("{}", i18n::s("用法：uuremote-server [run | login [--no-gui] | doctor | version | help]\n无参数：运行无头端口映射宿主；不提供桌面、内置终端或文件管理。",
        "Usage: uuremote-server [run | login [--no-gui] | doctor | version | help]\nNo arguments: run the headless port-mapping host. No desktop, built-in terminal or file manager."));
}
#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<_> = std::env::args().skip(1).collect();
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
                command(&args.into_iter().map(str::to_owned).collect::<Vec<_>>()),
                Err(())
            );
        }
        assert_eq!(command(&[]), Ok(Command::Run));
        assert_eq!(
            command(&["login".into(), "--no-gui".into()]),
            Ok(Command::Login)
        );
    }
}
