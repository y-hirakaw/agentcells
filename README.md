# agentcells

Issueを作ると、AIエージェントのcellが仕事を進めます。次の工程に回すか人間に回すかはdispatcherが振り分け、質問や検品の依頼はIssueとPRで人間に届きます。グラフエンジニアリング／ソフトウェアファクトリーのような仕組みです。

> **実験的なプロジェクトで、作成中です。** 仕様は予告なく変わります。設計は [concept.md](concept.md) にまとめています。

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

| コマンド | 何をするか |
| --- | --- |
| `agentcells doctor` | 動かすのに必要なものがそろっているかを確かめ、だめなら直し方を出す |
| `agentcells setup` | 作業データの置き場所と、設定ファイルの雛形（`config.toml`）を用意する（何度実行してもよい） |
| `agentcells labels sync [--dry-run]` | 設定ファイルのproductとcellに必要な `ac:` ラベルのうち、足りないものをGitHubに作る |
| `agentcells files apply <owner/repo> <worktree>` | gitignoreされたファイルを、worktreeの同じパスに置く |

設定ファイル `~/.agentcells/config.toml` には、product（どのリポジトリのIssueを、どのリポジトリで作業するか）とcell（どのラベルを拾い、どのプロンプトで動くか）を書きます。書き方は [concept.md](concept.md) と、雛形のコメントを見てください。

### GitHub App（推奨）

コメントやPRは、GitHub Appか、ghにログインしているあなたのアカウントで書き込みます。あなたのアカウントのままだと、AIの発言があなたの発言に見えてわかりにくいので、GitHub Appを作るのがおすすめです。

1. GitHubの Settings → Developer settings → GitHub Apps → New GitHub App で作る
   - Webhookは使わないので、Activeのチェックを外す
   - Repository permissions: Contents、Issues、Pull requests を Read and write にする（`.github/workflows` を変えるなら Workflows も）
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

MIT（[LICENSE](LICENSE)）
