//! `~/.agentcells/config.toml` の読み込みと確認。
//!
//! product（どのリポジトリのIssueを、どのリポジトリで作業するか）と、
//! cell（どのラベルを拾い、どのプロンプトで動くか）を書く。

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::home::RepoName;
use crate::labels;
use crate::prompt;
use crate::transition;

/// `agentcells setup` が、設定ファイルがないときに置く雛形。
pub const TEMPLATE: &str = r#"# agentcellsの設定。書き方は https://github.com/y-hirakaw/agentcells を見てください。
#
# operator: cellを使うあなたのGitHubのログイン。あなたがAssigneesに自分を入れ、cellのラベルを付けたIssue/PRだけが動きます
# （ほかの人がラベルやAssigneesを変えても、勝手に始まりません）
#
# operator = "your-login"
#
# product: どのリポジトリのIssueを、どのリポジトリで作業するか
#
# [[product]]
# name = "agentcells"
# issues = "y-hirakaw/agentcells"    # Issueを置くリポジトリ
# repos = ["y-hirakaw/agentcells"]   # 作業するリポジトリ
# cells = ["triage"]                 # このproductで動かすcell
#
# [product.commands]                 # AIが `agentcells cell run <名前>` で実行できるコマンド（これ以外は実行できない）
# test = "npm test"
# build = "npm run build"
#
# protected = ["src/hook.rs", "prompts/"]  # 守るファイル（リポジトリからの相対パス。ディレクトリならその下すべて）。変えたPRは、レビューのcellに特に確かめさせ、検品に回すときにそう書き足す
#
# [product.next]                     # このproductでだけ、cellの進め先を替える（cellはファイル全体で1つずつ定義するため）
# assess = ["stop", "done"]
#
# [product.sandbox]                  # AIが動かすコマンドに開ける場所。標準では、ホームの下は今の仕事の場所しか読めず、外へは通信できない
# read = ["~/.nvm"]                  # ホームの下で読ませる場所（ツールチェーンなど）。開けた場所の秘密は読まれうる
# caches = ["npm"]                   # ビルドツールのキャッシュ（gradle npm yarn pnpm cargo maven pip go）。agentcellsが持つ場所に向ける
# network = ["registry.npmjs.org"]   # 通信してよい先。取得元のホストを具体的に書く（*.amazonaws.com のような広いワイルドカードは、外へ送る道になる）
# read_placed = []                   # files/ から置いたファイルのうち、ビルドに読ませるもの（リポジトリからの相対パス）
# bind_localhost = false             # テストが localhost でサーバーを開けるようにする（手元のほかのサーバーにもつながるようになる）
#
# cell: どのラベルが付いたIssue/PRを拾い、どのプロンプトで動くか。1つのcellを1つのファイル（このファイルの隣の
# cells/<名前>.md）に書く。頭の +++ の行で囲んだところに設定を、そのあとに追加の指示を書く。名前はファイルの名前
#
# cells/triage.md:
#   +++
#   on = "issue"                     # issue か pull_request（両方なら ["issue", "pull_request"]）
#   label = "ac:triage"              # 拾うラベル（ac: で始める）
#   next = ["implement", "done"]     # 進めてよい先（cellの名前、done、stop）。needs-human はいつでも選べる
#   review = false                   # true にすると、PRへのレビューとして指摘を行ごとに投稿する（on は pull_request だけ）
#   max_rounds = 3                   # 往復の上限（1〜3。省略すると3）
#   model = "sonnet"                 # 使うモデル（省略すると、Claude Codeの既定のもの）
#   confirm = false                  # true にすると、人間が着手を認めるまで次のcellに進めない（assess は省略すると true）
#   timeout_minutes = 30             # 1回の実行の上限（省略すると30）
#   commands = ["test"]              # 使えるコマンド（productの commands の名前から選ぶ。省略するとすべて、[] なら使えない）
#   +++
#
#   （追加の指示。標準のcell（triage、implement、review、assess）なら、書かなくても標準の役割で動く。
#   それ以外のcellには、役割と、何を作るかを書く。例: https://github.com/y-hirakaw/agentcells/tree/main/examples/cells）
#
# このファイルに [[cell]] として書くこともできる（name と、追加の指示のファイル prompt を足す）
#
# inspector: tickの記録（~/.agentcells/logs/events.jsonl）を1日に1回調べ、ラインの不具合（同じcellが続けて動いた、
# AIの実行が失敗した、など）を見つけて、原因と直し方の案を書く。書かなければ動かさない
#
# [inspector]
# issues = "your-login/agentcells"   # agentcells自体の不具合をIssueにして出す先（書かなければ手元に残すだけ。本家に出すと、ほかの人にも見える）
# every_hours = 24                   # 何時間ごとに動かすか
# model = "sonnet"
#
# usage: 使用量の上限の手前で止まる。5時間か7日の上限に対する使用量がこれ以上になったら、その上限が戻るまで
# 新しく始めない（動いているものは最後まで動く）。あなたが自分でClaude Codeを使う分を残すため
#
# [usage]
# pause_at = 80                      # %（省略すると80。100にすると、上限に当たるまで動く。7日の上限で超えると、数日止まることがある。止まっている間は status に出る）
#
# github_app: GitHubへの書き込み（コメント、ラベル、push、PR）に使うGitHub App（推奨）
# 書かなければ、ghにログインしているあなたのアカウントで書き込みます。
# Appで書き込むと、コメントやPRが「<Appの名前>[bot]」になり、人間の発言と見分けられます。
#
# [github_app]
# id = "Iv23li..."                   # Client ID（App IDでもよい）
# private_key = "github-app.pem"     # 秘密鍵（このファイルからの相対パス。権限は600にする）
"#;

/// ファイルの全体にかかる項目（見出しの外に書くもの）。
const TOP_LEVEL_KEYS: &[&str] = &["operator"];

/// 往復の上限。これより多く往復させず、人間に回す。
pub const MAX_ROUNDS_LIMIT: u32 = 3;
const DEFAULT_MAX_ROUNDS: u32 = MAX_ROUNDS_LIMIT;
const DEFAULT_TIMEOUT_MINUTES: u32 = 30;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// cellを使う人のGitHubのログイン。この人が自分で決めたIssue/PRだけを動かす
    #[serde(default)]
    pub operator: Option<String>,
    #[serde(default, rename = "product")]
    pub products: Vec<Product>,
    #[serde(default, rename = "cell")]
    pub cells: Vec<Cell>,
    /// 書き込みに使うGitHub App。なければghにログインしているアカウントで書き込む
    #[serde(default)]
    pub github_app: Option<GithubApp>,
    /// tickの記録を調べて、ラインの不具合を見つける（書かなければ動かさない）
    #[serde(default)]
    pub inspector: Option<Inspector>,
    /// 使用量の上限の手前で止まる（`[usage]`）
    #[serde(default)]
    pub usage: UsageLimit,
}

/// 使用量の上限の手前で止まる設定（`[usage]`）。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageLimit {
    /// 使用量（5時間や7日の上限に対する割合。%）がこれ以上になったら、その上限が戻るまで新しく始めない。
    /// 使う人が自分でClaude Codeを使う分を残すため。100にすると、上限に当たるまで動く
    #[serde(default = "default_pause_at")]
    pub pause_at: u8,
}

impl Default for UsageLimit {
    fn default() -> Self {
        UsageLimit {
            pause_at: default_pause_at(),
        }
    }
}

fn default_pause_at() -> u8 {
    80
}

