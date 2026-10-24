# concept

agentcellsの構想メモ。まだ決まっていないことは「未定」と書く。

## 目的

- 個人で使えるソフトウェアファクトリーを、公開リポジトリとして作る
- 記事で紹介でき、他の人がcellを足してコントリビュートできるものにする

## 人間との関係

AIが働いている工場に、人間がときどき呼ばれるイメージ。

- AIは止まらずに進める。判断に迷ったら止まって、Issueやコメントで人間に質問する
- 成果物（PR）ができたら、内容に応じて自動でマージするか、人間に検品を依頼する
- やり取りはすべてGitHubのIssueとPRの上で行う。やり取りのための専用の画面は作らない（動作状況を見るコンソールは別に用意する）

## 層

### 1. 製造: cell

1つの仕事を回す独立したループ。他のcellとはIssueやPRを受け渡すだけでつながる。

候補:

- triage cell: Issueを読んで、種類・大きさ・次の工程を決める
- design cell: 設計を書く（必要なときだけ）
- implement cell: 実装してPRを作る
- review cell: PRをレビューして指摘する。implement cellが指摘を直すまで往復する

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

PRはCLIが作る。cellの中でPRができるときも、作るのはCLI:

- AIはworktreeの中でコミットするところまで。`git push` と `gh pr create` は使わせない
- 実行が終わったら、CLIがベースブランチからのコミットがあるかを確かめ、あればブランチをpushしてPRを作る。なければ「PRが作られていない」として人間に回す
- ブランチ名は `ac/<Issueのowner>-<Issueのrepo>-<Issue番号>`。Issueのリポジトリと作業するリポジトリが違ってもぶつからない
- PRの本文は1行目に目印（`[agentcells] implement round 1`）、その下にAIのまとめと `Closes <owner>/<repo>#<番号>` を書く
- PRのタイトルはIssueのタイトルにする

### 遷移

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

### コメントとレビュー

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

### 2. 振り分け: dispatcher

cellの外にいて、次の行き先を判断する。単に運ぶのではなく判断するところがコンベアとの違い。

- 次にどのcellに回すか（今はcellのAIが `next` の中から選ぶ）
- 人間に回すか（質問、検品）
- 自動でマージしてよいか

自動マージの判断の基準をどこに書くかは未定。

### 3. 監視と改善: inspector / maintenance

製品ではなく、ラインそのものを相手にする層。

- inspector: cellが動いているか、どこで止まりやすいか、どこで差し戻しが多いか（歩留まり）を見て、Issueにする
- maintenance: inspectorのIssueを受けて、cellやdispatcherの設定・プロンプトを直す

### 4. 出荷: shipping

リリースを担当する。製造とは責務を分ける。出荷してよいかの判定を持つ。

## 状態の持ち方

- 状態はGitHubのIssue・PRのラベルとコメントで持つ（専用のデータベースは持たない）
- agentcellsのラベルは `ac:` で始める
  - `ac:<工程>`（`ac:triage` など）: そのcellが拾う。名前は設定ファイルで決める
  - `ac:working`: 処理中。同じ仕事を二重に拾わないようにする
  - `ac:needs-human`: 人間の回答や検品を待っている
- `agentcells labels sync` で、足りないラベルをリポジトリに作る。すでにあるラベルは触らない
- 起動はタイマー（定期実行）。GitHubのイベントで起動するかは未定

## 実行のしかた

当面はClaude Codeを使い、サブスクリプションの範囲で回せることを中心にする。

AIを起動する前後の処理は、すべてagentcellsのCLIにまとめる。

- 仕事があるかを調べる（AIは使わない）
- 仕事を取って、処理中のラベルを付ける
- cellの定義からプロンプトを組み立てる
- 結果をGitHubに反映し、実行の記録を残す

CLIを呼び出す方法は2つ用意し、どちらでも動くようにする。片方が使えなくなっても、もう片方に切り替えれば止まらない。

| 方法 | 動き | 注意 |
| --- | --- | --- |
| A. Claude Desktopのルーティン | 定期実行でAIが起動し、CLIの出す指示に従う | 仕事がなくてもAIが起動するので、最初にCLIで確かめてすぐ終える |
| B. スクリプト | 定期実行（launchdなど）でCLIが仕事を調べ、あるときだけ `claude -p` を起動する | `claude -p` が使えなくなる可能性があるので、Aに切り替えられるようにしておく |

cellの定義（拾う条件、プロンプト、止まる条件、往復の上限）はAとBで共通にする。どちらで動いているかをcellは知らなくてよい。

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

### 作業データの置き場所

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

### エージェントの設定

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

### GitHubへの書き込み

コメント、ラベル、push、PR、レビューのような書き込みは、CLIがする。どのアカウントで書き込むかは2つから選べる。

| | GitHub App（推奨） | ghのアカウント |
| --- | --- | --- |
| 書き込む名前 | `<Appの名前>[bot]` | ghにログインしている人 |
| 見分けやすさ | AIの発言と人間の発言を見分けられる | AIのコメントやPRが自分の発言に見え、わかりにくい |
| 準備 | Appを作り、リポジトリにインストールし、`config.toml` の `[github_app]` に書く | なし（`gh auth login` だけ） |

