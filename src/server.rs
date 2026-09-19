//! Milestone 3, 4: サーバー側の実装。
//!
//! サーバーの責務:
//!   - ptyを1つ作り、その上でシェルを起動する(pty::spawn_in_pty)
//!   - Unixドメインソケットでlistenし、クライアントの接続を待つ
//!   - 接続してきたクライアントとptyの間で、双方向にバイト列を中継する
//!   - クライアントがデタッチ(切断)しても、シェル自体は動かし続ける
//!
//! I/Oの中継方法について:
//!   poll(2)で「ptyからの読み出し」「ソケットからの読み出し」を同時に
//!   監視する単一ループにしている(Milestone 2のterm.rs周りと同じ理由:
//!   スレッド+強制終了だと、途中の状態をきれいに畳めない)。

use std::io::Write;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::net::{UnixListener, UnixStream};

use nix::poll::{poll, PollFd, PollFlags, PollTimeout};

use crate::protocol::{self, ClientMessage, ServerMessage};
use crate::pty::{self, PtyProcess};
use crate::session;

/// サーバーのメインループ。呼び出したら、シェルが終了するまで戻ってこない。
/// クライアントは何度でも繋いだり切ったり(アタッチ/デタッチ)できる。
pub fn run(session_name: &str, shell_command: &[&str]) -> std::io::Result<()> {
    let socket_path = session::socket_path(session_name);

    // 前回このセッションが異常終了して、ソケットファイルの残骸だけ残って
    // いるかもしれない。実際に繋がらないなら削除してから新しくbindする。
    if UnixStream::connect(&socket_path).is_err() {
        let _ = std::fs::remove_file(&socket_path);
    }

    let listener = UnixListener::bind(&socket_path)?;
    let process = pty::spawn_in_pty(shell_command)?;

    loop {
        let (stream, _addr) = listener.accept()?;
        match handle_client(stream, &process) {
            Ok(ClientOutcome::Detached) => continue, // 次のattachを待つ
            Ok(ClientOutcome::ShellExited) => break,
            Err(e) => {
                eprintln!("mini-tmux: クライアントとの通信でエラー: {e}");
                continue;
            }
        }
    }

    // 子プロセス(シェル)は、ShellExitedに至った経路の中で
    // notify_shell_exited()が既にwaitpid済みなので、ここでは
    // fdの掃除だけ行う。
    let _ = nix::unistd::close(process.master_fd);
    let _ = std::fs::remove_file(&socket_path);
    Ok(())
}

enum ClientOutcome {
    /// クライアントが切断した(exit/Ctrl-D以外の理由も含む)。シェルは生きている。
    Detached,
    /// シェル自体が終了した。サーバーごと終了してよい。
    ShellExited,
}

/// 1クライアントとの間で、pty⇔ソケットの中継を行う。戻り値でクライアントが
/// 切断しただけなのか、シェル自体が終了したのかを呼び出し側に伝える。
fn handle_client(stream: UnixStream, process: &PtyProcess) -> std::io::Result<ClientOutcome> {
    let master_fd = process.master_fd;
    let socket_fd = stream.as_raw_fd();
    let mut socket_writer = stream.try_clone()?;

    // Safety: master_fdはPtyProcess(呼び出し元)が生存している間有効。
    // socket_fdは`stream`がこの関数の終わりまでdropされないので有効。
    let master_borrowed = unsafe { BorrowedFd::borrow_raw(master_fd) };
    let socket_borrowed = unsafe { BorrowedFd::borrow_raw(socket_fd) };

    let mut recv_buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];

    loop {
        let mut fds = [
            PollFd::new(master_borrowed, PollFlags::POLLIN),
            PollFd::new(socket_borrowed, PollFlags::POLLIN),
        ];
        match poll(&mut fds, PollTimeout::NONE) {
            Ok(_) => {}
            // シグナルで中断されただけなら、接続を切らずにやり直す。
            Err(nix::errno::Errno::EINTR) => continue,
            Err(_) => return Ok(ClientOutcome::Detached),
        }
        let master_revents = fds[0].revents().unwrap_or_else(PollFlags::empty);
        let socket_revents = fds[1].revents().unwrap_or_else(PollFlags::empty);

        if master_revents.intersects(PollFlags::POLLIN) {
            match pty::read_from_pty(master_fd, &mut chunk) {
                Ok(0) | Err(_) => {
                    // シェルが終了した。クライアントに知らせてから終わる。
                    let _ = notify_shell_exited(&mut socket_writer, process);
                    return Ok(ClientOutcome::ShellExited);
                }
                Ok(n) => {
                    let frame =
                        protocol::encode_server_message(&ServerMessage::Output(chunk[..n].to_vec()));
                    if socket_writer.write_all(&frame).is_err() {
                        return Ok(ClientOutcome::Detached);
                    }
                }
            }
        }
        if master_revents.intersects(PollFlags::POLLHUP | PollFlags::POLLERR) {
            let _ = notify_shell_exited(&mut socket_writer, process);
            return Ok(ClientOutcome::ShellExited);
        }

        if socket_revents.intersects(PollFlags::POLLIN) {
            match nix::unistd::read(socket_fd, &mut chunk) {
                Ok(0) | Err(_) => return Ok(ClientOutcome::Detached),
                Ok(n) => {
                    recv_buf.extend_from_slice(&chunk[..n]);
                    while let Some((msg, consumed)) = protocol::decode_client_message(&recv_buf) {
                        recv_buf.drain(..consumed);
                        match msg {
                            ClientMessage::Input(bytes) => {
                                let _ = pty::write_to_pty(master_fd, &bytes);
                            }
                            ClientMessage::Resize { rows, cols } => {
                                // ptyにサイズを設定すると、カーネルがシェル側へ
                                // SIGWINCHを送ってくれる(vim等はそれで再描画する)。
                                let _ = pty::resize_pty(master_fd, rows, cols);
                            }
                            ClientMessage::Detach => return Ok(ClientOutcome::Detached),
                        }
                    }
                }
            }
        }
        if socket_revents.intersects(PollFlags::POLLHUP | PollFlags::POLLERR) {
            return Ok(ClientOutcome::Detached);
        }
    }
}

/// シェルが終了したことをクライアントに1回だけ知らせる。
fn notify_shell_exited(socket_writer: &mut UnixStream, process: &PtyProcess) -> std::io::Result<()> {
    let status = nix::sys::wait::waitpid(nix::unistd::Pid::from_raw(process.child_pid), None);
    let code = match status {
        Ok(nix::sys::wait::WaitStatus::Exited(_, code)) => code,
        _ => -1,
    };
    let frame = protocol::encode_server_message(&ServerMessage::ShellExited { code });
    socket_writer.write_all(&frame)
}
