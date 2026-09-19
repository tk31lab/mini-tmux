//! Milestone 3, 4, 6: クライアント側の実装。
//!
//! クライアントの責務:
//!   - サーバーのソケットに接続する
//!   - 自分の標準入力をraw modeにする(term::RawModeGuard)
//!   - 自分の標準入力から読んだバイトをそのままソケットに送る
//!   - ソケットから受け取ったバイトをそのまま自分の標準出力に書く
//!   - SIGWINCH(ウィンドウのリサイズ)を検知して、新しい端末サイズを
//!     サーバーに通知する
//!   - prefixキー(`Ctrl-b`)に続くキーをコマンドとして解釈する
//!     (`Ctrl-b d` でデタッチ)
//!
//! デタッチは、クライアントプロセスを終了させる(ターミナルを閉じる、
//! 別プロセスからkillする)ことでも起きる。サーバー/クライアントが別
//! プロセスに分離されているので、どちらの方法でもサーバー側のシェルは
//! 動き続ける。

use std::io::Write;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::net::UnixStream;

use nix::errno::Errno;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};

use crate::protocol::{self, ClientMessage, ServerMessage};
use crate::session;
use crate::term;

/// prefixキー: `Ctrl-b` (0x02)。tmuxと同じで、このキーに続く1バイトを
/// シェルへの入力ではなくmini-tmuxへのコマンドとして解釈する。
/// なお、シェル側で`Ctrl-b`(emacsキーバインドのbackward-char)を使いたい
/// 場合は、`Ctrl-b`を2回続けて押すとそのまま1つ送られる。
const PREFIX_KEY: u8 = 0x02;

/// prefixに続けて押すとデタッチするキー。
const DETACH_KEY: u8 = b'd';

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

/// 入力バイト列をprefixキーの状態機械に通し、`(シェルに転送すべきバイト列,
/// デタッチ要求か)` を返す。
///
/// `prefix_pending`は「直前にprefixキーを受け取った状態か」を表し、read()を
/// またいで保持する必要がある(`Ctrl-b`と`d`が別々のread()で届くことがある)。
/// prefixキー自体は、次の1バイトが確定するまで転送を保留する。
fn process_input(chunk: &[u8], prefix_pending: &mut bool) -> (Vec<u8>, bool) {
    let mut forward = Vec::with_capacity(chunk.len());

    for &byte in chunk {
        if *prefix_pending {
            *prefix_pending = false;
            match byte {
                DETACH_KEY => return (forward, true),
                // Ctrl-bを2回続けた場合は、Ctrl-b自体をシェルに送る。
                PREFIX_KEY => forward.push(PREFIX_KEY),
                // 割り当てのないキーなら、保留していたprefixごと送る。
                other => forward.extend_from_slice(&[PREFIX_KEY, other]),
            }
        } else if byte == PREFIX_KEY {
            *prefix_pending = true;
        } else {
            forward.push(byte);
        }
    }

    (forward, false)
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
    // prefixキーを受け取った直後かどうか。read()をまたいで保持する。
    let mut prefix_pending = false;

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
                    let (forward, detach) = process_input(&chunk[..n], &mut prefix_pending);

                    if !forward.is_empty() {
                        let frame =
                            protocol::encode_client_message(&ClientMessage::Input(forward));
                        if socket_writer.write_all(&frame).is_err() {
                            break;
                        }
                    }

                    if detach {
                        let frame = protocol::encode_client_message(&ClientMessage::Detach);
                        let _ = socket_writer.write_all(&frame);
                        // raw mode中なので改行は\r\nで送る必要がある。
                        print!("\r\n[mini-tmux] デタッチしました\r\n");
                        let _ = std::io::stdout().flush();
                        return Ok(());
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 状態を持たない普通の入力は、そのまま素通しされる。
    #[test]
    fn passes_through_normal_input() {
        let mut pending = false;
        let (forward, detach) = process_input(b"ls -l\n", &mut pending);
        assert_eq!(forward, b"ls -l\n");
        assert!(!detach);
        assert!(!pending);
    }

    /// prefix + d でデタッチ要求になる。
    #[test]
    fn detects_detach_sequence() {
        let mut pending = false;
        let (forward, detach) = process_input(&[PREFIX_KEY, DETACH_KEY], &mut pending);
        assert!(forward.is_empty());
        assert!(detach);
    }

    /// prefixとコマンドが別々のread()で届いても検出できる。
    #[test]
    fn detects_detach_across_reads() {
        let mut pending = false;

        let (forward, detach) = process_input(&[PREFIX_KEY], &mut pending);
        assert!(forward.is_empty());
        assert!(!detach);
        assert!(pending, "prefixを受け取った状態が次のread()まで保持される");

        let (forward, detach) = process_input(&[DETACH_KEY], &mut pending);
        assert!(forward.is_empty());
        assert!(detach);
    }

    /// デタッチ前に入力されていた分は、取りこぼさず転送する。
    #[test]
    fn forwards_bytes_before_detach() {
        let mut pending = false;
        let (forward, detach) = process_input(b"abc\x02d", &mut pending);
        assert_eq!(forward, b"abc");
        assert!(detach);
    }

    /// prefixを2回続けると、prefixキー自体が1つシェルに送られる。
    #[test]
    fn double_prefix_sends_literal_prefix() {
        let mut pending = false;
        let (forward, detach) = process_input(&[PREFIX_KEY, PREFIX_KEY], &mut pending);
        assert_eq!(forward, [PREFIX_KEY]);
        assert!(!detach);
        assert!(!pending);
    }

    /// 割り当てのないキーが続いた場合は、保留していたprefixごと転送する。
    #[test]
    fn unknown_command_forwards_prefix_and_key() {
        let mut pending = false;
        let (forward, detach) = process_input(&[PREFIX_KEY, b'z'], &mut pending);
        assert_eq!(forward, [PREFIX_KEY, b'z']);
        assert!(!detach);
    }

    /// prefixだけで終わった場合、そのバイトは次の入力が来るまで保留される。
    #[test]
    fn lone_prefix_is_held_back() {
        let mut pending = false;
        let (forward, _) = process_input(&[PREFIX_KEY], &mut pending);
        assert!(forward.is_empty());
        assert!(pending);
    }
}
