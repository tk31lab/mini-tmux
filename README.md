# mini-tmux (Rust)

`spec.md` の仕様に対応するRust実装。`spec.md` のマイルストーン順に、対応する
ファイルの `todo!()` を一つずつ実装で置き換えていく方式で進めている。

現在 Milestone 5 まで実装済みで、セッションの作成・デタッチ・再アタッチが
動作する。残りの作業は `grep -rn "todo!" src/` で一覧できる(コードと乖離
しないので、これをそのまま残作業リストとして使う)。

## ビルド・実行

```
cd mini-tmux
cargo build
cargo run -- new -s work      # セッションを作ってアタッチ
cargo run -- attach -s work   # 既存セッションに再アタッチ
cargo run -- ls               # 起動中のセッション一覧
cargo run -- kill-session -s work  # セッションを終了させる
```

`kill-session` は、別のターミナルがアタッチ中のセッションには使えない
(サーバーが同時に1クライアントしか相手にしないため)。その場合はそちらで
`exit` するか、デタッチしてから実行する。

セッション内でのキー操作:

| キー | 動作 |
| --- | --- |
| `Ctrl-b d` | デタッチする(シェルは裏で動き続ける) |
| `Ctrl-b Ctrl-b` | `Ctrl-b` 自体をシェルに送る |
| それ以外 | すべてそのままシェルに渡る(`Ctrl-C` なども含む) |

Rust未導入の場合は https://rustup.rs からインストールする。初回 `cargo build`
はネットワーク経由で依存クレートを取得する。

## ファイルとマイルストーンの対応

| ファイル | 対応するマイルストーン | 役割 |
| --- | --- | --- |
| `src/pty.rs` | 1 | ptyの作成とシェルの起動 |
| `src/term.rs` | 2 | 自分の標準入出力をraw modeにする |
| `src/protocol.rs` | 3, 6 | クライアント/サーバー間のメッセージ形式 |
| `src/session.rs` | 3, 7 | セッション名とソケットパスの対応づけ、一覧管理 |
| `src/server.rs` | 3, 4 | サーバー側: pty⇔ソケットの中継 |
| `src/client.rs` | 3, 4, 6 | クライアント側: 入出力の中継、デタッチ、リサイズ通知 |
| `src/daemon.rs` | 4 | サーバーをバックグラウンドプロセス化する |
| `src/main.rs` | - | サブコマンド(`new` / `attach` / `ls`)の振り分け |

## 依存クレートについて

`nix` と `signal-hook` は Cargo.toml に入れてあるが、コード中ではまだ
`use` していない(スケルトンが常にコンパイルできるようにするため)。各
`todo!()` を実装するタイミングで、`nix::pty::openpty` や
`nix::sys::termios` などを実際にimportして使う。nixはバージョンで
APIやfeature名が変わることがあるので、詰まったらそのバージョンの
https://docs.rs/nix を確認するとよい。

## 動作確認のコツ

Milestone 1, 2 くらいまでは `cargo run` で直接シェルが手元で動くはずなので、
`ls` や `vim` など普通のコマンドを打って壊れないか確認する。Milestone 3
以降はサーバー/クライアントの2プロセスに分かれるので、ターミナルを2枚開いて
`cargo run -- new -s work` と `cargo run -- attach -s work` を別々に
試すとデバッグしやすい。

デーモン(サーバー)は端末から切り離されているので、`cargo run` を終了しても
残り続ける。状態を確認したいときは以下が使える。

```
ps aux | grep mini-tmux          # サーバープロセスが生きているか
ls -l "$TMPDIR"mini-tmux-*.sock  # セッションのソケット(= 生きているセッション)
```

## GitHubへの公開手順

現状はローカルのGitリポジトリのみで、GitHubには接続していない。公開したく
なったら以下のどちらかを行う。ローカルのコミット履歴はそのまま全部push
されるので、後から繋いでも何も失われない。

### 方法1: GitHub CLI (`gh`) を使う

リポジトリの作成・remote登録・pushを1コマンドで済ませられる。

```
gh repo create mini-tmux --private --source=. --push
```

- `--private` を `--public` にすれば公開リポジトリになる
- 初回は `gh auth login` でのログインが必要

### 方法2: ブラウザで作ってから繋ぐ

1. github.com で空のリポジトリを作る(README等は追加しない)
2. 表示されたURLをremoteとして登録してpushする

```
git remote add origin https://github.com/<ユーザー名>/mini-tmux.git
git push -u origin main
```

### 注意

コミットした内容は履歴に永久に残る。後からファイルを削除しても過去の
コミットには残り続けるので、秘密情報は最初からコミットしないこと。
ビルド成果物の `target/` は `.gitignore` で除外済み。
