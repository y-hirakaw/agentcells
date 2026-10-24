# 設計

agentcellsで、作ったものと決まったことの仕様。ひと通り整うまでは、ここで管理する。

- まだ作っていないことや決まっていないことは [concept.md](../concept.md) に書く。作ったら、こちらに移す
- 変更の履歴（版、何をしたか、なぜやったか）は [CHANGELOG.md](../CHANGELOG.md) に書く
- 「（予定）」と書いたものは、決まっているがまだ作っていない

## cellの設定

cellは設定ファイル（`~/.agentcells/config.toml`、TOML）の `[[cell]]` に書く。

```toml
[[cell]]
name = "review"
on = "pull_request"          # issue か pull_request（両方なら ["issue", "pull_request"]）
label = "ac:review"          # 拾うラベル
prompt = "cells/review.md"   # プロンプトのファイル
next = ["implement", "done"] # 進めてよい先（cellの名前か done）
max_rounds = 3               # 往復の上限（無限ループを防ぐ。1〜3、省略すると3）
timeout_minutes = 30         # 1回の実行の上限（省略すると30）
```

## PRの作成

PRはCLIが作る。cellの中でPRができるときも、作るのはCLI:

- AIはworktreeの中でコミットするところまで。`git push` と `gh pr create` は使わせない
- 実行が終わったら、CLIがベースブランチからのコミットがあるかを確かめ、あればブランチをpushしてPRを作る。なければ「PRが作られていない」として人間に回す
- ブランチ名は `ac/<Issueのowner>-<Issueのrepo>-<Issue番号>`。Issueのリポジトリと作業するリポジトリが違ってもぶつからない
- PRの本文は1行目に目印（`[agentcells] implement round 1`）、その下にAIのまとめと `Closes <owner>/<repo>#<番号>` を書く
- PRのタイトルはIssueのタイトルにする

## 遷移

cellが終わったら、AIは「次はどこか」と、Issue/PRに残すコメントを返す。ラベルを付け替えるのはCLIだけで、許された遷移しかできないようにする。

- 進めてよい先は、cellごとに `next` で決める。`needs-human`（人間に回す）は、迷ったら止まれるように、どのcellからでも選べる
- `next` の先のcellは、同じproductの `cells` に入っていないと設定の確認でエラーになる
- 選べる先は何重にも縛る
  - B（`claude -p`）は `--json-schema` で `next` を選べる先の `enum` にする。許されていない先は出せない（出そうとすると結果が空になる）
  - A（ルーティン）はAIが `agentcells finish --next <先>` を実行し、CLIが確かめる（予定）
  - どちらも最後は同じ確認を通す。許されていない先、結果が空、往復の上限を超えた先は `ac:needs-human` に回し、理由をコメントする
- AIにはラベルやIssue/PRの状態を変えさせない。`--settings` のdenyで `gh issue edit`、`gh pr edit`、`gh label`、`gh api` などを止める。実行の前後で `ac:` のラベルが変わっていたら、`ac:needs-human` に回す
- 人間は自由にラベルを付け替えてよい。縛るのはAIの遷移だけ（`ac:needs-human` を外して `ac:implement` を付ければ再開できる）

最小ループの遷移:

| cell | 扱うもの | 進めてよい先 | 意味 |
| --- | --- | --- | --- |
| triage | Issue | implement, done | 着手する / 対応しない（Issueを閉じる） |
| implement | Issue, PR | review | PRを作る（Issueから）か指摘を直して、レビューに回す |
| review | PR | implement, done | 指摘を直してもらう / 問題なし |
| （すべて） | | needs-human | 質問や検品を人間に頼む |

