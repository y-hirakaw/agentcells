# cellの例

agentcellsに標準で入っていないcellの例です。使うときは、写して直してください。

1. ファイルを `~/.agentcells/cells/` に写す（ファイルの名前が、cellの名前になります）
2. 頭の `+++` の間の設定（拾うラベル、進め先、使えるコマンドなど）と、本文の指示を、自分のリポジトリに合わせて直す
3. `~/.agentcells/config.toml` の、使うproductの `cells` に名前を足す。ほかのcellの `next` からも、必要なら進めるようにする
4. `agentcells doctor` で確かめ、`agentcells prompt show <名前> --issue owner/repo#N` で、AIに渡る指示を見る
5. ラベルがなければ、`doctor` が作り方を出します

| ファイル | 何をするか |
| --- | --- |
| [design.md](design.md) | Issueの要件から設計を書き、人が認めたら implement に進む。ファイルは変えない |
| [release-prep.md](release-prep.md) | 版を上げ、変更の履歴をまとめたPRを作る。タグ付けや、秘密が要る出荷はしない |

- 例の中の「ファイルを変えない」などは、AIへの指示です。CLIが止めるのは、頭の設定で決めたこと（拾うラベル、進め先、使えるコマンドなど）と、すべてのcellに共通の守り（sandbox、hook、コミットとpushの確かめ）です。`confirm = true` のcellは、コミットがあってもpushしません
- 標準にないcellは、本文（役割と、何をするか）がないと、設定の確かめでエラーになります

設定の項目は [docs/design.md](../../docs/design.md) の「cellの設定」にあります。
