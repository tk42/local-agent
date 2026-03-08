# Local Agent

ローカルの llama-server (llama.cpp) で動く Claude Code 風コーディングエージェント CLI。

[shareAI-lab/learn-claude-code](https://github.com/shareAI-lab/learn-claude-code) を参考に、Anthropic SDK → OpenAI 互換 API に変換して実装。

## 前提条件

- Python 3.10+
- llama-server が `--jinja` 付きで起動済み (tool calling に必要)
- ripgrep (`rg`) がインストール済み (grep_search ツール用、なければ grep にフォールバック)

## セットアップ

```bash
cd apps/local-agent
cp .env.example .env
# .env を必要に応じて編集

pip install -r requirements.txt
```

## 使い方

### インタラクティブ REPL

```bash
python agent.py
```

### ワンショット

```bash
python agent.py "このプロジェクトの構造を教えて"
```

### REPL コマンド

| コマンド | 説明 |
|----------|------|
| `/compact` | コンテキストを強制圧縮 |
| `/todos` | 現在の Todo リスト表示 |
| `/tokens` | 推定トークン数を表示 |
| `/clear` | 会話履歴をクリア |
| `/help` | ヘルプ表示 |
| `q` / `exit` | 終了 |

## ツール一覧

| ツール | 説明 |
|--------|------|
| `bash` | シェルコマンド実行 |
| `read_file` | ファイル読み取り (行番号付き) |
| `write_file` | ファイル書き込み |
| `edit_file` | テキスト置換による編集 |
| `list_directory` | ディレクトリ一覧 |
| `grep_search` | ripgrep によるコード検索 |
| `todo_write` | タスクリスト管理 |

## アーキテクチャ

```
agent.py          メインループ + REPL (s_full 相当)
├── llm_client.py  OpenAI SDK ラッパー (ストリーミング対応)
├── tools.py       ツール定義 + ハンドラ
├── todo_manager.py  TodoWrite ツール
└── context.py     コンテキスト圧縮 (microcompact + auto_compact)
```

### コアパターン

```
User → messages[] → LLM → response
                           │
                    tool_calls あり?
                    ├─ Yes → ツール実行 → 結果を messages に追加 → ループ
                    └─ No  → テキスト返却 → 終了
```
