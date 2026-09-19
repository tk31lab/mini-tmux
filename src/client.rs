//! Milestone 3, 4, 6: クライアント側の実装。
//!
//! クライアントの責務:
//!   - サーバーのソケットに接続する
//!   - 自分の標準入力をraw modeにする(term::RawModeGuard)
//!   - 自分の標準入力から読んだバイトをそのままソケットに送る
//!   - ソケットから受け取ったバイトをそのまま自分の標準出力に書く
//!   - SIGWINCH(ウィンドウのリサイズ)を検知して、新しい端末サイズを
//!     サーバーに通知する
//!
//! デタッチについて: 今のところ専用のキーシーケンス(例: Ctrl-b d)は
//! 実装していない。クライアントプロセスを終了させる(Ctrl-Cではなく、
//! 別プロセスからkillするか、ターミナル自体を閉じる)のがそのまま
//! デタッチになる。サーバー/クライアントが別プロセスに分離されたことで、
//! クライアントが終了してもサーバー側のシェルは動き続ける。

use std::io::Write;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::net::UnixStream;

use nix::errno::Errno;
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

    let resize_events = watch_resize()?;

    relay_stdin_and_socket(stream, resize_events)
}

/// Milestone 6: SIGWINCHのハンドラを登録し、通知を受け取るためのfdを返す。
///
/// いわゆる「self-pipe trick」。シグナルハンドラの中でできることは非常に
/// 限られている(async-signal-safeな操作のみ)ので、ハンドラでは
/// 「パイプに1バイト書く」だけを行い、実際の処理はpollループ側で行う。
/// こうするとシグナルを「読み取り可能になったfd」というpollが扱える形に
/// 変換できる。単なるフラグ変数と違い、poll()でブロックする直前に
/// シグナルが来ても取りこぼさない。
fn watch_resize() -> std::io::Result<UnixStream> {
    let (reader, writer) = UnixStream::pair()?;
    // writerの所有権はsignal-hook側に渡る(シグナル発生時にここへ書き込む)。
    signal_hook::low_level::pipe::register(signal_hook::consts::SIGWINCH, writer)?;
    Ok(reader)
}

/// 今の自分の端末サイズをサーバーに通知する。
fn send_window_size(socket_writer: &mut UnixStream) -> std::io::Result<()> {
    let (rows, cols) = term::window_size(0)?;
    let frame = protocol::encode_client_message(&ClientMessage::Resize { rows, cols });
    socket_writer.write_all(&frame)
}

/// 自分の標準入力/出力とソケットとの間でデータを中継する。poll(2)で
/// 標準入力・ソケット・リサイズ通知の3つを同時に監視する単一ループ。
fn relay_stdin_and_socket(
    stream: UnixStream,
    resize_events: UnixStream,
) -> std::io::Result<()> {
    let socket_fd = stream.as_raw_fd();
    let resize_fd = resize_events.as_raw_fd();
    let mut socket_writer = stream.try_clone()?;

    // アタッチした時点のサイズをまず伝える。これをしないと、サーバー側の
    // ptyは作成時のデフォルト(24x80)のままになってしまう。再アタッチ時に
    // 前回と違うサイズの端末から繋ぐ場合もあるので、毎回必要。
    let _ = send_window_size(&mut socket_writer);

    // Safety: fd 0はプロセス全体で有効。socket_fd/resize_fdは、それぞれの
    // UnixStreamがこの関数の終わりまでdropされないので有効。
    let stdin_fd = unsafe { BorrowedFd::borrow_raw(0) };
    let socket_borrowed = unsafe { BorrowedFd::borrow_raw(socket_fd) };
    let resize_borrowed = unsafe { BorrowedFd::borrow_raw(resize_fd) };

    let mut recv_buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];

    loop {
        let mut fds = [
            PollFd::new(stdin_fd, PollFlags::POLLIN),
            PollFd::new(socket_borrowed, PollFlags::POLLIN),
            PollFd::new(resize_borrowed, PollFlags::POLLIN),
        ];
        match poll(&mut fds, PollTimeout::NONE) {
            Ok(_) => {}
            // シグナルで中断されただけなので、やり直す。ここで抜けてしまうと
            // ウィンドウをリサイズした瞬間にクライアントが終了してしまう。
            Err(Errno::EINTR) => continue,
            Err(_) => break,
        }

        let stdin_revents = fds[0].revents().unwrap_or_else(PollFlags::empty);
        let socket_revents = fds[1].revents().unwrap_or_else(PollFlags::empty);
        let resize_revents = fds[2].revents().unwrap_or_else(PollFlags::empty);

        if resize_revents.intersects(PollFlags::POLLIN) {
            // 溜まっている通知バイトは読み捨てる(連続したリサイズが
            // まとめられていることがあるが、やることは「今のサイズを
            // 送り直す」の1回で足りる)。
            let _ = nix::unistd::read(resize_fd, &mut chunk);
            let _ = send_window_size(&mut socket_writer);
        }

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
