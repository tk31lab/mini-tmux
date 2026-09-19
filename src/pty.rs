//! Milestone 1: pty(疑似端末)の生成と、その上でのシェル起動。
//!
//! 目標: 新しいpty(master/slave)を作り、slave側をシェルの標準入出力に
//! 割り当てて起動し、呼び出し側はmaster側のfdを通じてシェルと対話できる
//! ようにする。
//!
//! 参考API(nixクレート、feature "term" / "process"):
//!   - nix::pty::openpty(None, None) -> master fdとslave fdのペアを返す
//!   - nix::unistd::fork() -> 親/子に分岐する
//!   - 子プロセス側: nix::unistd::setsid(), slave fdをdup2で0,1,2に複製,
//!     nix::unistd::execvp() でシェルを起動する
//!   - 親プロセス側: master fdを保持し、read/writeで子プロセスと対話する

use std::ffi::CString;
use std::os::fd::{BorrowedFd, IntoRawFd, RawFd};

use nix::pty::openpty;
use nix::unistd::{close, dup2, execvp, fork, read, setsid, write, ForkResult};

/// 生成したptyのmaster側と、その先で動いている子プロセスのPIDをまとめた型。
pub struct PtyProcess {
    pub master_fd: RawFd,
    pub child_pid: i32,
}

/// 新しいptyを作り、その上で`command`(例: ["/bin/zsh"])を起動する。
///
/// 実装の流れ:
///   1. openptyでmaster/slaveのfdペアを取得する
///   2. fork()する
///   3. 子プロセス側: setsidで新しいセッションリーダーになり、slave fdを
///      stdin/stdout/stderrにdup2する。master/slaveの元のfdは閉じる。
///      execvpでcommandを起動する。
///   4. 親プロセス側: slave fdは閉じる(子だけが使う)。master_fdと子のpidを
///      PtyProcessとして返す。
pub fn spawn_in_pty(command: &[&str]) -> std::io::Result<PtyProcess> {
    assert!(!command.is_empty(), "command must have at least a program name");

    let pty = openpty(None, None)?;
    // OwnedFdのままだとスコープを抜けた瞬間にcloseされてしまうので、
    // fork前にraw fdへ変換して寿命を自分たちで管理する。
    let master_fd = pty.master.into_raw_fd();
    let slave_fd = pty.slave.into_raw_fd();

    // Safety: forkの直後、子プロセス側ではexecvpまでの間、
    // async-signal-safeな操作(setsid, dup2, close)しか呼んでいない。
    match unsafe { fork() }? {
        ForkResult::Child => {
            // 子プロセス: 新しいセッションのリーダーになり、pty slaveを
            // 自分の制御端末にする。これでCtrl-CなどのシグナルがSIGWINCHや
            // ジョブ制御と正しく結びつく。
            setsid().expect("setsidに失敗しました");

            // TIOCSCTTY: このpty slaveを、今作ったセッションの制御端末として
            // 明示的に登録する。これをやらないと、外側の端末が閉じたときの
            // ハングアップ(SIGHUP)がこのプロセスグループに正しく配送されず、
            // フォアグラウンドで実行中のコマンドがいつまでも残ってしまう
            // (setsid直後・dup2より前に、session leaderが行う必要がある)。
            if unsafe { nix::libc::ioctl(slave_fd, nix::libc::TIOCSCTTY as _, 0) } != 0 {
                eprintln!(
                    "mini-tmux: TIOCSCTTYに失敗しました: {}",
                    std::io::Error::last_os_error()
                );
            }

            // master側は子には不要。
            close(master_fd).ok();

            for target_fd in 0..=2 {
                dup2(slave_fd, target_fd).expect("dup2に失敗しました");
            }
            if slave_fd > 2 {
                close(slave_fd).ok();
            }

            let program =
                CString::new(command[0]).expect("コマンド名にNUL文字は使えません");
            let args: Vec<CString> = command
                .iter()
                .map(|s| CString::new(*s).expect("引数にNUL文字は使えません"))
                .collect();

            // 成功すればここには戻ってこない。
            let _ = execvp(&program, &args);
            eprintln!("mini-tmux: execvp({:?}) に失敗しました", command);
            std::process::exit(127);
        }
        ForkResult::Parent { child } => {
            // 親プロセス: slave側は子だけが使うので閉じる。
            close(slave_fd).ok();
            Ok(PtyProcess {
                master_fd,
                child_pid: child.as_raw(),
            })
        }
    }
}

/// master fdからの読み出し。シェルの出力(標準出力・標準エラー)がここに
/// 流れてくる。
pub fn read_from_pty(master_fd: RawFd, buf: &mut [u8]) -> std::io::Result<usize> {
    Ok(read(master_fd, buf)?)
}

/// master fdへの書き込み。ここに書いたバイトはシェルの標準入力として届く。
pub fn write_to_pty(master_fd: RawFd, buf: &[u8]) -> std::io::Result<usize> {
    // Safety: master_fdはPtyProcessが生きている間有効であることを
    // 呼び出し側が保証する。BorrowedFdはここでの一時的な借用に過ぎず、
    // 元のfdをcloseしたりはしない。
    let fd = unsafe { BorrowedFd::borrow_raw(master_fd) };
    Ok(write(fd, buf)?)
}

/// Milestone 6: 自分のターミナルのサイズ変更をptyに伝える。
/// TIOCSWINSZ ioctlで、master_fd経由でslave側のwindow sizeを更新する。
pub fn resize_pty(master_fd: RawFd, rows: u16, cols: u16) -> std::io::Result<()> {
    let _ = (master_fd, rows, cols);
    todo!("nix::pty::Winsize を組み立てて、ioctl(master_fd, TIOCSWINSZ, ...) 相当を呼ぶ")
}
