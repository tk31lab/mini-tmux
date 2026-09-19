//! Milestone 3, 7: セッションの名前とソケットパスの対応づけ。
//!
//! 最初は「セッション名からソケットパスを決定論的に計算する」だけでよい
//! (例: /tmp/mini-tmux-<name>.sock)。複数セッションの一覧表示(`ls`)を
//! やりたくなったら、/tmp以下をディレクトリごと決めてスキャンする、と
//! いった方式に発展させる。

use std::os::unix::net::UnixStream;
use std::path::PathBuf;

/// ソケットファイル名の前後に付ける固定部分。socket_path()とlist_sessions()の
/// 両方で使うので、片方だけ変えてしまわないよう定数にしている。
const SOCKET_PREFIX: &str = "mini-tmux-";
const SOCKET_SUFFIX: &str = ".sock";

/// セッション名から、そのセッションのサーバーがlistenするUnixドメイン
/// ソケットのパスを求める。
pub fn socket_path(session_name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("{SOCKET_PREFIX}{session_name}{SOCKET_SUFFIX}"))
}

/// 指定したセッションのサーバーが既に起動しているか確認する。
/// ソケットファイルの有無だけでなく、実際に接続できるかで判定する
/// (サーバーが異常終了してソケットファイルだけ残っているケースがあるため)。
pub fn is_running(session_name: &str) -> bool {
    UnixStream::connect(socket_path(session_name)).is_ok()
}

/// Milestone 7: 起動中の全セッション名を列挙する。
///
/// セッションの実体は「そのパスにbindして待ち受けているサーバープロセス」
/// なので、名簿にあたるものはファイルシステムそのもの。ソケットを置いて
/// いるディレクトリを走査して名前を拾い、実際に接続できるものだけを返す
/// (サーバーが異常終了してソケットファイルだけ残っている場合を除くため)。
pub fn list_sessions() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return Vec::new();
    };

    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let file_name = entry.file_name();
            let name = file_name
                .to_str()?
                .strip_prefix(SOCKET_PREFIX)?
                .strip_suffix(SOCKET_SUFFIX)?;
            is_running(name).then(|| name.to_string())
        })
        .collect();

    names.sort();
    names
}
