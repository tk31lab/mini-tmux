//! mini-tmux エントリポイント。
//!
//! サブコマンド:
//!   mini-tmux new -s <name>     新しいセッションを作る(サーバー未起動なら起動する)
//!   mini-tmux attach -s <name>  既存セッションにアタッチする
//!   mini-tmux ls                起動中のセッション一覧を表示する
//!
//! 引数パースはあえて手書き(clap等には頼らない)。最初はシンプルにしておいて、
//! 使いにくくなったら差し替える。

mod pty;
mod term;
mod protocol;
mod session;
mod server;
mod client;
mod daemon;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    match args.get(1).map(String::as_str) {
        Some("new") => {
            let name = parse_session_name(&args).unwrap_or_else(|| "default".to_string());
            cmd_new(&name);
        }
        Some("attach") => {
            let name = parse_session_name(&args).unwrap_or_else(|| "default".to_string());
            cmd_attach(&name);
        }
        Some("ls") => {
            cmd_ls();
        }
        _ => {
            eprintln!("usage: mini-tmux <new|attach|ls> [-s <session-name>]");
            std::process::exit(1);
        }
    }
}

/// `-s <name>` を雑に拾うだけのヘルパー。
fn parse_session_name(args: &[String]) -> Option<String> {
    let pos = args.iter().position(|a| a == "-s")?;
    args.get(pos + 1).cloned()
}

/// セッションが存在しなければサーバーを起動し、その後クライアントとして
/// アタッチする(実物のtmuxの`new-session`と同じで、作成したその場で
/// 使い始められるようにする)。
fn cmd_new(session_name: &str) {
    if !session::is_running(session_name) {
        spawn_server_daemon(session_name);
        wait_until_session_ready(session_name);
    }

    if let Err(e) = client::attach(session_name) {
        eprintln!("mini-tmux: {e}");
        std::process::exit(1);
    }
}

/// サーバーをフォーク+デーモン化して起動する。この関数自体はすぐ戻る
/// (実際にサーバーとして動き続けるのは孫プロセス)。
fn spawn_server_daemon(session_name: &str) {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());

    // Safety: forkの直後、子はdaemon::daemonize()を呼ぶだけ
    // (setsid/dup2/closeなどasync-signal-safeな操作のみ)。
    match unsafe { nix::unistd::fork() }.expect("forkに失敗しました") {
        nix::unistd::ForkResult::Child => {
            // daemon::daemonize()の中でさらにforkする(このプロセス自身は
            // そこですぐexitし、実際にサーバーとして生き残るのは孫プロセス)。
            daemon::daemonize().expect("daemonizeに失敗しました");
            let _ = server::run(session_name, &[&shell]);
            std::process::exit(0);
        }
        nix::unistd::ForkResult::Parent { child } => {
            // daemonize内の一段目のfork(「親はすぐexitする」)に対応する
            // 中間プロセスを回収する。ほぼ即座に終わるはず。
            let _ = nix::sys::wait::waitpid(child, None);
        }
    }
}

/// サーバーがソケットのbindを終えてクライアントを受け付けられる状態になる
/// まで、短い間隔でリトライしながら待つ。
fn wait_until_session_ready(session_name: &str) {
    for _ in 0..100 {
        if session::is_running(session_name) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    eprintln!("mini-tmux: セッション '{session_name}' の起動待ちがタイムアウトしました");
}

/// Milestone 3: 既存セッションにクライアントとして接続する。
fn cmd_attach(session_name: &str) {
    if !session::is_running(session_name) {
        eprintln!(
            "mini-tmux: セッション '{session_name}' は起動していません \
             (`mini-tmux new -s {session_name}` で作成してください)"
        );
        std::process::exit(1);
    }

    if let Err(e) = client::attach(session_name) {
        eprintln!("mini-tmux: {e}");
        std::process::exit(1);
    }
}

/// Milestone 7 (拡張): 起動中のセッション一覧を表示する。
fn cmd_ls() {
    let sessions = session::list_sessions();

    if sessions.is_empty() {
        println!("起動中のセッションはありません");
        return;
    }

    for name in sessions {
        println!("{name}");
    }
}
