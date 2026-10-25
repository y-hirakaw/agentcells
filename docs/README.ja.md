# agentcells

[English](../README.md) | 日本語

Issueを作ると、AIエージェントのcellが仕事を進めます。次の工程に回すか人間に回すかはdispatcherが振り分け、質問や検品の依頼はIssueとPRで人間に届きます。グラフエンジニアリング／ソフトウェアファクトリーのような仕組みです。

> **実験的なプロジェクトで、作成中です。** 仕様は予告なく変わります。構想は [concept.md](../concept.md)、作ったものの仕様は [docs/design.md](design.md)、変更の履歴は [CHANGELOG.md](../CHANGELOG.md) にあります。

## できるようにしたいこと

- 設定どおりにAIエージェントを用意すれば、GitHubにIssueを作るだけで処理が進む
- IssueやPRの上で、人間とやり取りしているのと同じように指摘や質問ができる
  - 「確認したいことがあります」（質問）
  - 「PRができたので確認をお願いします」（検品の依頼）
- cellはそれぞれ独立したループとして動き、組み合わせて使える

## このリポジトリ自体もagentcellsで開発する

このリポジトリにIssueを作ると、PRが作られ、AIがレビューし、指摘を直します。内容に応じて自動でマージされるか、人間にレビューの依頼が来ます。

## 構成（予定）

| 役割 | 何をするか |
| --- | --- |
| cell | トリアージ・設計・実装・レビューなど、1つの仕事を回す独立したループ |
| dispatcher | 次にどのcellに回すか、人間に回すかを判断する |
| inspector | cellが動いているか、どこで歩留まりが落ちているかを見てIssueにする |
| maintenance | inspectorが報告したIssueを受けて、ラインそのものを直す |
| shipping | 成果物の出荷（リリース）を担当する。製造とは責務を分ける |

## インストール（作成中）

今はソースからビルドします。Rustが入っていなければ、先に https://rustup.rs の手順で入れてください。

```sh
git clone https://github.com/y-hirakaw/agentcells.git
cd agentcells
./install.sh
```

`~/.agentcells/bin/agentcells` に入り、続けて `agentcells setup` が動きます。足りないものがあれば、直し方が表示されます。置き場所は環境変数 `AGENTCELLS_HOME` で変えられます。

新しい版にするときは、cloneしたリポジトリで次を実行します。入れ直さないと、手元の `agentcells` は古い版のままです（`agentcells --version` で確かめられます）。新しい版で設定の項目が増えたときは、[CHANGELOG.md](../CHANGELOG.md) に書いてあります。

```sh
git pull
./install.sh
agentcells doctor
```

新しく要る設定やラベルがあれば、`agentcells doctor` が直し方を出します。

