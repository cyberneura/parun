# parun 開発メモ

シェルコマンドを並列実行し、端末ではワーカーごとのペインに出力の末尾を見せる Rust 製 CLI。
仕様と使い方は README.md を参照。ここには開発上の判断と注意点だけを書く。

[pullkit](https://github.com/cyberneura/pullkit) の `sync` 画面 (`sync_screen.rs`) と
プロセス管理 (`pullkit-core` の spawn / シグナル / 後始末) を、git 固有の部分を外して
単独コマンドにしたもの。設計上の約束の多くは pullkit から引き継いでいる。

## 言語の方針

public リポジトリなので、**README・コードコメント・UI 文字列・エラーメッセージはすべて英語**で書く。
**この AGENTS.md だけは日本語**。エージェント向けの開発メモで、他リポジトリでも
「ユーザー向け表示は英語、開発者向けドキュメントは日本語」で揃えているため。

## 構成

| パス | 内容 |
|---|---|
| `src/main.rs` | CLI 引数、コマンド一覧の収集 (引数 / `--file` / stdin)、plain 出力、Summary、終了コード |
| `src/runner.rs` | `Job` / `Event` / `JobResult`。ワーカースレッドで回し、イベントを呼び出しスレッドへ流す |
| `src/process.rs` | 1 コマンドの起動と行ストリーミング、プロセスグループの追跡、Ctrl-C 2 段階、後始末 |
| `src/screen.rs` | ペイン画面。フレーム組み立て (`Screen::frame`) と描画・キー入力 (`run` / `watch`) |
| `src/terminal.rs` | raw mode + alternate screen の出入り、SIGHUP / SIGTERM / SIGINT での後始末 |
| `src/text.rs` | 端末セル幅の計測・切り詰め、エスケープシーケンスと制御文字の除去 |

## 開発コマンド

```bash
cargo run -- 'sleep 1; echo a' 'echo b; exit 1'   # ペイン画面
cargo run -- --plain 'echo a' 'echo b'              # 端末でも plain 出力
cargo test
cargo clippy --all-targets --locked -- -D warnings
cargo fmt --all
```

`.j-menu.yaml` からデモ実行 / check / release を起動できる。

## 検証の方法

**E2E の前に必ず `cargo build` する。** `cargo test` / `cargo clippy` は `target/debug/parun` を
再リンクしないので、修正後に build せず実バイナリを触ると旧挙動を見る。

**PATH の `parun` は Homebrew の cask で、手元のビルドとは別物。** 検証は `./target/debug/parun` の
ようにパスを指定して行う。

### plain 出力

stdout をパイプにつなげば plain 経路になる (`./target/debug/parun ... | cat`)。stdin も端末でない
ことが条件なので、Claude Code の Bash ツールから実行した時は何もしなくても plain になる。

### ペイン画面

pty 経由で実画面を取れる。Python の `pty.fork()` で起動し、`TIOCSWINSZ` でサイズを与え、
出力からエスケープシーケンスを落として行を読む。「press any key」が出たらキーを送る。Ctrl-C は
`\x03` を書けば raw mode のキー入力として届く。中断の検証では、終了後に `pgrep -fl "^sleep 30"`
で子が残っていないことまで見る。

## 設計上の約束

- **端末が Ctrl-C を届けられないコマンドは自分のプロセスグループで走らせる** (`Isolation::OwnGroup`)。
  raw mode の画面では Ctrl-C はシグナルにならずキー入力になるので、画面側が
  `interrupt_running_commands` (SIGINT) を送り、2 度目で `terminate_running_commands` (SIGKILL) を
  送る。画面が自分の都合で抜ける時は `stop_running_commands` (SIGINT → 猶予 → SIGKILL)。
  stdin か stdout が端末でない plain 経路だけが共有グループ (`SharedGroup`) で、パイプ先の端末で
  打った Ctrl-C がそのまま届く。
- **端末が消えた時 (SIGHUP) と SIGTERM は自前で受けて `stop_running_commands` してから終わる**
  (`terminal::stop_commands_on_hangup`)。自グループの子には端末の hangup が届かず、デフォルト動作で
  parun だけが先に死ぬと丸ごと残る。stop の後は `exit()` ではなく同じシグナルで自分を殺す
  (`emulate_default_handler`)。bash はスクリプトを止めるかを子が SIGINT で死んだかで判断するので、
  `exit(130)` に戻すと plain 経路を呼ぶスクリプトが Ctrl-C で止まらなくなる。
- **中断 (`stop_running_commands`) と後始末 (`stop_leftover_commands`) は分ける**。前者は
  `ABANDONED` を立てるので、そのプロセスではもう run を始められない。run が終わった後にコマンドが
  残したプロセスを止めるのは後者。
- **`ABANDONED` は立てたら戻さない**。中断は必ずプロセス終了で終わる前提で、run の開始時に戻すと
  遅れて動き出したワーカースレッドが直前の中断を打ち消す。
- **プロセスグループは中身が残っている間 `RUNNING_COMMANDS` に残し、シグナルを送る前に空になった
  ものを外す**。コマンドが終わってもグループに中身が残っていれば `LEFTOVER_GROUPS` に移す。
  空になったグループの id は別プロセスに再利用されうるので、`killpg(pgid, 0)` で確かめてから送る。
  シグナルは `kill` バイナリではなく libc で送り、リストの Mutex は送る前に離す。
- **spawn から登録までは `SPAWNING` で数え、stop はそれが 0 になるまで待つ。** 登録前に来た中断は
  登録直後に自分で kill して答える。
- **パイプの行長 (`MAX_LINE_BYTES`) とチャネル (`PIPE_QUEUE` / `EVENT_QUEUE`) は有界**。改行の無い
  出力や描画が追いつかない UI でメモリが伸びないように、溢れたら子プロセスの write を待たせる。
- **出力は行単位でストリームする**。`run_jobs` はワーカー数ぶんのスレッドで回し、イベントは
  呼び出しスレッドで `on_event` に渡す (描画は 1 スレッドで済む)。まとめて出すとペインが
  長いビルドの間ずっと空のままになる。
- **ペインに入れる行はエスケープシーケンスと制御文字を落とす** (`text::plain_text`)。色やカーソル
  移動はペインの中では再現できず、幅計算も狂う。タブはタブストップまでのスペースに置き換える。
  plain 経路はそのまま流す (端末が解釈できる)。
- **桁を数える時は文字数ではなく端末のセル数を使う**。`display_width` / `truncate_to_width` /
  `pad_to_width` を使い、`chars().count()` や `{:<20}` で幅を扱わない。切り詰めは書記素クラスタ境界で
  行い、絵文字を途中で割らない。
- **フレームの最終行だけ幅いっぱいまで埋めない**。右下隅に文字を置くとスクロールする端末がある。
- **コマンドは `sh -c` で起動する**。`make` / `xargs` / `find -exec` と同じ慣習で、`$SHELL` は
  使わない (fish 等で構文が変わる)。stdin は `null`。
- **plain 経路の `print_line` は書けなくなったら即終了する**。`head` のように読み手が先に閉じた時、
  `println!` は panic し、その panic は全ワーカーの終了を待ってしまう。

## リリース

`main` の `Cargo.toml` の version を変えて push するとリリースされる (`.github/workflows/release.yml`)。
`plan` ジョブが「その version の Release が公開済みか」を releases API に訊き、404 の時だけ
test → build → release へ進む。判定は diff ではなく状態なので、失敗した run は原因を直して
push すれば続きから走る。構成は `cyberneura/runandlog` と同じ (`release-rust-cli` スキル参照)。

- 採番と push は `scripts/release.sh [patch|minor|major]` (既定 minor)。
- build は macOS arm64 (Developer ID 署名 + notarytool 公証) と Linux x86_64。
- 署名の Secrets (APPLE_*) は `~/home-files/sh/github-secret/deploy-github-secret-apple-building.sh`
  の `repos` に `cyberneura/parun` を入れて実行すると 1Password から配られる。欠けていると build が
  最初のステップで落ちる (黙って未署名で出さない)。
- crates.io への publish はリポジトリ変数 `PUBLISH_CRATES=true` と secret `CARGO_REGISTRY_TOKEN` が
  揃った時だけ走る。
- Homebrew は `cyberneura/homebrew-tap` の `Casks/parun.rb`。tap 側の `scripts/update.py` が毎時
  latest release を見て version / url / sha256 を書き換えるので、このリポジトリから tap へ push しない。
- `test.yml` は PR でも走る (fmt / clippy -D warnings / test)。release からは `workflow_call` で
  同じ定義を呼ぶ。コンパイラは `rust-toolchain.toml` で固定してある。