- Issueを扱うcellから、PRを扱うcellに進むときは、実行の中で作られたPRにラベルを付ける。PRが作られていなければ人間に回す
- `done` は、Issueなら閉じる。PRなら、自動マージの判断（dispatcher）ができるまでは `ac:needs-human` を付けて検品を頼む
- 往復は最大3回（`max_rounds` は1〜3）。進む先のcellが、そのIssue/PRですでに上限の回数動いていたら人間に回す。implementの最初の実装はIssueの上で数え、PRでは指摘を直した回数を数えるので、review → implement の差し戻しは最大3回になる

## コメントとレビュー

Issue/PRへのコメントとレビューは、CLIが投稿する。AIは内容を返すだけで、`gh issue comment`、`gh pr comment`、`gh pr review` は使わせない。

- CLIが投稿するコメントは、1行目を `[agentcells] <cell> round <n>` にする（例: `[agentcells] review round 1`）。PRの本文とレビューの本文も同じ形にする
- AIはIssueのコメントで人間に話しかける。`needs-human` に回すときは、その理由と人間にしてほしいことをコメントに書く
  - triageで、仕様について人間に確かめたいことがある（質問を箇条書きにする）
  - implementで実装してみたが、環境のせいでビルドが通らない（やったこと、結果、人間にやってほしいことを書く）
- 人間はIssueにコメントで答え、ラベルを付け替えて再開する。次の実行では、最後の目印のあとの人間のコメントをプロンプトに入れる（予定）
- 往復の回数は、この目印を数える。そのIssue/PRで、そのcellの目印にあるroundの最大値が、動いた回数になる
- PRへのレビュー（`review = true` のcell）は、GitHubのPRレビューとして投稿する
  - レビュー本文の1行目に目印を書き、その下にまとめを書く
  - コードへの指摘は1件につき1コメントで、コードの行に付ける。行コメントはそのレビューに付くので、GitHubの画面でもレビューの下にまとまる
  - 差分の外の行にはGitHubが行コメントを付けられないので、そうした指摘はレビュー本文にまとめて書く
  - 自分のPRには「変更を要求」も「承認」もできないので、レビューの種類はCOMMENTにする

## 状態の持ち方

- 状態はGitHubのIssue・PRのラベルとコメントで持つ（専用のデータベースは持たない）
- agentcellsのラベルは `ac:` で始める
  - `ac:<工程>`（`ac:triage` など）: そのcellが拾う。名前は設定ファイルで決める
  - `ac:working`: 処理中。同じ仕事を二重に拾わないようにする
  - `ac:needs-human`: 人間の回答や検品を待っている
- `agentcells labels sync` で、足りないラベルをリポジトリに作る。すでにあるラベルは触らない
- 起動はタイマー（定期実行）。GitHubのイベントで起動するかは未定

## 作業するリポジトリ

Issueがあるリポジトリと、実装を進めるリポジトリは同じとは限らない。

- どのIssueをどこで作業するかは、設定ファイルの `[[product]]` で決める。productは「Aプロダクト」「Bプロダクト」のような、cellで回したい範囲の単位

```toml
[[product]]
name = "agentcells"
issues = "y-hirakaw/agentcells"    # Issueを置くリポジトリ
repos = ["y-hirakaw/agentcells"]   # 作業するリポジトリ
cells = ["triage", "implement", "review"]
```

- PRは作業するリポジトリに作り、Issueにリンクをコメントする
- 作業は仕事ごとのworktreeで行う。人間がふだん使っているcloneとは別に、作業用のcloneを持つ
- `claude -p` はworktreeの中で起動する。作業するリポジトリのCLAUDE.mdと設定がそのまま効く
- PRがマージかクローズされたら、worktreeを片付ける

## 作業データの置き場所

agentcellsのリポジトリの中ではなく、独立した場所（`~/.agentcells/`）に置く。

- インストールスクリプトで入れた人は、agentcellsのリポジトリを持っていない（ビルド済みのバイナリを配るようになってから）
- agentcellsの中に置くと、親にあるagentcellsのCLAUDE.mdが別のリポジトリの作業に混ざる
- `git clean -fdx` で消えたり、`git add -f` で秘密のファイルが入ったりするのを避ける

