# agentcellsの開発ルール

このリポジトリを開発するエージェントへの指示。cellとして動くエージェントへの指示ではない（それは `~/.agentcells` に置く）。

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

## レビュー

実装が終わったら、pushする前に `/herdr` スキルで右のpaneにいるエージェントにレビューを頼む。このルールは、やめるように言われるまで続ける。

- 指摘は must / should / imo / nits の4段階で評価してもらう
- must と should は直す。imo と nits は直すかどうかを判断してよい
- must か should を直したら、再レビューを頼む。再レビューは最大2回まで。2回目のあとも must か should が残ったら、ユーザーに伝える
- レビューの結果と、直したこと・直さなかったこと（理由も）をユーザーに報告する

## 秘密

- コミットの前に、betterleaksで秘密が入っていないかを調べる。`git config core.hooksPath .githooks` でフックを有効にしておく（betterleaksは `brew install betterleaks`）

## 確かめること

pushする前に、次がすべて通ること。

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```
