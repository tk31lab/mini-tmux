//! Milestone 3, 6: クライアント/サーバー間でUnixドメインソケット越しに
//! やり取りするメッセージの型。
//!
//! フレーミングは単純に「1バイトのタグ + 4バイトのlittle-endian長さ +
//! 本体」。ソケットはストリームなので、1回のread()でメッセージが
//! 半端に届くこともある。そのため decode_* は「バッファの先頭に1メッセージ
//! 分そろっていればSomeを返し、消費したバイト数も一緒に返す」設計にして
//! いる。呼び出し側は、消費した分だけバッファから取り除いてから、また
//! decodeを試す(残りが半端なら次のreadを待つ)。

/// クライアントからサーバーへ送るメッセージ。
pub enum ClientMessage {
    /// 生の入力バイト列(キー入力)をそのままptyに渡してほしい、という指示。
    Input(Vec<u8>),
    /// Milestone 6: 自分のターミナルのサイズが変わった、という通知。
    Resize { rows: u16, cols: u16 },
    /// デタッチする(サーバー側のシェルは動かしたまま切断する)。
    /// (専用キーシーケンスでの送信はまだ未実装。プロトコル上は用意済み)
    Detach,
}

/// サーバーからクライアントへ送るメッセージ。
pub enum ServerMessage {
    /// ptyからの生の出力バイト列。
    Output(Vec<u8>),
    /// サーバー側でシェルが終了した、という通知。
    ShellExited { code: i32 },
}

const TAG_INPUT: u8 = 1;
const TAG_RESIZE: u8 = 2;
const TAG_DETACH: u8 = 3;

const TAG_OUTPUT: u8 = 1;
const TAG_SHELL_EXITED: u8 = 2;

/// tag(1バイト) + 長さ(4バイト, little-endian) + 本体、の順でフレームを作る。
fn encode_frame(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + 4 + body.len());
    out.push(tag);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    out
}

/// バッファの先頭に1フレーム分そろっていれば `(tag, 本体, 消費バイト数)` を返す。
/// 足りなければNone(呼び出し側は次のreadを待ってからもう一度呼ぶ)。
fn decode_frame(buf: &[u8]) -> Option<(u8, &[u8], usize)> {
    if buf.len() < 5 {
        return None;
    }
    let tag = buf[0];
    let len = u32::from_le_bytes(buf[1..5].try_into().unwrap()) as usize;
    let total = 5 + len;
    if buf.len() < total {
        return None;
    }
    Some((tag, &buf[5..total], total))
}

/// `ClientMessage`をバイト列にシリアライズする。
pub fn encode_client_message(msg: &ClientMessage) -> Vec<u8> {
    match msg {
        ClientMessage::Input(bytes) => encode_frame(TAG_INPUT, bytes),
        ClientMessage::Resize { rows, cols } => {
            let mut body = Vec::with_capacity(4);
            body.extend_from_slice(&rows.to_le_bytes());
            body.extend_from_slice(&cols.to_le_bytes());
            encode_frame(TAG_RESIZE, &body)
        }
        ClientMessage::Detach => encode_frame(TAG_DETACH, &[]),
    }
}

/// バイト列から`ClientMessage`を読み取る。
pub fn decode_client_message(buf: &[u8]) -> Option<(ClientMessage, usize)> {
    let (tag, body, consumed) = decode_frame(buf)?;
    let msg = match tag {
        TAG_INPUT => ClientMessage::Input(body.to_vec()),
        TAG_RESIZE if body.len() == 4 => ClientMessage::Resize {
            rows: u16::from_le_bytes([body[0], body[1]]),
            cols: u16::from_le_bytes([body[2], body[3]]),
        },
        TAG_DETACH => ClientMessage::Detach,
        // 未知のタグや壊れたフレームは読み飛ばせないので、消費なしのNoneを
        // 返す。呼び出し側はこれ以上進めないので、接続を切るのが安全。
        _ => return None,
    };
    Some((msg, consumed))
}

/// `ServerMessage`をバイト列にシリアライズする。
pub fn encode_server_message(msg: &ServerMessage) -> Vec<u8> {
    match msg {
        ServerMessage::Output(bytes) => encode_frame(TAG_OUTPUT, bytes),
        ServerMessage::ShellExited { code } => encode_frame(TAG_SHELL_EXITED, &code.to_le_bytes()),
    }
}

/// バイト列から`ServerMessage`を読み取る。
pub fn decode_server_message(buf: &[u8]) -> Option<(ServerMessage, usize)> {
    let (tag, body, consumed) = decode_frame(buf)?;
    let msg = match tag {
        TAG_OUTPUT => ServerMessage::Output(body.to_vec()),
        TAG_SHELL_EXITED if body.len() == 4 => ServerMessage::ShellExited {
            code: i32::from_le_bytes(body.try_into().unwrap()),
        },
        _ => return None,
    };
    Some((msg, consumed))
}