```
~/.agentcells/
  bin/                                   agentcellsのCLI
  .claude/settings.json                  cellとして動くエージェントの設定（setupで置く）
  CLAUDE.md                              cellとして動くエージェントへの指示（setupで置く）
  repos/<owner>/<repo>/                  作業用のclone
  worktrees/<owner>/<repo>/<issue番号>/  仕事ごとのworktree
  files/<owner>/<repo>/<配置するパス>     worktreeに置くファイル
  logs/                                  実行の記録
```

## エージェントの設定

cellとして動くエージェントのsettings.jsonやhooksは、グローバル設定ではなくagentcellsの設定で決める。

- 原本はこのリポジトリで管理し、`agentcells setup` が `~/.agentcells` に置く
- このリポジトリの `.claude/` は、agentcellsを開発するエージェントのための設定。cellとして動くエージェントの設定とは分ける
- B（`claude -p`）は、worktreeの中で `--settings` にagentcellsの設定を渡し、`--setting-sources project,local` でグローバル設定を外して起動する。作業するリポジトリ自身の設定とCLAUDE.mdの上に、agentcellsの設定を重ねる形になる
- A（ルーティン）は、作業フォルダを `~/.agentcells` にする。そこの `.claude/settings.json` とCLAUDE.mdが効く

Claude Code 2.1.310 の `claude -p` で確かめたこと（2026-10-24）:

- hooksは上書きされずに合わさる。project・local・`--settings` のSessionStart hookが3つとも動いた
- denyはallowに勝つ。projectのdenyと `--settings` のallowがぶつかると、読めなかった
- 信頼していないディレクトリ（一度も対話で開いていないディレクトリ）では、projectの `settings.json` のallowが無視される。denyとhooksは効く。worktreeは毎回新しいパスなので、cellに必要なallowは `--settings` で渡す
- `Read(//絶対パス)` の形でdenyを書ける。Readを禁止したファイルは、`Bash(cat:*)` を許していても `cat` で読めなかった（ただし、ほかのコマンドで読めないとまでは言えないので、秘密を守る仕組みの1つとして扱う）
- `Bash(gh label:*)` や `Bash(gh api:*)` のdenyは、allowに同じものがあっても効いた（`gh repo view` のような許したコマンドは動く）
- `--json-schema` の `enum` にない値は、AIに強く指示しても返せなかった。そのときは結果の `structured_output` が null になる
- `--setting-sources project,local` でグローバル設定のhooksが止まるかは、まだ確かめていない

## GitHubへの書き込み

コメント、ラベル、push、PR、レビューのような書き込みは、CLIがする。どのアカウントで書き込むかは2つから選べる。

| | GitHub App（推奨） | ghのアカウント |
| --- | --- | --- |
| 書き込む名前 | `<Appの名前>[bot]` | ghにログインしている人 |
| 見分けやすさ | AIの発言と人間の発言を見分けられる | AIのコメントやPRが自分の発言に見え、わかりにくい |
| 準備 | Appを作り、リポジトリにインストールし、`config.toml` の `[github_app]` に書く | なし（`gh auth login` だけ） |