- `[github_app]` を書かなければ、ghのアカウントで書き込む。`doctor` はそのことを知らせ、Appを勧める
- Appのときは、JWT（有効期限9分）でリポジトリのインストールを探し、そのリポジトリだけに使えるトークン（有効期限1時間）を作る。Issueのリポジトリと作業するリポジトリが別なら、それぞれに作る
- トークンもJWTも、コマンドの引数には書かず環境変数で渡す（`ps` で見えないように）。GitHubは `Authorization: token` で送ったJWTも受け付けるので、ghには `GH_TOKEN` で渡せる
- gitには `-c credential.helper=` で、使う人のgitの設定にあるcredential helperを外してから、agentcellsのhelperを渡す。Appならトークンを、ghのアカウントなら `gh auth git-credential` を使う
- AIの実行（`claude -p`）にはトークンを渡さない。AIはコミットまでで、書き込みはCLIだけがする
- Appのときは、AIのコミットの作者も `<Appの名前>[bot]` にする（予定。`GIT_AUTHOR_NAME` などで渡す）
- `doctor` は、秘密鍵が読めるか、idと鍵が合っているか、設定にあるすべてのリポジトリにAppがインストールされているか、鍵の権限が600か、を確かめる。インストールされていなければ、インストールのURLを出す
- Appに要る権限は、Contents（読み書き）、Issues（読み書き）、Pull requests（読み書き）、Metadata（読み取り）。`.github/workflows` を変えるPRを作るならWorkflows（読み書き）も要る

### gitignoreされたファイルの配置

keystore（Android）やapplication.yml（Spring Boot）のように、gitignoreされていてビルドに要るファイルを、worktreeを作るときに自動で置く。

- `files/<owner>/<repo>/` の下に、worktreeに置くのと同じパスでファイルを置いておく
- 置く前に `git check-ignore` で、そのパスがgitignoreされているか確かめる。されていなければ置かずに知らせる（秘密のファイルをコミットさせない）
- シンボリックリンクではなくコピーで置き、権限は600にする（元のファイルを書き換えさせない）
- 置いたファイルは、`--settings` で渡す設定に `Read(//絶対パス)` のdenyを足して読めなくする。ビルドツールは読めるが、AIは中身を読まないので、プロンプトや実行の記録に入らない。worktreeに設定ファイルを書かないので、それをコミットしてしまう心配もない（Aのルーティンでの渡し方は未定）
- `doctor` で、登録しているリポジトリに必要なファイルがそろっているかを確かめ、足りなければ置き場所を案内する
- あとで、リポジトリごとの準備コマンド（`npm ci` など）も足せるようにしておく

## ダッシュボード

「今どうなってるかな」と見に来たら、すぐわかる画面を用意する。`claude -p` の動作を見るコンソールも、ここにまとめる。

見せるもの:

- どのIssue/PRが、どのcellで処理されているか（`ac:working` と、実行中の `claude -p`）
- 何が待ちになっているか
  - 人間待ち（`ac:needs-human`）: 質問への回答か検品か。いつから待っているか
  - cellの順番待ち（`ac:<cell>` が付いていて、まだ拾われていない）
- 実行中の `claude -p` の様子（今どのツールを使っているか、経過時間）
- 最近の実行の結果（どこへ進んだか、人間に回った理由、失敗、タイムアウト）

実行の記録:

- `claude -p --output-format stream-json` の出力を、実行ごとにファイルへ記録する
- 終了コード、かかった時間、タイムアウト、セッションID（`--resume` で再開するため）も残す
- GitHubの状態（ラベル）と、実行の記録をあわせて表示する

作り方:

- 最初はターミナルUIで、情報を並べるだけにする
- 最終的には工場風に見せたい。cellを製造ラインの工程、Issue/PRをラインを流れる製品として描き、どこで止まっているか、どこに人間が呼ばれているかが一目でわかるようにする。inspectorの歩留まりの可視化と合わせて、Web画面を検討する

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

## 進め方

1. このリポジトリ自体で、triage → implement → review の最小ループを動かす
2. dispatcherで「自動マージ / 人間に検品」を分ける
3. inspectorで止まった場所を見えるようにする
4. 動かした結果を記事にする（作ってから書く）

## 決まっていないこと

- productの repos が複数あるとき、どのリポジトリで作業するか
- 定期実行の間隔
- Claude Desktopのルーティンで、作業フォルダや外のディレクトリへの権限をどう扱えるか（実機で確かめる）
- ラベルの同期（`labels sync`）もAppのトークンで行うか（今はghのアカウントで行う）
- AIが読むためにghを使うとき、どのアカウントを使わせるか（今はghのアカウントのまま。読み取りに必要な内容はプロンプトに入れて、ghを使わせない形も検討する）

## ライセンス

MIT
