//! Milestone 2: 自分の標準入出力をraw modeにする。
//!
//! なぜ必要か: 通常のターミナルはcanonical mode(行単位でEnterまで入力を
//! バッファし、Ctrl-CやBackspaceを自分で処理してしまう)になっている。
//! tmuxクライアントはキー入力を1バイトずつそのまま向こう側のシェルに
//! 渡したいので、自分のstdinをraw modeにする必要がある。
//!
//! 参考API(nixクレート、feature "term"):
//!   - nix::sys::termios::tcgetattr(fd) で現在の設定を取得
//!   - nix::sys::termios::cfmakeraw(&mut termios) でraw mode相当のフラグに変更
//!   - nix::sys::termios::tcsetattr(fd, SetArg::TCSANOW, &termios) で適用

use std::os::fd::{BorrowedFd, RawFd};

use nix::sys::termios::{self, SetArg, Termios};

/// raw modeにする前の設定を保持しておき、Dropで元に戻すためのガード。
/// プログラムがどこで終了しても(panicも含めて)ターミナルの設定を元に
/// 戻せるように、RAIIパターンで実装する。
pub struct RawModeGuard {
    fd: RawFd,
    original: Termios,
}

impl RawModeGuard {
    /// `fd`(通常は標準入力の0)をraw modeにし、そのガードを返す。
    pub fn enable(fd: RawFd) -> std::io::Result<Self> {
        // Safety: `fd`は呼び出し側が生きている間保持する責任を持つ。
        // ここでは値を借用するだけで、closeなどはしない。
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };

        // 元の設定を保存しておく(Dropで戻すため)。
        let original = termios::tcgetattr(borrowed)?;

        // cfmakerawは、ICANON/ECHO/ISIGなどをまとめて落として、
        // 1バイトずつ生のデータがそのまま読める状態にする。
        // (ISIGが落ちるので、Ctrl-Cはここでは処理されず、そのままバイトとして
        //  read()に渡ってくる。それをptyに転送すれば、シェル側のterminal設定が
        //  代わりにSIGINTを解釈してくれる)
        let mut raw = original.clone();
        termios::cfmakeraw(&mut raw);
        termios::tcsetattr(borrowed, SetArg::TCSANOW, &raw)?;

        Ok(Self { fd, original })
    }
}

impl Drop for RawModeGuard {
    /// スコープを抜けるとき(通常終了・panicどちらでも)に、保存しておいた
    /// 元の設定に戻す。
    fn drop(&mut self) {
        let borrowed = unsafe { BorrowedFd::borrow_raw(self.fd) };
        // Dropの中でエラーを伝える手段がないので、失敗しても握りつぶす。
        let _ = termios::tcsetattr(borrowed, SetArg::TCSANOW, &self.original);
    }
}

/// Milestone 6: `fd`が指す端末の現在のサイズを `(行数, 列数)` で返す。
///
/// 端末のサイズはカーネルがttyごとに保持していて、TIOCGWINSZ ioctlで
/// 問い合わせる。ウィンドウがリサイズされると、カーネルがこの値を更新した
/// 上でSIGWINCHを送ってくるので、受け取った側は改めてこれを呼んで新しい
/// サイズを知る、という流れになる。
pub fn window_size(fd: RawFd) -> std::io::Result<(u16, u16)> {
    let mut winsize: nix::libc::winsize = unsafe { std::mem::zeroed() };

    if unsafe { nix::libc::ioctl(fd, nix::libc::TIOCGWINSZ as _, &mut winsize) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((winsize.ws_row, winsize.ws_col))
}
