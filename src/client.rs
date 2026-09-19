//! Milestone 3, 4, 6: クライアント側の実装。
//!
//! クライアントの責務:
//!   - サーバーのソケットに接続する
//!   - 自分の標準入力をraw modeにする(term::RawModeGuard)
//!   - 自分の標準入力から読んだバイトをそのままソケットに送る
//!   - ソケットから受け取ったバイトをそのまま自分の標準出力に書く
//!   - Milestone 6: SIGWINCHを受け取ったら、新しい端末サイズをサーバーに
//!     通知する(signal-hookクレートの利用を想定)
//!
//! デタッチについて: 今のところ専用のキーシーケンス(例: Ctrl-b d)は
//! 実装していない。クライアントプロセスを終了させる(Ctrl-Cではなく、
//! 別プロセスからkillするか、ターミナル自体を閉じる)のがそのまま
//! デタッチになる。サーバー/クライアントが別プロセスに分離されたことで、
//! クライアントが終了してもサーバー側のシェルは動き続ける。

use std::io::Write;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::net::UnixStream;

use nix::poll::{poll, PollFd, PollFlags, PollTimeout};

use crate::protocol::{self, ClientMessage, ServerMessage};
use crate::session;
use crate::term;

/// 指定したセッションに接続し、対話を開始する。デタッチする(=プロセスが
/// 終了させられる)か、サーバー側のシェルが終了するまで戻らない。
pub fn attach(session_name: &str) -> std::io::Result<()> {
    let socket_path = session::socket_path(session_name);
    let stream = UnixStream::connect(&socket_path).map_err(|e| {
        std::io::Error::new(
            e.kind(),
            format!(
                "セッション '{session_name}' に接続できませんでした({e})。\
                 `mini-tmux new -s {session_name}` で先に作成してください"
            ),
        )
    })?;

    // 自分の標準入力(fd 0)をraw modeにする。この関数を抜けるとき(panicも
    // 含め)ガードがDropされ、自動的に元のterminal設定に戻る。
    let _raw_guard = term::RawModeGuard::enable(0)?;

    relay_stdin_and_socket(stream)
}

/// 自分の標準入力/出力とソケットとの間でデータを中継する。poll(2)で
/// 標準入力とソケットを同時に監視する単一ループ(main.rsのMilestone 2の
/// 実装と同じ考え方)。
fn relay_stdin_and_socket(stream: UnixStream) -> std::io::Result<()> {
    let socket_fd = stream.as_raw_fd();
    let mut socket_writer = stream.try_clone()?;

    // Safety: fd 0はプロセス全体で有効。socket_fdはこの関数の終わりまで
    // `stream`がdropされないので有効。
    let stdin_fd = unsafe { BorrowedFd::borrow_raw(0) };
    let socket_borrowed = unsafe { BorrowedFd::borrow_raw(socket_fd) };

    let mut recv_buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];

    loop {
        let mut fds = [
            PollFd::new(stdin_fd, PollFlags::POLLIN),
            PollFd::new(socket_borrowed, PollFlags::POLLIN),
        ];
        if poll(&mut fds, PollTimeout::NONE).is_err() {
            break;
        }
        let stdin_revents = fds[0].revents().unwrap_or_else(PollFlags::empty);
        let socket_revents = fds[1].revents().unwrap_or_else(PollFlags::empty);

        if stdin_revents.intersects(PollFlags::POLLIN) {
            match nix::unistd::read(0, &mut chunk) {
                Ok(0) | Err(_) => break, // 自分の標準入力がEOF(Ctrl-D)になった
                Ok(n) => {
                    let frame =
                        protocol::encode_client_message(&ClientMessage::Input(chunk[..n].to_vec()));
                    if socket_writer.write_all(&frame).is_err() {
                        break;
                    }
                }
            }
        }
        if stdin_revents.intersects(PollFlags::POLLHUP | PollFlags::POLLERR) {
            break;
        }

        if socket_revents.intersects(PollFlags::POLLIN) {
            match nix::unistd::read(socket_fd, &mut chunk) {
                Ok(0) | Err(_) => break, // サーバーが切断した
                Ok(n) => {
                    recv_buf.extend_from_slice(&chunk[..n]);
                    while let Some((msg, consumed)) = protocol::decode_server_message(&recv_buf) {
                        recv_buf.drain(..consumed);
                        match msg {
                            ServerMessage::Output(bytes) => {
                                if std::io::stdout().write_all(&bytes).is_err() {
                                    return Ok(());
                                }
                                let _ = std::io::stdout().flush();
                            }
                            ServerMessage::ShellExited { code } => {
                                println!("\r\n[mini-tmux] シェルが終了しました (code={code})");
                                return Ok(());
                            }
                        }
                    }
                }
            }
        }
        if socket_revents.intersects(PollFlags::POLLHUP | PollFlags::POLLERR) {
            break;
        }
    }

    Ok(())
}

/// Milestone 6: SIGWINCHのハンドラ登録と、リサイズ通知の送信。
fn watch_resize(_stream: &UnixStream) {
    todo!("signal-hookでSIGWINCHを監視し、ioctl(TIOCGWINSZ)で新しいサイズを取得して\nprotocol::ClientMessage::Resizeとして送る")
}