- `[github_app]` を書かなければ、ghのアカウントで書き込む。`doctor` はそのことを知らせ、Appを勧める
- 今これを使って書き込むのは `labels sync` だけ。コメント、push、PR、レビューは `tick` で使う（予定）。読み取り（ラベルの一覧など）は、ghのアカウントのままで行う
- Appのときは、JWT（有効期限9分）でリポジトリのインストールを探し、そのリポジトリだけに使えるトークン（有効期限1時間）を作る。Issueのリポジトリと作業するリポジトリが別なら、それぞれに作る
- トークンには、使い道に要る権限だけを付ける。ラベルを作るならIssuesの書き込みだけ、作業（push、PR、コメント、レビュー）ならContents・Issues・Pull requestsの書き込み。Appに付けたほかの権限（Workflowsなど）は付けない
- トークンもJWTも、コマンドの引数には書かない（`ps` で見えるため）
  - Appとしての呼び出し（Appの情報、インストールの検索、トークンの作成）は、JWTを `Authorization: Bearer` で送る必要がある。`token` 方式では「A JSON web token could not be decoded」で断られる（2026-10-24に本物のAppで確かめた）。ghの `GH_TOKEN` は `token` 方式で送るので、これにはcurlを使い、ヘッダーはcurlの設定として標準入力から渡す。`--disable` で `~/.curlrc` を読まない（AIも書けるため）
  - 作ったトークン（installation token、ghのアカウントのトークン）は `token` 方式で受け付けられるので、ghには `GH_TOKEN` で、gitにはcredential helperが読む環境変数で渡す。`GH_HOST` もgithub.comにする（当面はgithub.comだけを扱う）
- トークンを渡すghには、毎回まっさらな設定ディレクトリ（`GH_CONFIG_DIR`）を使う。`~/.config/gh/config.yml` はAIも書けて、`http_unix_socket` などで通信を横取りできるため
- AIの実行（`claude -p`）にはトークンを渡さない。AIはコミットまでで、書き込みはCLIだけがする
- AIの実行には、Appの秘密鍵、`config.toml`、`files/` の下を `Read(//絶対パス)` のdenyで読ませない。秘密鍵を読まれると、インストール先のすべてのリポジトリへのトークンを作れてしまうため（`files/**` のようなディレクトリの指定も効くことを、2.1.310で確かめた）。パスは、書いたままの形と、シンボリックリンクや `..` を解いた形の両方をdenyに入れる
- `gh auth` もdenyにする。ghのアカウントで動くときは、`gh auth token` で使う人のトークンを取り出せるため（これもAppを勧める理由の1つ）。ghの設定ディレクトリ（`GH_CONFIG_DIR`、`XDG_CONFIG_HOME/gh`、`~/.config/gh`）も読ませない。keyringを使わない環境では、`hosts.yml` にトークンが平文で入っているため
- ghのアカウントで書き込むときも、CLIは最初に `gh auth token` でトークンを1回だけ取り出し、あとはAppのときと同じように、トークンと空の設定ディレクトリでghとgitを動かす。`gh auth token` は通信しないので、ghの設定にある通信の設定の影響を受けない
- Appのときは、AIのコミットの作者も `<Appの名前>[bot]` にする（予定。`GIT_AUTHOR_NAME` などで渡す）
- `doctor` は、設定にほかの問題（プロンプトのファイルがないなど）があっても、TOMLとして読めればGitHubの確認まで進む。`doctor` は、秘密鍵が読めるか、idと鍵が合っているか、Appに要る権限が付いているか、設定にあるすべてのリポジトリにAppがインストールされているか、鍵を持ち主だけが読めるか（600など）、を確かめる。足りなければ、権限の設定画面やインストールのURLを出す
- Appに要る権限は、Contents（読み書き）、Issues（読み書き）、Pull requests（読み書き）、Metadata（読み取り）

gitで書き込むとき（push）は、worktreeの `.git/config` をAIが書き換えられることを前提にする。

