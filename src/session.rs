//! Milestone 3, 7: セッションの名前とソケットパスの対応づけ。
//!
//! 最初は「セッション名からソケットパスを決定論的に計算する」だけでよい
//! (例: /tmp/mini-tmux-<name>.sock)。複数セッションの一覧表示(`ls`)を
//! やりたくなったら、/tmp以下をディレクトリごと決めてスキャンする、と
//! いった方式に発展させる。

use std::os::unix::net::UnixStream;
use std::path::PathBuf;

/// セッション名から、そのセッションのサーバーがlistenするUnixドメイン
/// ソケットのパスを求める。
pub fn socket_path(session_name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("mini-tmux-{session_name}.sock"))
}

/// 指定したセッションのサーバーが既に起動しているか確認する。
/// ソケットファイルの有無だけでなく、実際に接続できるかで判定する
/// (サーバーが異常終了してソケットファイルだけ残っているケースがあるため)。
pub fn is_running(session_name: &str) -> bool {
    UnixStream::connect(socket_path(session_name)).is_ok()
}

/// Milestone 7: 起動中の全セッション名を列挙する。
pub fn list_sessions() -> Vec<String> {
    todo!("ソケットファイルを置くディレクトリを決めてスキャンする")
}
