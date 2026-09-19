//! Milestone 4: サーバープロセスをバックグラウンド化(デーモン化)する。
//!
//! 手順:
//!   1. fork()する。親はすぐexit(0)する(呼び出し元に制御を返す)
//!   2. 子プロセスはsetsid()で新しいセッションのリーダーになる
//!      (これで制御端末を持たない状態になる)
//!   3. 標準入力/出力/エラーを/dev/nullにリダイレクトする
//!      (端末に紐づいたままだと、ターミナルを閉じたときに巻き込まれる)
//!   4. その状態でserver::run()を呼ぶ(呼び出し側の責務)
//!
//! 参考API(nixクレート、feature "process" / "unistd"):
//!   nix::unistd::{fork, ForkResult, setsid, dup2, close}

use std::os::fd::IntoRawFd;

use nix::unistd::{close, dup2, fork, setsid, ForkResult};

/// 現在のプロセスをデーモン化する。呼び出し後、この関数から戻ってくるのは
/// デーモン化されたプロセス(子)のみ(元のプロセスはexitしている)。
pub fn daemonize() -> std::io::Result<()> {
    // Safety: forkの直後、親はexitのみ、子はasync-signal-safeな
    // setsid/dup2/closeしか呼んでいない。
    match unsafe { fork() }? {
        ForkResult::Parent { .. } => {
            // 呼び出し元(のちにclientとして振る舞うプロセス)に制御を
            // 返すため、ここで即終了する。
            std::process::exit(0);
        }
        ForkResult::Child => {}
    }

    // 新しいセッションのリーダーになる = 制御端末を持たない状態になる。
    // これで、後から元のターミナルを閉じてもSIGHUPの影響を受けない。
    setsid()?;

    redirect_stdio_to_dev_null()?;

    Ok(())
}

/// 標準入力/出力/エラーを/dev/nullに繋ぎ変える。端末につながったままだと、
/// 元のターミナルを閉じたときにこのプロセスも巻き込まれる可能性がある。
fn redirect_stdio_to_dev_null() -> std::io::Result<()> {
    let dev_null_fd = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")?
        .into_raw_fd();

    for target_fd in 0..=2 {
        dup2(dev_null_fd, target_fd)?;
    }
    if dev_null_fd > 2 {
        close(dev_null_fd).ok();
    }

    Ok(())
}
