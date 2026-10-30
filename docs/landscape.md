# 周りの動きと、agentcellsの位置づけ

agentcellsの目的を考えるために調べたことと、そこから考えたことのメモ（2026-10-30〜31）。調べたものの多くは、売り手の発表やブログで、論文の一部には日付や番号の食い違いもあった。数字を引くときは、もとの資料で確かめる。

- 目的そのものは [concept.md](../concept.md) の「目的」に書く。ここは、その材料
- 調べたことは古くなるので、使うときは、日付を見て確かめ直す

## 目的の言い直し

最初の目的は「個人で使えるソフトウェアファクトリー」だった。それを、人間とAIが、バージョン管理をしながら、何人かで1つのものを作っていく仕組みで、作るものはClaudeが作れるものなら何でもよい、と言い直した（concept.md の「目的」）。

ほかの道具との違いを探すより、自分が使いたいものを作る。外からのアイデアは、Issueにして対話しながら取り込む。

## 似た仕組み

### TAKT（nrslib/takt）

いちばん近い国産のOSS（MIT、TypeScript）。plan → implement → review → fix のような流れをYAMLで宣言し、Claude Code、Codexなどを順に動かす。

- 中心は、人がCLIから仕事を投げる形。`takt #N` でIssueを取り込む使い方、常駐の `watch`、CI向けのパイプライン、GitHub Actions版もある
- 用語は、piece / movement から workflow / step に変わった（0.34.0〜0.36.0）
- stepの移り方: 使う人が rules に条件と次のstepを書く。`approved` のような意味の判定はAI（status judge）が1つ選び、`when(...)` はエンジンが計算で決める。どれにも当たらなければ止まる。つまり、使う人が定義したつながりを、AIが実行時に選んでたどり、エンジンが縛る点は、agentcellsと同じ
- agentcellsとの違い: agentcellsは、状態をGitHubのラベルとコメントに置き、人もAIも同じ場所で受け渡す。tickで自分から仕事を拾う常駐の仕組み

