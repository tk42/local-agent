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
| `LLM_MODEL`       | `any-model-name`           | モデル名（任意の文字列） |
| `LLM_MAX_TOKENS`  | `32768`                    | 最大トークン数           |
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

| コマンド     | 説明                                     |
| ------------ | ---------------------------------------- |
| `/compact`   | コンテキストを強制圧縮                   |
| `/todos`     | 現在の Todo リスト表示                   |
| `/tokens`    | 推定トークン数を表示                     |
| `/skills`    | ロード済みスキル一覧                     |
| `/plan`      | Plan モードの ON/OFF（Shift+Tab と同じ） |
| `/clear`     | 会話履歴をクリア                         |
| `/help`      | ヘルプ表示                               |
| `q` / `exit` | 終了                                     |

| キー        | 説明                                                                  |
| ----------- | --------------------------------------------------------------------- |
| `Shift+Tab` | Plan モードのトグル（端末が backtab を送らない場合は `/plan` を使う） |

## Plan モード

実装に手をつける前にまず計画を立てさせるためのモード。Claude Code の Plan モード相当。

- **トグル**: `Shift+Tab` または `/plan`
- **ON のとき**:
  - システムプロンプトに「調査して Markdown の計画を出せ。書き換え系ツールは使うな」を注入
  - `bash` / `write_file` / `edit_file` を **ハードガード**で拒否（モデルが呼んでもエラー結果が返るだけで実行されない）
  - プロンプトが `[PLAN] >>> ` に変わる
- **OFF のとき**: 通常モード（全ツール許可）
- ワンショット (`local-agent "..."`) では無効（対話で切替不能のため常に OFF）

## Skills

Claude Code 風のローカルスキル（`SKILL.md`）をオンデマンドで読ませられる。

### 配置

バイナリと同じディレクトリの `skills/<name>/SKILL.md`（開発時は CWD の `./skills/` も探索）:

```
./local-agent
./skills/
├── git-commit/
│   └── SKILL.md
└── pr-review/
    └── SKILL.md
```

### `SKILL.md` の形式

YAML frontmatter + Markdown 本文。Claude Code 互換。

```markdown
---
name: git-commit
description: コミットメッセージ規約に従って git commit を作成する
---

## 手順

1. `git status` で変更を確認
2. ...
```

- `name` 省略時はディレクトリ名を採用
- `description` 省略時は本文の先頭行で代替

### 仕組み（オンデマンド方式）

起動時にシステムプロンプトに **名前 + description のみ** を載せ、`load_skill` ツールを公開する。モデルは要求にマッチするスキルを見つけたとき自分で `load_skill(name="...")` を呼んで本文を取得する。スキル数が増えてもコンテキスト消費は名前と説明だけ。

スキルが 1 つも見つからなければ `load_skill` ツール自体を登録しないため、未使用環境では何も起きない。

`/skills` でロード済み一覧を確認可能。

## ツール一覧

| ツール           | 説明                                                         |
| ---------------- | ------------------------------------------------------------ |
| `bash`           | シェルコマンド実行                                           |
| `read_file`      | ファイル読み取り (行番号付き)                                |
| `write_file`     | ファイル書き込み                                             |
| `edit_file`      | テキスト置換による編集                                       |
| `list_directory` | ディレクトリ一覧                                             |
| `grep_search`    | ripgrep によるコード検索                                     |
| `todo_write`     | タスクリスト管理                                             |
| `load_skill`     | ローカル SKILL.md の本文取得 (skills が存在するときのみ登録) |

## アーキテクチャ

```
src/
├── main.rs           エントリポイント + REPL + agent_loop + Plan モード
├── llm_client.rs     OpenAI互換HTTPクライアント (SSEストリーミング対応)
├── tools.rs          ツール定義 + ハンドラ
├── todo_manager.rs   TodoWrite ツール
├── skills.rs         SKILL.md ローダ + load_skill ツール
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