| コマンド | 何をするか |
| --- | --- |
| `agentcells doctor` | 動かすのに必要なもの（ツール、ログイン、設定、GitHub App、ラベルなど）がそろっているかを確かめ、だめなら直し方を出す |
| `agentcells setup` | 作業データの置き場所と、設定ファイルの雛形（`config.toml`）を用意する（何度実行してもよい） |
| `agentcells labels sync [--dry-run]` | 設定ファイルのproductとcellに必要な `ac:` ラベルのうち、足りないものをGitHubに作る |
| `agentcells files apply <owner/repo> <worktree>` | gitignoreされたファイルを、worktreeの同じパスに置く |
| `agentcells tick [--dry-run]` | 仕事（あなたが選んだ、cellのラベルが付いたIssue/PRと、人間を待っていたところにあなたが返事をしたもの）を探して、1件ずつcellで動かす。仕事が残っていれば、上限（`--max-minutes`、既定は30分）まで続けて動かす。閉じたIssue/PRに残った `ac:` ラベルも外す。`--dry-run` なら表示だけ。AIが使用量の上限に当たったら、その仕事は人間に回さずに元に戻し、上限が戻るまで新しく始めない（`--ignore-pause` で動かせる）。implementはコミットを確かめてpushし、PRを作る。reviewはPRにレビューを投稿する |
| `agentcells schedule install [--every <分>]` / `remove` | `tick` を決まった間隔（既定は10分）で動かすように、launchd（macOS）に登録する／外す。cellが動いている間は、次のtickは何もしない。ログは `~/.agentcells/logs/tick.log` |
| `agentcells inspect [--dry-run]` | tickの記録（`~/.agentcells/logs/events.jsonl`）を決まった規則で調べ、ラインの不具合（同じcellが続けて動いた、AIの実行が失敗した、など）を見つけて、AIがagentcellsのソースから原因と直し方の案を書く。`[inspector] issues` に書いたリポジトリにIssueにする（書かなければ手元に残すだけ）。`[inspector]` を書けば、tickも1日に1回、最後にこれを動かす。`--dry-run` は、AIに渡す伏せ字の記録だけを出す。cellの実行ごとの要約（使ったツール、エラー、許されなかった操作）も読み、設定やテスト、プロンプトなど、ラインを遅くしていることを手元にメモする。`--notes` で見られる |
| `agentcells status [--watch]` | 動いているcell（経過時間、AIが使っているツール）と、扱っているIssueごとに、今どこまで進んでいるか（作業中、返事待ち、検品待ち、順番待ち）と、いつからかを出す。PRができたIssueは、PRの状態を出す（読むだけ）。`--watch` なら、動いているcellの様子だけを2秒ごとに出し直す |
| `agentcells prompt show <cell> [--issue owner/repo#N \| --pr owner/repo#N]` | cellとして動くAIに渡すプロンプト（標準、追加の指示、今回の仕事）を組み立てて表示する |

設定ファイル `~/.agentcells/config.toml` には、cellを使うあなたのGitHubのログイン（`operator`）と、product（どのリポジトリのIssueを、どのリポジトリで作業するか）とcell（どのラベルを拾い、どのプロンプトで動くか）を書きます。書き方は [docs/design.md](design.md) と、雛形のコメントを見てください。動くのは、あなたがAssigneesに自分を入れ、cellのラベルを付けたIssue/PRだけです（ほかの人がラベルやAssigneesを変えても、勝手に始まりません）。AIが質問して止まったもの（`ac:needs-human`）や、検品を待つPR（`ac:in-review`）には、人に返事をするのと同じようにコメントすれば、続きが動きます（ほかの人のコメントでは動きません。あなたのコメントは、どれも返事として扱います）。標準のcell（triage、implement、review、assess）は、プロンプトを書かなくても [prompts/](../prompts/) の標準のプロンプトで動きます。`prompt` にファイルを書くと、追加の指示として足されます。

### GitHub App（推奨）

コメントやPRは、GitHub Appか、ghにログインしているあなたのアカウントで書き込みます（当面はgithub.comだけに対応しています）。あなたのアカウントのままだと、AIの発言があなたの発言に見えてわかりにくいので、GitHub Appを作るのがおすすめです。

1. GitHubの Settings → Developer settings → GitHub Apps → New GitHub App で作る
   - Webhookは使わないので、Activeのチェックを外す
   - Repository permissions: Contents、Issues、Pull requests を Read and write にする（Workflows は要らない。agentcellsは `.github/workflows` を変えるコミットをpushしない）
   - Where can this GitHub App be installed? は Only on this account でよい
2. 作ったAppの画面で Generate a private key を押し、ダウンロードした `.pem` を `~/.agentcells/github-app.pem` に置いて `chmod 600` する
3. Install App から、Issueのリポジトリと作業するリポジトリにインストールする
4. `~/.agentcells/config.toml` に書く

   ```toml
   [github_app]
   id = "Iv23li..."                 # Appの画面のClient ID
   private_key = "github-app.pem"
   ```

5. `agentcells doctor` で確かめる。インストールが足りないリポジトリがあれば、インストールのURLが表示されます

## 開発

Rustのバージョンは `rust-toolchain.toml` で決めています。

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## 対応するエージェント

最初はClaude Codeを前提に作ります。名前と設計は特定のエージェントに依存しないようにしています。

## ライセンス

MIT（[LICENSE](../LICENSE)）
