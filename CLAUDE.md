# agentcellsの開発ルール

このリポジトリを開発するエージェントへの指示。cellとして動くエージェントへの指示は `~/.agentcells` に置く。

## agentcellsのcellとして、このリポジトリで作業するとき

cellもこのファイルを読む。cellとして動くときは、「レビュー」の節（右のpaneに頼むこと）と、「マージしたあと」の節（版を上げてまとめること）、mainへの直接のpush、タグのpushは除く。PRはCLIが作り、レビューはreviewのcellがする。版と `CHANGELOG.md` は変えず、変更の内容を `changes/` のファイルに書く（「版とCHANGELOG」の節の「PRでは」）。

cellのsandboxでは、プロセスの一覧と手元のポートが使えないので、次のテストは中身を確かめずに通る（飛ばす）: `run::process::tests::the_process_table_sees_this_process`、`run::tests::unstopped_processes_mean_nothing_is_written_to_github`、`github::tests::curl::*`、`fetch::tests::curl::*`。これらに関わる変更は、PRのCI（ubuntuとmacOS）で確かめる必要があると、PRに書くこと。

## やり取り

- 日本語で書く
- 当面はmainに直接pushする

## ドキュメント

- [concept.md](concept.md): 最初の構想。まだ作っていないことと、決まっていないことを書く
- [docs/design.md](docs/design.md): 作ったものと決まったことの仕様。ひと通り整うまでは、ここで管理する
- 作ったら、concept.md からその部分を消して docs/design.md に移す

## 版とCHANGELOG

- 版は `Cargo.toml` の `version` で管理する。1.0.0までは、機能を足したらマイナー（0.x.0）、直しだけならパッチ（0.0.x）を上げる。ドキュメントやテストだけの変更では上げない
- 版を上げるときは、[CHANGELOG.md](CHANGELOG.md) に版と日付、足した機能や直した内容、なぜやったかを書く
- pushしたら、その版のタグ `v<版>` を付けてpushする
- 「版」は、`Cargo.toml` の `version` と、それに合わせて変わる `Cargo.lock` の `agentcells` の `version` の2つ。いつも2つをそろえる（CIは `--locked` で動く）

### PRでは

PR（cellのものも）では、版と `CHANGELOG.md` を変えない。PRごとに書き換えると、ほかのPRやmainとぶつかり続けるため。

- 代わりに、`changes/<Issue番号>.md` に、何をしたかとなぜやったかを書く。Issue番号は、元のIssueの番号（PRの番号ではない）。Issueがなければ、ほかと重ならない名前にする。同じ名前のファイルがもうあれば（同じIssueの前のPRがマージされ、まだ版になっていない）、そこに書き足す
- 中身は、版の見出し（`## [...]`）を書かず、`### 足したこと` か `### 直したこと`（使うほうだけ）から始める。下は例

  ```markdown
  ### 足したこと

  - `doctor` で、〜を知らせる。〜のため
  ```

- 版を上げない変更（ドキュメントやテストだけ）では、`changes/` のファイルを作らない
- 前のルールで、版や `CHANGELOG.md` をすでに変えているPRでは、それらをベースのブランチと同じに戻す（取り込みでぶつかったら、ベースの側を採る）。`CHANGELOG.md` に足していた内容は、別のコミットで `changes/` のファイルに移す

### マージしたあと

- PRをマージしたら、続けてmainで版を上げ、タグを付ける。するのは、mainを扱うこの開発のエージェント（ユーザーがマージを伝えたときか、次に作業を始めたときに `changes/` を見て気づく）。版にしないままにすると、動いているagentcellsが、古い版の番号のまま新しい変更を含むことになる（inspectorは、その版のタグのソースを読む）
- 版を上げるときは、`changes/` のファイルをすべて、その版の節にまとめてから消す。足したことが1つでもあればマイナー、直しだけならパッチを上げる

## レビュー

実装が終わったら、pushする前に `/herdr` スキルで右のpaneにいるエージェントにレビューを頼む。このルールは、やめるように言われるまで続ける。

- 指摘は must / should / imo / nits の4段階で評価してもらう
- must と should は直す。imo と nits は直すかどうかを判断してよい
- must か should を直したら、再レビューを頼む。再レビューは最大2回まで。2回目のあとも must か should が残ったら、ユーザーに伝える
- レビューの結果と、直したこと・直さなかったこと（理由も）をユーザーに報告する

## 秘密

- コミットの前に、betterleaksで秘密が入っていないかを調べる。`git config core.hooksPath .githooks` でフックを有効にしておく（betterleaksは `brew install betterleaks`）。cellとして動くときは、`agentcells cell commit` が調べるので要らない

## 確かめること

pushする前に、次がすべて通ること。

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```