- トークンを使うpushは、worktreeからはしない。CLIがpushのたびに新しいbareリポジトリを作り、トークンを渡さずにworktreeのブランチを移し（fetch）、そこから `https://github.com/<owner>/<repo>.git` にpushする。URLごとの `http.<URL>.sslVerify` やproxyは `-c` でも上書きできないので、AIが書いた設定を読まない場所からpushする
- `GIT_ALLOW_PROTOCOL=https` で、https以外の通信を止める。設定より優先されるので、`url.ext::<コマンド>.insteadOf` のような書き換えでコマンドを動かされない（ブランチを移すときは `file` だけを許す）
- `-c credential.helper=` で、使う人やリポジトリの設定にあるcredential helperを外してから、環境変数のトークンを返すagentcellsのhelperを渡す
- helperはgithub.com宛てにだけ付ける（`credential.https://github.com.helper`）。URLをほかのホストに向けられても、トークンを渡さない
- `core.hooksPath=/dev/null` でhooks（pre-pushなど）を動かさない
- 使う人とシステムのgitの設定は読まない（`GIT_CONFIG_GLOBAL=/dev/null`、`GIT_CONFIG_NOSYSTEM=1`）。AIは `git config --global` も実行できるため
- `GIT_TERMINAL_PROMPT=0` で、パスワードの入力を求めて止まらないようにする
- テストでは本物のgitを動かし、書き換えた設定（`ext::`、`insteadOf`、URLごとのsslVerifyとproxy、hooksPath、helper）があっても、トークンが漏れないことを確かめる。守りを外すとテストが落ちることも確かめた

ここまでは、どれも守りを重ねるためのもの。AIのBashは使う人と同じ権限で動くので、次がいちばんの守りになる（どちらも `tick` を作るときに入れる。予定）。

- 許すコマンドを絞る（`--settings` のallow）
- `claude -p` のプロセスグループを終わらせてから、JWTやトークンを作る。AIが残したプロセスが生きていると、一時ディレクトリを書き換えたり、Linuxでは `/proc/<pid>/environ` からトークンを読んだりできるため

## gitignoreされたファイルの配置

keystore（Android）やapplication.yml（Spring Boot）のように、gitignoreされていてビルドに要るファイルを、worktreeを作るときに自動で置く。

- `files/<owner>/<repo>/` の下に、worktreeに置くのと同じパスでファイルを置いておく
- 置く前に `git check-ignore` で、そのパスがgitignoreされているか確かめる。されていなければ置かずに知らせる（秘密のファイルをコミットさせない）
- シンボリックリンクではなくコピーで置き、権限は600にする（元のファイルを書き換えさせない）
- 置いたファイルは、`--settings` で渡す設定に `Read(//絶対パス)` のdenyを足して読めなくする。ビルドツールは読めるが、AIは中身を読まないので、プロンプトや実行の記録に入らない。worktreeに設定ファイルを書かないので、それをコミットしてしまう心配もない（Aのルーティンでの渡し方は未定）
- `doctor` で、登録しているリポジトリに必要なファイルがそろっているかを確かめ、足りなければ置き場所を案内する
- あとで、リポジトリごとの準備コマンド（`npm ci` など）も足せるようにしておく

## セットアップ

Homebrewのように、どんな状態からでもセットアップできるようにする。

- `agentcells setup` は何度実行しても同じ結果になるようにする。途中で失敗しても、もう一度実行すれば続きから進む
- `agentcells doctor` で項目ごとに確認し、だめなときは理由と直し方を出す（`claude` の有無とログイン、`gh` の認証と権限、git、リポジトリのラベル、定期実行の登録など）
- 当面はHomebrewなどには登録せず、自作のインストールスクリプトで入れる
- スクリプトはCLIを `~/.agentcells/bin` に入れるところまでにとどめ、そのあとの確認と設定は `agentcells setup` に任せる（シェルスクリプトを小さく保ち、込み入った処理はRustでテストする）
- 最初はスクリプトがソースからビルドする。GitHubのReleasesにビルド済みのバイナリを置くようになったら、それをダウンロードする形に切り替える（Rustの開発環境がなくても入れられる）

## 言語

CLIはRustで書く。

- 頻繁に定期実行するので、起動が速くメモリが少ないほうがよい
- 1つのバイナリで配れるので、使う人の環境に実行環境を入れなくてよい（セットアップの目標に合う）
- テストが数千件に増えても、速く回せる
- AIが書いたコードの誤りを、コンパイラが見つけてくれる

ビルドに時間がかかるのが弱み。開発の速さを優先したくなったときの候補はGo。