/// inspectorの設定（`[inspector]`）。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Inspector {
    /// agentcells自体の不具合を、Issueにして出すリポジトリ。書かなければ、手元に残すだけ
    #[serde(default)]
    pub issues: Option<RepoName>,
    /// 何時間ごとに動かすか（tickの最後に、前回からこれだけたっていれば動く）
    #[serde(default = "default_inspector_hours")]
    pub every_hours: u32,
    /// `claude -p` の `--model`
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default = "default_timeout_minutes")]
    pub timeout_minutes: u32,
}

fn default_inspector_hours() -> u32 {
    24
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubApp {
    /// Client IDかApp ID
    #[serde(deserialize_with = "string_or_number")]
    pub id: String,
    /// 秘密鍵のファイル。読み込んだあとは絶対パスになっている
    pub private_key: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Product {
    pub name: String,
    /// Issueを置くリポジトリ
    pub issues: RepoName,
    /// 作業するリポジトリ
    pub repos: Vec<RepoName>,
    /// このproductで動かすcellの名前
    pub cells: Vec<String>,
    /// AIが `agentcells cell run <名前>` で実行できるコマンド。AIは、ここに書いたもの以外は実行できない
    #[serde(default)]
    pub commands: BTreeMap<String, String>,
    /// AIが動かすコマンド（ビルドやテストを含む）に、読ませる場所と通信させる先
    #[serde(default)]
    pub sandbox: Sandbox,
    /// このproductでだけ、cellの進め先（`next`）を替える（cellの名前 → 進め先）。cellはファイル全体で
    /// 1つずつ定義するので、productごとに進め先を変えたいときに使う
    #[serde(default)]
    pub next: BTreeMap<String, Vec<String>>,
    /// 守るファイル（hookや権限、CIの設定など、AIの守りそのもの）。リポジトリからの相対パスで、その下すべても含む。変えたPRは、レビューのcellに特に確かめさせ、検品に回すときにCLIがそう書き足す
    #[serde(default)]
    pub protected: Vec<String>,
}

impl Product {
    /// `cell` が `agentcells cell run` で使えるコマンド。cellの `commands` で選んだものだけ（書かなければすべて）。
    pub fn commands_for(&self, cell: &Cell) -> BTreeMap<String, String> {
        self.commands
            .iter()
            .filter(|(name, _)| {
                cell.commands
                    .as_ref()
                    .is_none_or(|names| names.contains(name))
            })
            .map(|(name, command)| (name.clone(), command.clone()))
            .collect()
    }

    /// `path`（リポジトリからの相対パス）が、守るファイルか。
    /// 書いたパスそのものと、その下すべてに当たる（`/` で終えるかどうかによらない。ディレクトリを
    /// submoduleやファイルに置き換えたときも当たるように）。大文字と小文字は区別しない（macOSでは同じファイルになる）。
    pub fn protects(&self, path: &str) -> bool {
        let path = path.to_ascii_lowercase();
        self.protected.iter().any(|entry| {
            let entry = entry
                .strip_suffix('/')
                .unwrap_or(entry)
                .to_ascii_lowercase();
            path.strip_prefix(&entry)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
        })
    }
}

/// AIが動かすコマンドは、sandboxの中で動く。標準では、ホームの下は今の仕事の場所しか読めず、外へは通信できない。
/// ここに書いた分だけ開ける（開けた場所の秘密が読まれても防げないので、書くのは使う人の責任）。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sandbox {
    /// ホームの下で読ませる場所（ツールチェーンやキャッシュなど）。`~/` で始めるか、絶対パスで書く。
    /// 読み込んだあとは絶対パスになっている
    #[serde(default)]
    pub read: Vec<PathBuf>,
    /// ビルドツールのキャッシュ（`gradle`、`npm` など）。使う人のキャッシュではなく、agentcellsが持つ
    /// productごとの場所に向けて、書かせる
    #[serde(default)]
    pub caches: Vec<String>,
    /// ホームの下で書かせる（読ませもする）場所。書き方は `read` と同じ。上級の設定で、勧めない:
    /// AIが書き換えたビルドがここに置いたコードを、使う人が手元（sandboxの外）で動かすツールが実行しうる
    #[serde(default)]
    pub write: Vec<PathBuf>,
    /// worktreeに置いたファイルのうち、ビルドやテストに読ませるもの（リポジトリからの相対パス）。
    /// 読ませると、AIが書き換えたビルドを通して中身を取り出せるので、要るものだけにする
    #[serde(default)]
    pub read_placed: Vec<PathBuf>,
    /// 通信してよい先（`registry.npmjs.org` のようなドメイン）。取得元のホストを具体的に書く。
    /// `*.amazonaws.com` のようなワイルドカードは、誰でも書ける置き場所を含み、外へ送れてしまう
    #[serde(default)]
    pub network: Vec<String>,
    /// テストが手元（localhost）でサーバーを開けるようにする
    #[serde(default)]
    pub bind_localhost: bool,
}

impl Sandbox {
    /// 開ける場所と、その項目の名前（`read` か `write`）。
    pub fn opened(&self) -> impl Iterator<Item = (&'static str, &PathBuf)> {
        self.read
            .iter()
            .map(|p| ("read", p))
            .chain(self.write.iter().map(|p| ("write", p)))
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cell {
    pub name: String,
    /// 扱うもの。`"issue"` のように1つでも、`["issue", "pull_request"]` のようにリストでも書ける
    #[serde(deserialize_with = "one_or_many")]
    pub on: Vec<Target>,
    /// 拾うラベル
    pub label: String,
    /// 標準のプロンプトに足す、追加の指示のファイル。読み込んだあとは絶対パスになっている。
    /// 標準のcell（triage、implement、review、assess）なら省略できる
    #[serde(default)]
    pub prompt: Option<PathBuf>,
    /// 進めてよい先（cellの名前か `done`）。`needs-human` は書かなくても選べる
    #[serde(default)]
    pub next: Vec<String>,
    /// PRへのレビューとして、指摘をコードの行ごとに投稿する
    #[serde(default)]
    pub review: bool,
    #[serde(default = "default_max_rounds")]
    pub max_rounds: u32,
    #[serde(default = "default_timeout_minutes")]
    pub timeout_minutes: u32,
    /// `claude -p` の `--model`（`sonnet` など）。書かなければ、Claude Codeの既定のもの
    #[serde(default)]
    pub model: Option<String>,
    /// 次のcellに進む前に、人間に着手してよいかを確かめる（人間の返事で動いたときだけ、次のcellと
    /// `stop` を選べる）。書かなければ、assess だけ確かめる
    #[serde(default)]
    pub confirm: Option<bool>,
    /// `agentcells cell run` で使える名前（productの `commands` から選ぶ）。書かなければ、productのものをすべて使える
    #[serde(default)]
    pub commands: Option<Vec<String>>,
    /// `cells/<名前>.md` の頭に書いたcellか（`[[cell]]` ではなく）
    #[serde(skip)]
    pub from_file: bool,
}

impl Cell {
    /// 追加の指示（`prompt` のファイルの中身）。cellのファイルなら、頭の設定を除いた本文。
    pub fn extra_prompt(&self) -> Result<Option<String>> {
        let Some(path) = &self.prompt else {
            return Ok(None);
        };
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::new(format!("追加の指示 {} を読めません: {e}", path.display())))?;
        Ok(Some(match front_matter(&text) {
            Ok(Some((_, body))) => body.to_string(),
            _ => text,
        }))
    }

    /// 次のcellに進む前に、人間に確かめるか。
    pub fn confirms(&self) -> bool {
        self.confirm.unwrap_or(self.name == "assess")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    Issue,
    PullRequest,
}

impl Target {
    pub fn as_str(self) -> &'static str {
        match self {
            Target::Issue => "issue",
            Target::PullRequest => "pull_request",
        }
    }
}

fn one_or_many<'de, D>(deserializer: D) -> std::result::Result<Vec<Target>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(Target),
        Many(Vec<Target>),
    }
    Ok(match OneOrMany::deserialize(deserializer)? {
        OneOrMany::One(t) => vec![t],
        OneOrMany::Many(ts) => ts,
    })
}

