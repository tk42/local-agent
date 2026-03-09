# Local Agent

ローカルの llama-server (llama.cpp) で動く Claude Code 風コーディングエージェント CLI。

[shareAI-lab/learn-claude-code](https://github.com/shareAI-lab/learn-claude-code) を参考に、OpenAI 互換 API で実装。Rust 製シングルバイナリで Python 環境不要。

## 前提条件

- Rust 1.85+ (ビルドのみ。実行にはランタイム不要)
- llama-server が `--jinja` 付きで起動済み (tool calling に必要)
- ripgrep (`rg`) がインストール済み (grep_search ツール用、なければ grep にフォールバック)

## ビルド

```bash
# デバッグビルド
cargo build

# リリースビルド (最適化 + strip 済み、~1.6MB)
cargo build --release

# バイナリの場所
./target/release/local-agent
```

## セットアップ

```bash
cp .env.example .env
# .env を必要に応じて編集 (LLM_BASE_URL, LLM_API_KEY, LLM_MODEL 等)
```

### 環境変数

| 変数名            | デフォルト値               | 説明                     |
| ----------------- | -------------------------- | ------------------------ |
| `LLM_BASE_URL`    | `http://localhost:8080/v1` | OpenAI互換APIのベースURL |
| `LLM_API_KEY`     | `sk-no-key-required`       | APIキー                  |
| `LLM_MODEL`       | `qwen3.5`                  | モデル名                 |
| `LLM_MAX_TOKENS`  | `8192`                     | 最大トークン数           |
| `LLM_TEMPERATURE` | `0.6`                      | Temperature              |

## 使い方

### インタラクティブ REPL

```bash
./target/release/local-agent
```

### ワンショット

```bash
./target/release/local-agent "このプロジェクトの構造を教えて"
```

### REPL コマンド

| コマンド     | 説明                   |
| ------------ | ---------------------- |
| `/compact`   | コンテキストを強制圧縮 |
| `/todos`     | 現在の Todo リスト表示 |
| `/tokens`    | 推定トークン数を表示   |
| `/clear`     | 会話履歴をクリア       |
| `/help`      | ヘルプ表示             |
| `q` / `exit` | 終了                   |

## ツール一覧

| ツール           | 説明                          |
| ---------------- | ----------------------------- |
| `bash`           | シェルコマンド実行            |
| `read_file`      | ファイル読み取り (行番号付き) |
| `write_file`     | ファイル書き込み              |
| `edit_file`      | テキスト置換による編集        |
| `list_directory` | ディレクトリ一覧              |
| `grep_search`    | ripgrep によるコード検索      |
| `todo_write`     | タスクリスト管理              |

## アーキテクチャ

```
src/
├── main.rs           エントリポイント + REPL + agent_loop
├── llm_client.rs     OpenAI互換HTTPクライアント (SSEストリーミング対応)
├── tools.rs          ツール定義 + ハンドラ
├── todo_manager.rs   TodoWrite ツール
└── context.rs        コンテキスト圧縮 (microcompact + auto_compact)
```

### コアパターン

```
User → messages[] → LLM → response
                           │
                    tool_calls あり?
                    ├─ Yes → ツール実行 → 結果を messages に追加 → ループ
                    └─ No  → テキスト返却 → 終了
```

## クロスコンパイル

```bash
# cross をインストール
cargo install cross

# macOS ARM64 (Apple Silicon)
cross build --release --target aarch64-apple-darwin

# Linux ARM64
cross build --release --target aarch64-unknown-linux-gnu

# Linux x86_64
cross build --release --target x86_64-unknown-linux-gnu
```

## 旧 Python 版

元の Python 実装は `python/` ディレクトリに保存されています。
