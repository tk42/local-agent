---
name: git-commit
description: Conventional Commits 規約に従って git commit を作成する
---

## 手順

1. `git status` で変更内容を確認する
2. `git diff --staged` でステージ済みの差分を確認する（ステージされていなければ `git add` を提案する）
3. 変更内容を分析し、Conventional Commits 形式のコミットメッセージを生成する

## コミットメッセージ形式

```
<type>(<scope>): <subject>

<body>
```

### type 一覧

- **feat**: 新機能
- **fix**: バグ修正
- **docs**: ドキュメントのみの変更
- **style**: コードの意味に影響しない変更（空白、フォーマット等）
- **refactor**: バグ修正でも機能追加でもないコード変更
- **perf**: パフォーマンス改善
- **test**: テストの追加・修正
- **chore**: ビルドプロセスや補助ツールの変更

### ルール

- subject は英語で書く
- subject は命令形（imperative mood）で書く（例: "add" not "added"）
- subject の先頭は小文字
- subject の末尾にピリオドを付けない
- body は変更の理由（why）を説明する（任意）
- 破壊的変更がある場合は `BREAKING CHANGE:` フッターを付ける

## 例

```
feat(skills): add on-demand skill loading system

Implement Claude Code-style local skills that are loaded via
load_skill tool. Only name and description are injected into
the system prompt to minimize context usage.
```

## 注意事項

- ユーザーに確認を取ってから `git commit` を実行すること
- `--amend` は明示的に指示された場合のみ使用する