fn string_or_number<'de, D>(deserializer: D) -> std::result::Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrNumber {
        String(String),
        Number(u64),
    }
    Ok(match StringOrNumber::deserialize(deserializer)? {
        StringOrNumber::String(s) => s,
        StringOrNumber::Number(n) => n.to_string(),
    })
}

fn default_max_rounds() -> u32 {
    DEFAULT_MAX_ROUNDS
}

fn default_timeout_minutes() -> u32 {
    DEFAULT_TIMEOUT_MINUTES
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            Error::new(format!("{} を読めません: {e}", path.display()))
                .with_hint("`agentcells setup` を実行すると雛形が置かれます")
        })?;
        let base = path.parent().unwrap_or(Path::new("."));
        Self::parse(&text, base).map_err(|e| {
            let message = format!("{} に問題があります:\n{}", path.display(), e.message);
            Error {
                message,
                hint: e.hint,
            }
        })
    }

    /// 設定を読むが、中身は確かめない。設定に問題があっても、ほかの確認（`doctor` のGitHubの確認など）を進めるために使う。
    pub fn load_unchecked(path: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        Self::read(&text, path.parent().unwrap_or(Path::new("."))).ok()
    }

    /// TOMLを読み、ファイルのパスを `base` からの相対パスとして解決する。
    fn read(text: &str, base: &Path) -> Result<Self> {
        let mut config: Config = toml::from_str(text).map_err(|e| {
            let message = e.to_string().trim_end().to_string();
            // TOMLでは、[github_app] のような見出しのあとの行は、その見出しの中の項目になる。
            // ファイルの全体にかかる項目を最後に書き足すと、見出しの中の知らない項目として断られる
            let hint = match TOP_LEVEL_KEYS
                .iter()
                .find(|key| message.contains(&format!("unknown field `{key}`")))
            {
                Some(key) => format!(
                    "{key} は、ファイルのいちばん上（最初の [ で始まる行より前）に書いてください。[...] の見出しのあとに書くと、その見出しの中の項目として読まれます"
                ),
                None => "TOMLの書き方と項目名を確かめてください".to_string(),
            };
            Error::new(message).with_hint(hint)
        })?;
        for cell in &mut config.cells {
            if let Some(prompt) = &mut cell.prompt {
                *prompt = base.join(&*prompt);
            }
        }
        config.cells.extend(cell_files(&base.join(CELLS_DIR))?);
        if let Some(app) = &mut config.github_app {
            app.private_key = base.join(&app.private_key);
        }
        let user_home = std::env::var_os("HOME").map(PathBuf::from);
        for product in &mut config.products {
            let sandbox = &mut product.sandbox;
            for path in sandbox.read.iter_mut().chain(sandbox.write.iter_mut()) {
                if let (Ok(rest), Some(user_home)) = (path.strip_prefix("~"), &user_home) {
                    *path = user_home.join(rest);
                }
            }
        }
        Ok(config)
    }

    /// 設定を読んで確かめる。プロンプトのパスは `base` からの相対パスとして解決する。
    pub fn parse(text: &str, base: &Path) -> Result<Self> {
        let config = Self::read(text, base)?;
        let problems = config.problems();
        if problems.is_empty() {
            Ok(config)
        } else {
            let list = problems
                .iter()
                .map(|p| format!("  - {p}"))
                .collect::<Vec<_>>()
                .join("\n");
            Err(Error::new(list).with_hint("上の項目を直してから、もう一度実行してください"))
        }
    }

    /// Issueを置くリポジトリと作業するリポジトリをすべて、重ならないように返す。
    pub fn repos(&self) -> BTreeSet<&RepoName> {
        self.products
            .iter()
            .flat_map(|p| std::iter::once(&p.issues).chain(&p.repos))
            .collect()
    }

    pub fn cell(&self, name: &str) -> Option<&Cell> {
        self.cells.iter().find(|c| c.name == name)
    }

    /// どのproductの `cells` にも入っていないcell（動かない）。定義した順で返す。
    /// `next` の先にあるだけでは、使っていると数えない。
    pub fn unused_cells(&self) -> Vec<&Cell> {
        self.cells
            .iter()
            .filter(|c| !self.products.iter().any(|p| p.cells.contains(&c.name)))
            .collect()
    }

    /// productで動くcell。productの `next` に書いてあれば、進め先をそれに替える。
    pub fn cell_in(&self, product: &Product, name: &str) -> Option<Cell> {
        let mut cell = self.cell(name)?.clone();
        if let Some(next) = product.next.get(name) {
            cell.next = next.clone();
        }
        Some(cell)
    }

    /// cellの進め先の問題（自分自身、定義されていないcell）。
    fn next_problems(&self, c: &Cell) -> Vec<String> {
        let mut problems = Vec::new();
        for next in &c.next {
            if next == &c.name {
                problems.push(format!("cell「{}」の next に自分自身があります", c.name));
            } else if !transition::is_reserved(next) && self.cell(next).is_none() {
                problems.push(format!(
                    "cell「{}」の next の「{next}」が定義されていません（cellの名前か {} か {} を書きます。cellは cells/<名前>.md か [[cell]] で定義します）",
                    c.name,
                    transition::DONE,
                    transition::STOP
                ));
            }
        }
        problems
    }

    /// 見つかった問題をすべて返す。1つずつ直しては実行し直す手間を減らすため、最初の1つで止めない。
    fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();

        match &self.operator {
            Some(login) if !is_valid_login(login) => problems.push(format!(
                "operator の「{login}」はGitHubのログインではありません（英数字と -、39文字まで）"
            )),
            None if !self.products.is_empty() => problems.push(
                "operator に、cellを使うあなたのGitHubのログインを、ファイルのいちばん上（最初の [ で始まる行より前）に書いてください（あなたが自分で決めたIssue/PRだけを動かすため）"
                    .to_string(),
            ),
            _ => {}
        }

        let mut product_names = HashSet::new();
        for p in &self.products {
            if !is_valid_name(&p.name) {
                problems.push(format!(
                    "productの名前「{}」に使えない文字があります（英小文字・数字・-）",
                    p.name
                ));
            }
            if !product_names.insert(p.name.as_str()) {
                problems.push(format!("productの名前「{}」が重複しています", p.name));
            }
            if p.repos.is_empty() {
                problems.push(format!(
                    "product「{}」の repos が空です。作業するリポジトリを書いてください",
                    p.name
                ));
            }
            if p.cells.is_empty() {
                problems.push(format!(
                    "product「{}」の cells が空です。動かすcellを書いてください",
                    p.name
                ));
            }
            for (name, command) in &p.commands {
                if !is_valid_name(name) {
                    problems.push(format!(
                        "product「{}」のコマンドの名前「{name}」に使えない文字があります（英小文字・数字・-）",
                        p.name
                    ));
                }
                if command.trim().is_empty() {
                    problems.push(format!("product「{}」のコマンド「{name}」が空です", p.name));
                }
            }
            for (key, path) in p.sandbox.opened() {
                if !path.is_absolute() || has_parent_dir(path) {
                    problems.push(format!(
                        "product「{}」の sandbox.{key} の {} は、`~/` で始めるか絶対パスで書いてください（`..` は使えません）",
                        p.name,
                        path.display()
                    ));
                }
            }
            for path in &p.sandbox.read_placed {
                if path.as_os_str().is_empty() || path.is_absolute() || has_parent_dir(path) {
                    problems.push(format!(
                        "product「{}」の sandbox.read_placed の「{}」は、リポジトリからの相対パスで書いてください（`..` は使えません）",
                        p.name,
                        path.display()
                    ));
                }
            }
            for entry in &p.protected {
                // gitのパスと同じ形でないと、何にも当たらないまま守ったつもりになる
                let path = entry.strip_suffix('/').unwrap_or(entry);
                if path.split('/').any(|part| {
                    matches!(part, "" | "." | "..") || part.contains(['\\', '*', '?', '['])
                }) {
                    problems.push(format!(
                        "product「{}」の protected の「{entry}」は、リポジトリからの相対パスで書いてください（例: src/hook.rs、prompts/。`.` や `..`、`\\`、`*` などのワイルドカードは使えません）",
                        p.name
                    ));
                }
            }
            for name in &p.sandbox.caches {
                if !crate::caches::is_known(name) {
                    problems.push(format!(
                        "product「{}」の sandbox.caches の「{name}」は知らないキャッシュです（使えるのは {}）",
                        p.name,
                        crate::caches::names().join("、")
                    ));
                }
            }
            for domain in &p.sandbox.network {
                if !is_valid_domain(domain) {
                    problems.push(format!(
                        "product「{}」の sandbox.network の「{domain}」はドメインではありません（例: registry.npmjs.org、*.gradle.org）",
                        p.name
                    ));
                }
            }
            for name in p.next.keys() {
                if !p.cells.contains(name) {
                    problems.push(format!(
                        "product「{}」の next の「{name}」が、このproductの cells にありません",
                        p.name
                    ));
                }
            }
            for name in &p.cells {
                let Some(cell) = self.cell_in(p, name) else {
                    problems.push(format!(
                        "product「{}」のcell「{name}」が定義されていません（cells/<名前>.md か [[cell]] で定義します）",
                        p.name
                    ));
                    continue;
                };
                if p.next.contains_key(name) {
                    problems.extend(
                        self.next_problems(&cell)
                            .into_iter()
                            .map(|e| format!("product「{}」の next: {e}", p.name)),
                    );
                }
                // 遷移先のcellも、同じproductで動いていないと拾われない。
                for next in &cell.next {
                    if self.cell(next).is_some() && !p.cells.contains(next) {
                        problems.push(format!(
                            "product「{}」のcell「{name}」は「{next}」に進めますが、「{next}」がこのproductの cells にありません（このproductでだけ進め先を変えるなら、[product.next] に書きます）",
                            p.name
                        ));
                    }
                }
                if cell.confirms()
                    && !cell
                        .next
                        .iter()
                        .any(|n| n == transition::STOP || self.cell(n).is_some())
                {
                    problems.push(format!(
                        "product「{}」のcell「{name}」は着手を確かめるので、next に、認められたあとに進むcellか {} を書いてください",
                        p.name,
                        transition::STOP
                    ));
                }
            }
        }

        let mut cell_names = HashSet::new();
        let mut cell_labels = HashSet::new();
        for c in &self.cells {
            if !is_valid_name(&c.name) {
                problems.push(format!(
                    "cellの名前「{}」に使えない文字があります（英小文字・数字・-）",
                    c.name
                ));
            }
            if !cell_names.insert(c.name.as_str()) {
                problems.push(format!("cellの名前「{}」が重複しています", c.name));
            }
            if transition::is_reserved(&c.name) {
                problems.push(format!(
                    "cellの名前「{}」は遷移先として予約されています。別の名前にしてください",
                    c.name
                ));
            }
            problems.extend(self.next_problems(c));
            if !c.label.starts_with(labels::PREFIX) || c.label.len() == labels::PREFIX.len() {
                problems.push(format!(
                    "cell「{}」のラベル「{}」は {} で始めてください",
                    c.name,
                    c.label,
                    labels::PREFIX
                ));
            }
            if labels::is_system(&c.label) {
                problems.push(format!(
                    "cell「{}」のラベル「{}」はagentcellsが使うラベルです。別の名前にしてください",
                    c.name, c.label
                ));
            }
            if !cell_labels.insert(c.label.as_str()) {
                problems.push(format!(
                    "ラベル「{}」を複数のcellが拾おうとしています",
                    c.label
                ));
            }
            match &c.prompt {
                Some(prompt) if !prompt.is_file() => problems.push(format!(
                    "cell「{}」のプロンプト {} がありません",
                    c.name,
                    prompt.display()
                )),
                None if prompt::builtin(&c.name).is_none() => problems.push(format!(
                    "cell「{}」には標準のプロンプトがないので、prompt にプロンプトのファイルを書いてください",
                    c.name
                )),
                _ => {}
            }
            for name in c.commands.iter().flatten() {
                let users: Vec<&Product> = self
                    .products
                    .iter()
                    .filter(|p| p.cells.contains(&c.name))
                    .collect();
                if !users.is_empty() && !users.iter().any(|p| p.commands.contains_key(name)) {
                    problems.push(format!(
                        "cell「{}」の commands の「{name}」が、このcellを使うproductの commands にありません",
                        c.name
                    ));
                }
            }
            if c.review && c.on != [Target::PullRequest] {
                problems.push(format!(
                    "cell「{}」は review = true なので、on は \"pull_request\" だけにしてください",
                    c.name
                ));
            }
            if c.on.is_empty() {
                problems.push(format!(
                    "cell「{}」の on が空です。issue か pull_request を書いてください",
                    c.name
                ));
            }
            if !(1..=MAX_ROUNDS_LIMIT).contains(&c.max_rounds) {
                problems.push(format!(
                    "cell「{}」の max_rounds は1から{MAX_ROUNDS_LIMIT}までにしてください",
                    c.name
                ));
            }
            if let Some(model) = &c.model
                && !is_valid_model(model)
            {
                problems.push(format!(
                    "cell「{}」の model「{model}」に使えない文字があります（英数字・-・.・_・[]）",
                    c.name
                ));
            }
            if c.timeout_minutes == 0 {
                problems.push(format!(
                    "cell「{}」の timeout_minutes は1以上にしてください",
                    c.name
                ));
            }
        }
        if !(1..=100).contains(&self.usage.pause_at) {
            problems.push("[usage] の pause_at は1〜100（%）にしてください".to_string());
        }
        if let Some(inspector) = &self.inspector {
            if inspector.every_hours == 0 {
                problems.push("[inspector] の every_hours は1以上にしてください".to_string());
            }
            if inspector.timeout_minutes == 0 {
                problems.push("[inspector] の timeout_minutes は1以上にしてください".to_string());
            }
            if let Some(model) = &inspector.model
                && !is_valid_model(model)
            {
                problems.push(format!(
                    "[inspector] の model「{model}」に使えない文字があります（英数字・-・.・_・[]）"
                ));
            }
        }
        if let Some(app) = &self.github_app {
            if app.id.trim().is_empty() {
                problems.push(
                    "[github_app] の id が空です。GitHub AppのClient IDを書いてください"
                        .to_string(),
                );
            }
            if !app.private_key.is_file() {
                problems.push(format!(
                    "[github_app] の秘密鍵 {} がありません（GitHub Appの設定画面で作れます）",
                    app.private_key.display()
                ));
            }
        }
        problems
    }
}

fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// `claude --model` に渡す名前（`sonnet`、`claude-sonnet-5-5`、`opus[1m]` など）。引数として渡すので、
/// `-` で始まるものは断る。
fn is_valid_model(model: &str) -> bool {
    !model.is_empty()
        && !model.starts_with('-')
        && model
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-._[]".contains(c))
}

/// GitHubのログイン（英数字と -、39文字まで。- で始まったり終わったりしない）。
fn is_valid_login(login: &str) -> bool {
    !login.is_empty()
        && login.len() <= 39
        && !login.starts_with('-')
        && !login.ends_with('-')
        && login.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// cellのファイルを置くディレクトリ（設定ファイルからの相対パス）。
pub const CELLS_DIR: &str = "cells";

/// `cells/` の、頭に設定（`+++` で囲んだTOML）を書いたmdファイルを、cellとして読む。名前はファイルの名前。
/// 頭のないファイルは、`[[cell]]` の `prompt` から指す追加の指示なので、読まない。
fn cell_files(dir: &Path) -> Result<Vec<Cell>> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(Vec::new());
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        // . で始まるもの（エディタの一時ファイルや、macOSが作る ._<名前>.md など）は読まない
        .filter(|p| {
            p.extension().is_some_and(|ext| ext == "md")
                && !p
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with('.'))
                && p.is_file()
        })
        .collect();
    paths.sort();
    let mut cells = Vec::new();
    for path in paths {
        let fail = |message: String| Error::new(format!("{}: {message}", path.display()));
        let text = std::fs::read_to_string(&path).map_err(|e| fail(format!("読めません: {e}")))?;
        let Some((head, body)) = front_matter(&text).map_err(fail)? else {
            continue;
        };
        let mut table: toml::Table =
            toml::from_str(head).map_err(|e| fail(e.to_string().trim_end().to_string()))?;
        for key in ["name", "prompt"] {
            if table.contains_key(key) {
                return Err(fail(format!(
                    "{key} は書けません（名前はファイルの名前、指示はこのファイルの本文です）"
                )));
            }
        }
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| fail("ファイルの名前を読めません".to_string()))?;
        table.insert("name".to_string(), name.into());
        let mut cell: Cell = toml::Value::Table(table)
            .try_into()
            .map_err(|e: toml::de::Error| fail(e.to_string().trim_end().to_string()))?;
        // 標準の役割がないcellは、本文（役割の指示）がないと、何をするかが分からないまま動く
        if body.trim().is_empty() && prompt::builtin(&cell.name).is_none() {
            return Err(fail(format!(
                "cell「{}」には標準の役割がないので、頭の設定のあとに、役割と何をするかを書いてください",
                cell.name
            )));
        }
        cell.prompt = Some(path.clone());
        cell.from_file = true;
        cells.push(cell);
    }
    Ok(cells)
}