出典: [nrslib/takt](https://github.com/nrslib/takt)、[TAKT docs/workflows.md](https://github.com/nrslib/takt/blob/main/docs/workflows.md)

### n8n、Dify

汎用のAIアプリや自動化をノードで組む道具で、層が違う。コーディングエージェントを工程として回す公式の仕組みは見つからなかった（逆に、Claude CodeがMCPなどでn8nやDifyのワークフローを作る使い方はある）。

### ラベルやIssueで動き、PRを作るもの

| 名前 | ホスト型か手元か | きっかけ | 工程を分けるか | 人の関門 |
| --- | --- | --- | --- | --- |
| Orbi | 両方（launchd / systemd の tick） | `ai-ready` ラベル | 計画・実装・テスト → レビュー → マージの関門 | 失敗すると `ai-blocked` |
| Vercel Foreman（eve） | Vercel | `factory` ラベル、メンション、Linear | classifier → analyst → implementer → reviewer（固定） | draft PRで止まる。ラベルを付けた人の権限を確かめる |
| GitHub Agentic Workflows | Actions | GitHubのイベント | frontmatter付きのMarkdownで書く | 書き込みは別のジョブで、許した分だけ |
| OpenAI Symphony | 自前で運用（仕様と参考の実装。製品として保守はしないと報じられた） | Linearのボード | Issueごとに Codex | PRのレビュー |
| Copilot coding agent、Jules、Kiro、Devin など | ホスト型 | Issueの割り当て、ラベル、メンション | 1つのエージェント | PR |
| claude-code-action、OpenHands resolver | Actions | メンション、ラベル | 1つのエージェント | PR |
| TAKT | 手元 | 人がCLIから | YAMLで自由に組む | 確定の入力、権限の判定 |

- エージェントをまとめて動かす道具の一覧（awesome-agent-orchestrators）だけで約290件。Issue → PRまで自分で進めるものだけでも26件
- 「software factory」という呼び名も、2026年に広まった

出典: [Orbi](https://github.com/orbi-build/orbi)、[Vercel: eve software factory](https://vercel.com/kb/guide/eve-software-factory)、[gh-aw safe outputs](https://github.github.com/gh-aw/reference/safe-outputs/)、[OpenAI Symphony](https://openai.com/index/open-source-codex-orchestration-symphony)（開けないときは [Help Net Security の記事](https://www.helpnetsecurity.com/2026/04/28/openai-symphony-codex-orchestration-linear/)）、[Copilot coding agent](https://docs.github.com/en/copilot/using-github-copilot/coding-agent/using-copilot-to-work-on-an-issue)、[awesome-agent-orchestrators](https://github.com/andyrewlee/awesome-agent-orchestrators)

### ありふれていることと、珍しいこと

ありふれている:

- ラベルで動いてPRで終わる形、worktreeで分けること、triage → implement → review のつなぎ方、tickで動かすこと、エージェントをMarkdownとfrontmatterで定義すること

調べた範囲で、見当たらなかったか少なかった:

- 状態をGitHubのラベルとコメントに置き、人もAIも同じ場所で受け渡しながら、使う人が定義したcellのつながりを、AIが実行時に選んでたどり、CLIがスキーマで縛る形（Foremanは順番が固定。TAKTもAIが判定して移るが、状態はTAKTの実行の中にあり、GitHubには結果（PRなど）だけが出る）
- 手元のMacで、ここまで守りを重ねたもの（sandbox、既定で通信なし、置いた秘密をコミットさせない、betterleaks、ラベルを付けた人の確かめ）。同じくらい厚いForemanやgh-awは、ホスト型かActionsの上で動く
- inspectorが、自分の動いた記録から改善のIssueを作ること

骨組み（ラベルで動くIssue → PRの工場）だけでは、もう違いにならない。違いになるのは、上の組み合わせと、サブスクリプションの範囲で手元で回せること。

## Claudeが作れる、プログラム以外のもの

agentcellsに合うかの分かれ目は、テキストのファイルとして書けて、差分でレビューできるか。

- そのまま乗る: 文章（Markdown、LaTeX、Fountain、Ink / Yarn）、スライド（Marp、reveal.js、Slidev）、図（Mermaid、PlantUML、Graphviz）、表やデータ、企画書や仕様、ゲームのルールやシナリオ、設定集、楽譜（ABC、LilyPond、MusicXML）
- コードで描く（描いた結果を見て確かめる手段が要る。[Issue #3](https://github.com/y-hirakaw/agentcells/issues/3)）: SVGの絵、ピクセルアート、生成アート、HTMLとCSSのデザイン、OpenSCADやBlenderのスクリプト、Remotion / Manim の動画、音
- 乗りにくい: 写真のような画像（Claudeは画像を生成するモデルではない）。WordやPowerPointのファイルは作れるが、差分で見にくいので、もとになるテキストを管理して生成する

試すなら、短い文章（今の作りで回るはず）と、SVGの絵（確かめる手段がないところが分かる）。

## チャット、エージェント、パーソナルAIの流れ

AIに任せる範囲と、持たせる文脈が、段階ごとに広がってきた。

1. チャット: 1回ごとの相談相手。文脈は会話の中だけ
2. エージェント: 道具を使い、1つの仕事を何手も続けて進める。人は確かめる側に回る
3. パーソナルAI: 自分のデータと記憶を持ち、生活や仕事の一部を代わりにする

予想（調べたものではなく、見方）:

- 1つのAIから、役割を分けた何人かのAIへ（1人だと自分の誤りに気づきにくい）
- 呼ばれたときだけでなく、ずっと動いていて、必要なときだけ人を呼ぶ形へ
- 人とAIが、同じ記録を見ながら働く形へ。誰が何をなぜしたか、戻せること、任せてよい範囲の線引きが大事になる
- 任せる範囲が広がるほど、安全と信頼が、性能より大事になる

## 人のチームとAI

### 製品: AIに担当と権限を持たせる

- Notion Custom Agents（2026年2月）: チームで使い、決まった時刻やきっかけで動き、エージェントごとに権限を持つ
- Linear: Cursorなどの外のエージェントが、人と同じようにIssueを割り当てられる
- ClickUp: 「AIのチームメイト」として Super Agents を作っている

出典: [Notion: Custom Agents](https://www.notion.com/blog/introducing-custom-agents)、[Linear AI（Sentraの記事。第三者）](https://www.sentra.app/articles/linear-ai)、[ClickUp（Wikipedia。第三者）](https://en.wikipedia.org/wiki/ClickUp)。LinearとClickUpは、公式のページで確かめ直す

### 研究: 段取りがないと、人を増やしても悪くなる

- 人とAIが同じ場所で働く実験（1,482回、人の役は模擬）: 段取りなしに協力者を増やすと成果が下がった。共有の記憶と、決めた人が承認する関門を足すと上がり、3人のチームでいちばん効いた（[arXiv 2606.18413](https://arxiv.org/abs/2606.18413v2)）
- 2,234人の実地実験（広告づくり）: 人とAIのチームは、1人あたりの数が50%多く、文章の質も高めだが、画像の質は人どうしが上。成果物が似たものばかりになる傾向もあった（[arXiv 2503.18238](https://arxiv.org/pdf/2503.18238)）
- 大企業に「チームメイト」のAIを入れた観察: 書かれていない仕事の決まり、AIの役割、信頼と決める権限の配分でぶつかった（[arXiv 2609.29901](https://arxiv.org/abs/2609.29901v1)）
- Anthropicの2026年の報告: 開発は「コードを書く」から「書くエージェントをまとめる」へ。Anthropic社内の調べでは、開発者は仕事の約60%でAIを使うが、「完全に任せられる」と答えたのは0〜20%の仕事（本人の申告）（[Anthropic 2026 Agentic Coding Trends Report（PDF）](https://resources.anthropic.com/hubfs/2026%20Agentic%20Coding%20Trends%20Report.pdf)）

### OpenClaw のチーム

OpenClaw（個人用のAIエージェント）は、AIどうしのチーム（まとめ役と専門役）を足している。人が何人か加わる話ではない。

- 2026年3月の Agent Teams の提案と、その前の要望は、4月に「not planned」で閉じられた
- 2026年10月の時点では、まとめ役と専門役のチーム（`agents team create` など）が、正式な使い方として文書（docs/concepts/multi-agent.md の Team preset）に書かれている
- Claude CLIで動かしたときの報告（#164972）: 最初（10月4日）は、細かく決めた仕事を子に渡し、結果を1つ受け取ることしかできなかった。10月中旬のβで、聞き返しと続きのやり取りは通るようになった。新しいセッションでの再開、子として起こされたまとめ役から自分の専門役へ送ること（確かめ直されていない）、約5分の遅れ、動いている子への指示は、まだ残っている

出典: [#56482](https://github.com/openclaw/openclaw/issues/56482)、[#10010](https://github.com/openclaw/openclaw/issues/10010)、[#164972](https://github.com/openclaw/openclaw/issues/164972)

## 考えたこと

### 主語は「人」や「AI」ではなく「役割と流れ」

agentcellsの芯は、仕事の流れ（役割と受け渡し）を先に決めておき、その役割をAIか人が担うところにある。

- triage、implement、review はAIが担う役割。質問に答える、着手を認める、検品するは、人が担う役割（needs-human、confirm、in-review）
- どちらも、git、Issue、PRという同じ記録の上で受け渡す
- いつか役割ごとに人を割り当てられれば、それで人のチームになる

### 人のチームまで広げると、変数が多すぎる

グラフ（cellのつながり）、人（人数、役割、誰の指示が優先か、誰の承認がどこで効くか）、同時に動かすこと、守りが、掛け算で増える。原因が分からなくなる。研究でも、段取りなしに協力者を増やすと悪くなった。

今のところ安定しそうなのは、「1人＋1AI」を何組か集めて、人どうしが調整する形。責任が人ごとに閉じるため。

### 1人と、仕事を任せたループの束

1人の周りに、仕事の一部ずつを任せたループが何本もある、という働き方がある（たとえば、開発、問い合わせの受け付け、進み具合の管理）。それぞれのループは、従来のプログラムでは書けなかった「よしなに」の部分をAIが担い、迷ったら人に戻す。「1人＋1AI」の「1AI」は、実際にはループの束。

この見方だと:

- agentcellsは、全部を担う仕組みではなく、「何かを作る仕事」を任せるループの1本
- 大事になるのは、ほかのループとのつなぎ目（問い合わせからIssueを立てて入口にする、進み具合をバーンダウンのループが読む、パーソナルAIが判断待ちを知らせる）。GitHubを共有の場にしているので、つなぎ目はかなりできている。足りないとすれば、外から読みやすい形で状態を出すこと（`status` をJSONで出す、など）
- ループに共通する骨組み（きっかけ、AIの判断、守り、人に戻すこと、記録）は、agentcellsで開発の仕事向けに作り込んだもの。ほかのループを作るときの手本にもなる
