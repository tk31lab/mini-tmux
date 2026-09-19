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

/// アタッチしてきたクライアントに送り直す、直近のpty出力の保持量。
///
/// 端末のサイズや内容によるが、だいたい1画面分をカバーできる程度にしている。
const REPLAY_BUFFER_BYTES: usize = 8 * 1024;

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

    // クライアントが繋がっていない間もptyの出力は流れてくる。アタッチして
    // きたクライアントに直近の内容を送り直せるよう、ここで保持しておく。
    let mut recent_output: Vec<u8> = Vec::new();

    loop {
        let stream = match wait_for_client(&listener, &process, &mut recent_output)? {
            WaitOutcome::ClientArrived(stream) => stream,
            WaitOutcome::ShellExited => break,
        };

        match handle_client(stream, &process, &mut recent_output) {
            Ok(ClientOutcome::Detached) => continue, // 次のattachを待つ
            Ok(ClientOutcome::ShellExited) => break,
            Err(e) => {
                eprintln!("mini-tmux: クライアントとの通信でエラー: {e}");
                continue;
            }
        }
    }

    // シェルがクライアント接続中に終了した場合は notify_shell_exited() が
    // 既にwaitpid済み。デタッチ中に終了した場合はここが初回になる
    // (二重に呼んでもECHILDが返るだけなので、区別せず呼ぶ)。
    let _ = nix::sys::wait::waitpid(nix::unistd::Pid::from_raw(process.child_pid), None);
    let _ = nix::unistd::close(process.master_fd);
    let _ = std::fs::remove_file(&socket_path);
    Ok(())
}

enum WaitOutcome {
    ClientArrived(UnixStream),
    /// クライアントが繋がっていない間にシェルが終了した。
    ShellExited,
}

/// クライアントの接続を待つ。ただし待っている間も、ptyの出力を読み続ける。
///
/// 単に accept() でブロックしてしまうと、デタッチ中は誰もptyを読まなくなる。
/// するとカーネルのptyバッファがすぐ詰まり、画面に出力しようとしたコマンドが
/// write()でブロックして止まってしまう(「長いビルドを流してデタッチ、後で
/// 戻ったら終わっている」が成立しなくなる)。本家tmuxもサーバーは常にptyを
/// 読み続けている。
///
/// 読んだ内容は送る相手がいないので、再送用のバッファに溜めるだけ。結果として
/// デタッチ中の出力も、再アタッチしたときにある程度見えるようになる。
fn wait_for_client(
    listener: &UnixListener,
    process: &PtyProcess,
    recent_output: &mut Vec<u8>,
) -> std::io::Result<WaitOutcome> {
    let master_fd = process.master_fd;

    // Safety: どちらのfdも呼び出し元(run)が生存させている。
    let master_borrowed = unsafe { BorrowedFd::borrow_raw(master_fd) };
    let listener_borrowed = unsafe { BorrowedFd::borrow_raw(listener.as_raw_fd()) };

    let mut chunk = [0u8; 4096];

    loop {
        let mut fds = [
            PollFd::new(master_borrowed, PollFlags::POLLIN),
            PollFd::new(listener_borrowed, PollFlags::POLLIN),
        ];
        match poll(&mut fds, PollTimeout::NONE) {
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => return Err(e.into()),
        }
        let master_revents = fds[0].revents().unwrap_or_else(PollFlags::empty);
        let listener_revents = fds[1].revents().unwrap_or_else(PollFlags::empty);

        if master_revents.intersects(PollFlags::POLLIN) {
            match pty::read_from_pty(master_fd, &mut chunk) {
                Ok(0) | Err(_) => return Ok(WaitOutcome::ShellExited),
                Ok(n) => remember_output(recent_output, &chunk[..n]),
            }
        }
        if master_revents.intersects(PollFlags::POLLHUP | PollFlags::POLLERR) {
            return Ok(WaitOutcome::ShellExited);
        }

        if listener_revents.intersects(PollFlags::POLLIN) {
            let (stream, _addr) = listener.accept()?;
            return Ok(WaitOutcome::ClientArrived(stream));
        }
    }
}

enum ClientOutcome {
    /// クライアントが切断した(exit/Ctrl-D以外の理由も含む)。シェルは生きている。
    Detached,
    /// シェル自体が終了した。サーバーごと終了してよい。
    ShellExited,
}

/// 1クライアントとの間で、pty⇔ソケットの中継を行う。戻り値でクライアントが
/// 切断しただけなのか、シェル自体が終了したのかを呼び出し側に伝える。
fn handle_client(
    stream: UnixStream,
    process: &PtyProcess,
    recent_output: &mut Vec<u8>,
) -> std::io::Result<ClientOutcome> {
    let master_fd = process.master_fd;
    let socket_fd = stream.as_raw_fd();
    let mut socket_writer = stream.try_clone()?;

    // 直近の出力を送り直す。これをしないと、再アタッチしても画面は空白の
    // ままになる(シェルは既にプロンプトを出力済みで、それは前のクライアント
    // に送られてしまっているため、こちらから何か入力するまで何も届かない)。
    if !recent_output.is_empty() {
        let frame = protocol::encode_server_message(&ServerMessage::Output(recent_output.clone()));
        if socket_writer.write_all(&frame).is_err() {
            return Ok(ClientOutcome::Detached);
        }
    }

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
                    remember_output(recent_output, &chunk[..n]);

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
                                // ptyにサイズを設定すると、サイズが実際に
                                // 変化した場合に限りカーネルがシェル側へ
                                // SIGWINCHを送る(vim等はそれで再描画する)。
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

/// 直近のpty出力を、上限を超えた分だけ先頭から捨てつつ覚えておく。
fn remember_output(buffer: &mut Vec<u8>, bytes: &[u8]) {
    buffer.extend_from_slice(bytes);

    if buffer.len() > REPLAY_BUFFER_BYTES {
        buffer.drain(..buffer.len() - REPLAY_BUFFER_BYTES);
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