/// ファイルの頭の、`+++` の行で囲んだ設定と、そのあとの本文。頭がなければ `None`。閉じていなければエラー。
/// 行の終わりの空白と、ファイルの始めのBOMは見ない（エディタによって付くため）。
fn front_matter(text: &str) -> std::result::Result<Option<(&str, &str)>, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some((first, rest)) = text.split_once('\n') else {
        return Ok(None);
    };
    if first.trim_end() != "+++" {
        return Ok(None);
    }
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "+++" {
            let body = &rest[offset + line.len()..];
            return Ok(Some((
                &rest[..offset],
                body.trim_start_matches(['\r', '\n']),
            )));
        }
        offset += line.len();
    }
    Err("頭の設定が +++ の行で閉じていません".to_string())
}

fn has_parent_dir(path: &Path) -> bool {
    path.components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
}

/// `example.com` か `*.example.com`。ポートやスキームは書けない。
fn is_valid_domain(domain: &str) -> bool {
    let host = domain.strip_prefix("*.").unwrap_or(domain);
    host.contains('.')
        && host.split('.').all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
operator = "y-hirakaw"

[[product]]
name = "agentcells"
issues = "y-hirakaw/agentcells"
repos = ["y-hirakaw/agentcells"]
cells = ["triage", "review"]

[[cell]]
name = "triage"
on = "issue"
label = "ac:triage"
prompt = "cells/triage.md"
next = ["review", "done"]

[[cell]]
name = "review"
on = "pull_request"
label = "ac:review"
prompt = "cells/review.md"
max_rounds = 2
timeout_minutes = 10
"#;

    #[test]
    fn on_accepts_a_list() {
        let text = VALID.replace(
            "on = \"pull_request\"",
            "on = [\"issue\", \"pull_request\"]",
        );
        let dir = base_with_prompts(&["triage.md", "review.md"]);
        let config = Config::parse(&text, dir.path()).unwrap();
        assert_eq!(
            config.cell("review").unwrap().on,
            vec![Target::Issue, Target::PullRequest]
        );
        let empty = VALID.replace("on = \"pull_request\"", "on = []");
        assert!(problems_of(&empty).contains("on が空"));
    }

    #[test]
    fn next_defaults_to_empty() {
        let dir = base_with_prompts(&["triage.md", "review.md"]);
        let config = Config::parse(VALID, dir.path()).unwrap();
        assert_eq!(config.cell("triage").unwrap().next, vec!["review", "done"]);
        assert!(config.cell("review").unwrap().next.is_empty());
    }

    #[test]
    fn review_cells_must_be_on_pull_requests_only() {
        let ok = VALID.replace("max_rounds = 2", "max_rounds = 2\nreview = true");
        let dir = base_with_prompts(&["triage.md", "review.md"]);
        assert!(
            Config::parse(&ok, dir.path())
                .unwrap()
                .cell("review")
                .unwrap()
                .review
        );
        assert!(
            !Config::parse(&ok, dir.path())
                .unwrap()
                .cell("triage")
                .unwrap()
                .review
        );

        let bad = VALID.replace(
            "next = [\"review\", \"done\"]",
            "next = [\"review\", \"done\"]\nreview = true",
        );
        assert!(problems_of(&bad).contains("review = true"));
    }

    #[test]
    fn cells_can_choose_a_model() {
        let ok = VALID.replace(
            "max_rounds = 2",
            "max_rounds = 2\nmodel = \"claude-sonnet-5-5\"",
        );
        let dir = base_with_prompts(&["triage.md", "review.md"]);
        let config = Config::parse(&ok, dir.path()).unwrap();
        assert!(
            config
                .cells
                .iter()
                .any(|c| c.model.as_deref() == Some("claude-sonnet-5-5"))
        );
        for bad in ["--dangerously-skip-permissions", "", "son net"] {
            let text = VALID.replace(
                "max_rounds = 2",
                &format!("max_rounds = 2\nmodel = \"{bad}\""),
            );
            assert!(problems_of(&text).contains("model"), "{bad}");
        }
    }

    #[test]
    fn max_rounds_is_capped_at_three() {
        let ok = VALID.replace("max_rounds = 2", "max_rounds = 3");
        let dir = base_with_prompts(&["triage.md", "review.md"]);
        assert!(Config::parse(&ok, dir.path()).is_ok());
        let over = VALID.replace("max_rounds = 2", "max_rounds = 4");
        assert!(problems_of(&over).contains("1から3まで"));
    }

    #[test]
    fn rejects_bad_next() {
        let text = VALID.replace(
            "next = [\"review\", \"done\"]",
            "next = [\"triage\", \"deploy\", \"done\", \"needs-human\"]",
        );
        let message = problems_of(&text);
        assert!(message.contains("自分自身"), "{message}");
        assert!(
            message.contains("「deploy」が定義されていません"),
            "{message}"
        );
        assert!(!message.contains("「done」"), "{message}");
        assert!(!message.contains("「needs-human」"), "{message}");
    }

    #[test]
    fn confirming_cells_need_somewhere_to_go_after_approval() {
        let text = VALID.replace(
            "next = [\"review\", \"done\"]",
            "next = [\"review\", \"done\"]\nconfirm = true",
        );
        let dir = base_with_prompts(&["triage.md", "review.md"]);
        assert!(Config::parse(&text, dir.path()).is_ok());
        let text = text.replace("next = [\"review\", \"done\"]", "next = [\"done\"]");
        let message = problems_of(&text);
        assert!(
            message.contains("cell「triage」は着手を確かめる"),
            "{message}"
        );
        let text = text.replace("next = [\"done\"]", "next = [\"stop\", \"done\"]");
        assert!(Config::parse(&text, dir.path()).is_ok());
    }

    #[test]
    fn products_can_change_where_a_cell_goes_next() {
        // review は triage に進めるが、このproductでは止める
        let text = VALID
            .replace("cells = [\"triage\", \"review\"]", "cells = [\"review\"]")
            .replace(
                "[[cell]]\nname = \"triage\"",
                "[product.next]\nreview = [\"stop\", \"done\"]\n\n[[cell]]\nname = \"triage\"",
            )
            .replace(
                "timeout_minutes = 10",
                "timeout_minutes = 10\nnext = [\"triage\", \"done\"]",
            );
        let dir = base_with_prompts(&["triage.md", "review.md"]);
        let config = Config::parse(&text, dir.path()).unwrap();
        let product = &config.products[0];
        assert_eq!(config.cell("review").unwrap().next, ["triage", "done"]);
        assert_eq!(
            config.cell_in(product, "review").unwrap().next,
            ["stop", "done"]
        );

        // 書き替えた先も確かめる
        let bad = text.replace("review = [\"stop\", \"done\"]", "review = [\"deploy\"]");
        assert!(
            problems_of(&bad)
                .contains("product「agentcells」の next: cell「review」の next の「deploy」")
        );
        let bad = text.replace("review = [\"stop\", \"done\"]", "review = [\"triage\"]");
        assert!(problems_of(&bad).contains("「triage」がこのproductの cells にありません"));
        // このproductで動かないcellは書けない
        let bad = text.replace(
            "review = [\"stop\", \"done\"]",
            "review = [\"done\"]\ntriage = [\"done\"]",
        );
        assert!(
            problems_of(&bad).contains("next の「triage」が、このproductの cells にありません")
        );
    }

    #[test]
    fn rejects_next_outside_the_product() {
        let text = VALID.replace("cells = [\"triage\", \"review\"]", "cells = [\"triage\"]");
        let message = problems_of(&text);
        assert!(
            message.contains("「review」がこのproductの cells にありません"),
            "{message}"
        );
    }

    #[test]
    fn rejects_reserved_cell_names() {
        let text = VALID
            .replace("name = \"review\"", "name = \"done\"")
            .replace("\"review\", \"done\"", "\"done\"")
            .replace(
                "cells = [\"triage\", \"review\"]",
                "cells = [\"triage\", \"done\"]",
            );
        assert!(problems_of(&text).contains("予約されています"));
    }

    fn base_with_prompts(names: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("cells")).unwrap();
        for name in names {
            std::fs::write(dir.path().join("cells").join(name), "prompt").unwrap();
        }
        dir
    }

    fn problems_of(text: &str) -> String {
        let dir = base_with_prompts(&["triage.md", "review.md"]);
        Config::parse(text, dir.path()).unwrap_err().message
    }

    #[test]
    fn usage_pause_defaults_to_80_and_is_checked() {
        let dir = base_with_prompts(&["triage.md", "review.md"]);
        let config = Config::parse(VALID, dir.path()).unwrap();
        assert_eq!(config.usage.pause_at, 80);
        let config =
            Config::parse(&format!("{VALID}\n[usage]\npause_at = 95\n"), dir.path()).unwrap();
        assert_eq!(config.usage.pause_at, 95);
        for bad in ["0", "101"] {
            let message = problems_of(&format!("{VALID}\n[usage]\npause_at = {bad}\n"));
            assert!(message.contains("pause_at は1〜100"), "{bad}: {message}");
        }
    }

    #[test]
    fn parses_products_and_cells() {
        let dir = base_with_prompts(&["triage.md", "review.md"]);
        let config = Config::parse(VALID, dir.path()).unwrap();

        let product = &config.products[0];
        assert_eq!(product.issues.to_string(), "y-hirakaw/agentcells");
        assert_eq!(product.cells, vec!["triage", "review"]);

        let triage = config.cell("triage").unwrap();
        assert_eq!(triage.on, vec![Target::Issue]);
        assert_eq!(triage.max_rounds, DEFAULT_MAX_ROUNDS);
        assert_eq!(triage.timeout_minutes, DEFAULT_TIMEOUT_MINUTES);
        assert_eq!(triage.prompt, Some(dir.path().join("cells/triage.md")));

        let review = config.cell("review").unwrap();
        assert_eq!(review.on, vec![Target::PullRequest]);
        assert_eq!(review.max_rounds, 2);
    }

    #[test]
    fn template_is_a_valid_empty_config() {
        let config = Config::parse(TEMPLATE, Path::new("/nonexistent")).unwrap();
        assert!(config.products.is_empty());
        assert!(config.cells.is_empty());
    }

    #[test]
    fn reports_unknown_fields_with_hint() {
        let err = Config::parse("[[cell]]\nnmae = \"x\"\n", Path::new("/")).unwrap_err();
        assert!(err.message.contains("nmae"), "{}", err.message);
        assert!(err.hint.is_some());
    }

    #[test]
    fn operator_written_under_a_table_points_to_the_top_of_the_file() {
        let text = format!(
            "{}\n[github_app]\nid = \"1\"\nprivate_key = \"k.pem\"\noperator = \"me\"\n",
            VALID.replace("operator = \"y-hirakaw\"\n", "")
        );
        let err = Config::parse(&text, Path::new("/")).unwrap_err();
        assert!(err.hint.unwrap().contains("ファイルのいちばん上"));
        // ほかの見出しの下でも同じ
        let text = VALID.replace("operator = \"y-hirakaw\"\n", "") + "operator = \"me\"\n";
        let err = Config::parse(&text, Path::new("/")).unwrap_err();
        assert!(
            err.hint.unwrap().contains("ファイルのいちばん上"),
            "{}",
            err.message
        );
    }

    #[test]
    fn reports_invalid_repo_names() {
        let text = VALID.replace("\"y-hirakaw/agentcells\"]", "\"agentcells\"]");
        let dir = base_with_prompts(&["triage.md", "review.md"]);
        let err = Config::parse(&text, dir.path()).unwrap_err();
        assert!(
            err.message.contains("リポジトリ名の形式"),
            "{}",
            err.message
        );
    }

    #[test]
    fn reports_all_problems_at_once() {
        let text = VALID
            .replace(
                "cells = [\"triage\", \"review\"]",
                "cells = [\"triage\", \"deploy\"]",
            )
            .replace("label = \"ac:review\"", "label = \"review\"")
            .replace("max_rounds = 2", "max_rounds = 0");
        let message = problems_of(&text);
        assert!(message.contains("deploy"), "{message}");
        assert!(message.contains("ac: で始めて"), "{message}");
        assert!(message.contains("max_rounds"), "{message}");
    }

    #[test]
    fn rejects_duplicate_names_and_labels() {
        let text = VALID
            .replace("name = \"review\"", "name = \"triage\"")
            .replace("label = \"ac:review\"", "label = \"ac:triage\"");
        let message = problems_of(&text);
        assert!(message.contains("cellの名前「triage」が重複"), "{message}");
        assert!(message.contains("ラベル「ac:triage」を複数"), "{message}");
    }

    #[test]
    fn rejects_system_labels_for_cells() {
        let text = VALID.replace("label = \"ac:review\"", "label = \"ac:working\"");
        assert!(problems_of(&text).contains("agentcellsが使うラベル"));
    }

    #[test]
    fn builtin_cells_need_no_prompt_but_custom_cells_do() {
        let text = VALID
            .replace("prompt = \"cells/triage.md\"\n", "")
            .replace("prompt = \"cells/review.md\"\n", "");
        let dir = base_with_prompts(&[]);
        let config = Config::parse(&text, dir.path()).unwrap();
        assert_eq!(config.cell("triage").unwrap().prompt, None);

        let custom = text
            .replace("name = \"review\"", "name = \"design\"")
            .replace("\"review\", \"done\"", "\"design\", \"done\"")
            .replace(
                "cells = [\"triage\", \"review\"]",
                "cells = [\"triage\", \"design\"]",
            );
        let err = Config::parse(&custom, dir.path()).unwrap_err();
        assert!(
            err.message
                .contains("cell「design」には標準のプロンプトがない"),
            "{}",
            err.message
        );
    }

    #[test]
    fn rejects_missing_prompt_file() {
        let dir = base_with_prompts(&["triage.md"]);
        let err = Config::parse(VALID, dir.path()).unwrap_err();
        assert!(err.message.contains("review.md"), "{}", err.message);
        assert!(!err.message.contains("triage.md"), "{}", err.message);
    }

    #[test]
    fn rejects_empty_repos_and_bad_names() {
        let text = VALID
            .replace("repos = [\"y-hirakaw/agentcells\"]", "repos = []")
            .replace("name = \"agentcells\"", "name = \"Agent Cells\"");
        let message = problems_of(&text);
        assert!(message.contains("repos が空"), "{message}");
        assert!(message.contains("使えない文字"), "{message}");
    }

    #[test]
    fn repos_lists_issue_and_work_repos_once() {
        let text = VALID.replace(
            "repos = [\"y-hirakaw/agentcells\"]",
            "repos = [\"y-hirakaw/agentcells\", \"y-hirakaw/web\"]",
        );
        let dir = base_with_prompts(&["triage.md", "review.md"]);
        let config = Config::parse(&text, dir.path()).unwrap();
        let repos: Vec<String> = config.repos().iter().map(|r| r.to_string()).collect();
        assert_eq!(repos, vec!["y-hirakaw/agentcells", "y-hirakaw/web"]);
    }

    #[test]
    fn products_can_name_commands_for_the_ai() {
        let text = VALID.replace(
            "cells = [\"triage\", \"review\"]",
            "cells = [\"triage\", \"review\"]\n\n[product.commands]\ntest = \"npm test\"",
        );
        let dir = base_with_prompts(&["triage.md", "review.md"]);
        let config = Config::parse(&text, dir.path()).unwrap();
        assert_eq!(config.products[0].commands["test"], "npm test");

        let bad = text.replace("test = \"npm test\"", "Run_Tests = \"\"");
        let message = problems_of(&bad);
        assert!(message.contains("「Run_Tests」に使えない文字"), "{message}");
        assert!(message.contains("が空です"), "{message}");
    }

    #[test]
    fn cells_can_be_written_as_files() {
        let dir = base_with_prompts(&["triage.md", "review.md"]);
        let cells = dir.path().join("cells");
        std::fs::write(
            cells.join("design.md"),
            "+++\non = \"issue\"\nlabel = \"ac:design\"\nnext = [\"triage\"]\nconfirm = true\ncommands = [\"lint\"]\n+++\n\n# 役割: design\n設計を書く\n",
        )
        .unwrap();
        // 頭のないファイル（[[cell]] の prompt から指すもの）や、md でないものは、cellにしない
        std::fs::write(cells.join("notes.md"), "+ メモ\n").unwrap();
        std::fs::write(cells.join("._design.md"), [0xff, 0xfe]).unwrap();
        std::fs::write(cells.join("deploy.txt"), "+++\non = \"issue\"\n+++\n").unwrap();
        let text = VALID
            .replace(
                "cells = [\"triage\", \"review\"]",
                "cells = [\"triage\", \"review\", \"design\"]\n\n[product.commands]\nlint = \"make lint\"\ntest = \"make test\"",
            )
            .replace("next = [\"review\", \"done\"]", "next = [\"review\", \"design\", \"done\"]");
        let config = Config::parse(&text, dir.path()).unwrap();
        assert_eq!(config.cells.len(), 3);
        let design = config.cell("design").unwrap();
        assert!(design.from_file);
        assert!(!config.cell("triage").unwrap().from_file);
        assert_eq!(design.label, "ac:design");
        assert!(design.confirms());
        assert_eq!(design.max_rounds, DEFAULT_MAX_ROUNDS);
        assert_eq!(design.prompt, Some(cells.join("design.md")));
        // 指示は、頭の設定を除いた本文
        assert_eq!(
            design.extra_prompt().unwrap().unwrap(),
            "# 役割: design\n設計を書く\n"
        );
        assert_eq!(
            config.cell("triage").unwrap().extra_prompt().unwrap(),
            Some("prompt".to_string())
        );
        // 使えるコマンドは、選んだものだけ（書かなければすべて）
        let product = &config.products[0];
        assert_eq!(
            product.commands_for(design).keys().collect::<Vec<_>>(),
            ["lint"]
        );
        assert_eq!(
            product.commands_for(config.cell("triage").unwrap()).len(),
            2
        );

        // productのコマンドにない名前は断る
        std::fs::write(
            cells.join("design.md"),
            "+++\non = \"issue\"\nlabel = \"ac:design\"\ncommands = [\"deploy\"]\n+++\n設計を書く\n",
        )
        .unwrap();
        let message = Config::parse(&text, dir.path()).unwrap_err().message;
        assert!(message.contains("commands の「deploy」"), "{message}");

        // [[cell]] と同じ名前は、重なりとして断る
        std::fs::write(
            cells.join("triage.md"),
            "+++\non = \"issue\"\nlabel = \"ac:triage2\"\n+++\n",
        )
        .unwrap();
        let message = Config::parse(&text, dir.path()).unwrap_err().message;
        assert!(message.contains("「triage」が重複"), "{message}");
        std::fs::write(cells.join("triage.md"), "prompt").unwrap();

        // 頭の書き間違いは、ファイルの名前を付けて断る
        for (head, expected) in [
            (
                "+++\nname = \"x\"\non = \"issue\"\nlabel = \"ac:x\"\n+++\n",
                "name は書けません",
            ),
            (
                "+++\non = \"issue\"\nlabel = \"ac:x\"\nprompt = \"a.md\"\n+++\n",
                "prompt は書けません",
            ),
            (
                "+++\non = \"issue\"\nlabel = \"ac:x\"\ncolor = 1\n+++\n",
                "color",
            ),
            ("+++\non = \"issue\"\nlabel = \"ac:x\"\n", "閉じていません"),
            (
                "+++\non = \"issue\"\nlabel = \"ac:x\"\n+++\n\n",
                "役割と何をするか",
            ),
        ] {
            std::fs::write(cells.join("design.md"), head).unwrap();
            let message = Config::parse(&text, dir.path()).unwrap_err().message;
            assert!(message.contains("design.md"), "{message}");
            assert!(message.contains(expected), "{expected}: {message}");
        }
    }

    #[test]
    fn unused_cells_are_those_in_no_product() {
        let dir = base_with_prompts(&["triage.md", "review.md"]);
        std::fs::write(
            dir.path().join("cells/design.md"),
            "+++\non = \"issue\"\nlabel = \"ac:design\"\n+++\n設計を書く\n",
        )
        .unwrap();
        // design（ファイル）も review（[[cell]]）も、どのproductにも入っていない
        let text = VALID
            .replace("cells = [\"triage\", \"review\"]", "cells = [\"triage\"]")
            .replace("next = [\"review\", \"done\"]", "next = [\"done\"]");
        let config = Config::parse(&text, dir.path()).unwrap();
        let names: Vec<&str> = config
            .unused_cells()
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(names, ["review", "design"]);

        // 片方のproductでだけ使っていれば、使っている
        let text = format!(
            "{text}\n[[product]]\nname = \"web\"\nissues = \"y-hirakaw/web\"\nrepos = [\"y-hirakaw/web\"]\ncells = [\"review\", \"design\"]\n"
        );
        let config = Config::parse(&text, dir.path()).unwrap();
        assert!(config.unused_cells().is_empty());
    }

    #[test]
    fn example_cells_can_be_read() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/cells");
        let cells = cell_files(&dir).unwrap();
        let names: Vec<&str> = cells.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["design", "release-prep"]);
        for cell in &cells {
            let body = cell.extra_prompt().unwrap().unwrap();
            assert!(
                body.starts_with(&format!("## この cell の役割: {}", cell.name)),
                "{body}"
            );
        }
    }

    #[test]
    fn front_matter_is_split_from_the_body() {
        assert_eq!(front_matter("本文"), Ok(None));
        assert_eq!(
            front_matter("+++\na = 1\n+++\n本文"),
            Ok(Some(("a = 1\n", "本文")))
        );
        assert_eq!(
            front_matter("+++\r\na = 1\r\n+++\r\n\r\n本文"),
            Ok(Some(("a = 1\r\n", "本文")))
        );
        assert_eq!(front_matter("+++\n+++"), Ok(Some(("", ""))));
        assert_eq!(
            front_matter("\u{feff}+++ \r\na = 1\n+++  \n本文"),
            Ok(Some(("a = 1\n", "本文")))
        );
        assert!(front_matter("+++\na = 1\n++++\n").is_err());
    }

    #[test]
    fn products_can_protect_files() {
        let text = VALID.replace(
            "cells = [\"triage\", \"review\"]",
            "cells = [\"triage\", \"review\"]\nprotected = [\"src/hook.rs\", \"prompts/\", \".github/\"]",
        );
        let dir = base_with_prompts(&["triage.md", "review.md"]);
        let config = Config::parse(&text, dir.path()).unwrap();
        let product = &config.products[0];
        for path in [
            "src/hook.rs",
            "prompts/common.md",
            ".github/workflows/ci.yml",
        ] {
            assert!(product.protects(path), "{path}");
        }
        for path in ["SRC/Hook.rs", ".GitHub/x", "prompts"] {
            assert!(product.protects(path), "{path}");
        }
        for path in [
            "src/hook.rs.bak",
            "src/main.rs",
            "prompts.md",
            "src/prompts/x",
        ] {
            assert!(!product.protects(path), "{path}");
        }
        // 書かなければ、何も守らない
        let config = Config::parse(VALID, dir.path()).unwrap();
        assert!(!config.products[0].protects("src/hook.rs"));

        let bad = text.replace(
            "\"src/hook.rs\", \"prompts/\"",
            "\"/src/hook.rs\", \"./prompts/\", \"a/../b\", \"src\\\\hook.rs\", \"/\", \"a//b\", \".github/*\"",
        );
        let message = problems_of(&bad);
        for entry in [
            "/src/hook.rs",
            "./prompts/",
            "a/../b",
            "src\\hook.rs",
            "「/」",
            "a//b",
            ".github/*",
        ] {
            assert!(message.contains(entry), "{entry}: {message}");
        }
    }

    #[test]
    fn products_can_open_the_sandbox() {
        let text = VALID.replace(
            "cells = [\"triage\", \"review\"]",
            "cells = [\"triage\", \"review\"]\n\n[product.sandbox]\nread = [\"~/.cargo\", \"/opt/sdk\"]\nwrite = [\"~/.gradle\"]\ncaches = [\"gradle\", \"npm\"]\nread_placed = [\"app/google-services.json\"]\nnetwork = [\"registry.npmjs.org\", \"*.gradle.org\"]",
        );
        let dir = base_with_prompts(&["triage.md", "review.md"]);
        let config = Config::parse(&text, dir.path()).unwrap();
        let sandbox = &config.products[0].sandbox;
        let user_home = PathBuf::from(std::env::var_os("HOME").unwrap());
        assert_eq!(
            sandbox.read,
            vec![user_home.join(".cargo"), PathBuf::from("/opt/sdk")]
        );
        assert_eq!(sandbox.write, vec![user_home.join(".gradle")]);
        assert_eq!(sandbox.caches, vec!["gradle", "npm"]);
        assert_eq!(
            sandbox.read_placed,
            vec![PathBuf::from("app/google-services.json")]
        );
        assert_eq!(sandbox.network, vec!["registry.npmjs.org", "*.gradle.org"]);

        let bad = text
            .replace("\"/opt/sdk\"", "\"opt/sdk\", \"~/../etc\"")
            .replace("app/google-services.json", "../key.jks")
            .replace("\"*.gradle.org\"", "\"https://gradle.org\", \"localhost\"")
            .replace("\"npm\"]", "\"npm\", \"bazel\"]");
        let message = problems_of(&bad);
        assert!(message.contains("sandbox.read の opt/sdk"), "{message}");
        assert!(message.contains("sandbox.read の /"), "{message}");
        assert!(message.contains("「../key.jks」"), "{message}");
        assert!(message.contains("「https://gradle.org」"), "{message}");
        assert!(message.contains("「localhost」"), "{message}");
        assert!(
            message.contains("「bazel」は知らないキャッシュ"),
            "{message}"
        );

        // 書かなければ、何も開けない
        let config = Config::parse(VALID, dir.path()).unwrap();
        assert!(config.products[0].sandbox.read.is_empty());
        assert!(config.products[0].sandbox.network.is_empty());
    }

    #[test]
    fn operator_is_required_with_products_and_must_be_a_login() {
        let dir = base_with_prompts(&["triage.md", "review.md"]);
        let config = Config::parse(VALID, dir.path()).unwrap();
        assert_eq!(config.operator.as_deref(), Some("y-hirakaw"));

        let message = problems_of(&VALID.replace("operator = \"y-hirakaw\"\n", ""));
        assert!(message.contains("operator に"), "{message}");
        let message =
            problems_of(&VALID.replace("y-hirakaw\"\n\n[[product]]", "bad login\"\n\n[[product]]"));
        assert!(message.contains("「bad login」"), "{message}");
    }

    #[test]
    fn github_app_is_optional() {
        let dir = base_with_prompts(&["triage.md", "review.md"]);
        assert!(
            Config::parse(VALID, dir.path())
                .unwrap()
                .github_app
                .is_none()
        );
    }

    #[test]
    fn github_app_id_accepts_numbers_and_resolves_the_key_path() {
        let dir = base_with_prompts(&["triage.md", "review.md"]);
        std::fs::write(dir.path().join("app.pem"), "key").unwrap();
        for (id, expected) in [("123456", "123456"), ("\"Iv23liABC\"", "Iv23liABC")] {
            let text = format!("{VALID}\n[github_app]\nid = {id}\nprivate_key = \"app.pem\"\n");
            let app = Config::parse(&text, dir.path())
                .unwrap()
                .github_app
                .unwrap();
            assert_eq!(app.id, expected);
            assert_eq!(app.private_key, dir.path().join("app.pem"));
        }
    }

    #[test]
    fn github_app_needs_an_id_and_a_key_file() {
        let text = format!("{VALID}\n[github_app]\nid = \"\"\nprivate_key = \"missing.pem\"\n");
        let message = problems_of(&text);
        assert!(message.contains("id が空"), "{message}");
        assert!(message.contains("missing.pem"), "{message}");
    }

    #[test]
    fn load_reports_missing_file_with_setup_hint() {
        let dir = tempfile::tempdir().unwrap();
        let err = Config::load(&dir.path().join("config.toml")).unwrap_err();
        assert!(err.hint.unwrap().contains("agentcells setup"));
    }
}
