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
# [product.sandbox]                  # AIが動かすコマンドに開ける場所。標準では、ホームの下は今の仕事の場所しか読めず、外へは通信できない
# read = ["~/.nvm"]                  # ホームの下で読ませる場所（ツールチェーンなど）。開けた場所の秘密は読まれうる
# write = ["~/.npm"]                 # ホームの下で書かせる場所（キャッシュなど）。書かれたものは、手元で動かすツールにも効く
# network = ["registry.npmjs.org"]   # 通信してよい先。取得元のホストを具体的に書く（*.amazonaws.com のような広いワイルドカードは、外へ送る道になる）
# read_placed = []                   # files/ から置いたファイルのうち、ビルドに読ませるもの（リポジトリからの相対パス）
# bind_localhost = false             # テストが localhost でサーバーを開けるようにする
#
# cell: どのラベルが付いたIssue/PRを拾い、どのプロンプトで動くか
#
# [[cell]]
# name = "triage"
# on = "issue"                       # issue か pull_request（両方なら ["issue", "pull_request"]）
# label = "ac:triage"                # 拾うラベル（ac: で始める）
# prompt = "cells/triage.md"         # 追加の指示のファイル（このファイルからの相対パス）。標準のcell（triage、implement、review）なら省略できる
# next = ["implement", "done"]       # 進めてよい先（cellの名前か done）。needs-human はいつでも選べる
# review = false                     # true にすると、PRへのレビューとして指摘を行ごとに投稿する（on は pull_request だけ）
# max_rounds = 3                     # 往復の上限（1〜3。省略すると3）
# timeout_minutes = 30               # 1回の実行の上限（省略すると30）
#
# github_app: GitHubへの書き込み（コメント、ラベル、push、PR）に使うGitHub App（推奨）
# 書かなければ、ghにログインしているあなたのアカウントで書き込みます。
# Appで書き込むと、コメントやPRが「<Appの名前>[bot]」になり、人間の発言と見分けられます。
#
# [github_app]
# id = "Iv23li..."                   # Client ID（App IDでもよい）
# private_key = "github-app.pem"     # 秘密鍵（このファイルからの相対パス。権限は600にする）
"#;

/// 往復の上限。これより多く往復させず、人間に回す。
pub const MAX_ROUNDS_LIMIT: u32 = 3;
const DEFAULT_MAX_ROUNDS: u32 = MAX_ROUNDS_LIMIT;
const DEFAULT_TIMEOUT_MINUTES: u32 = 30;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default, rename = "product")]
    pub products: Vec<Product>,
    #[serde(default, rename = "cell")]
    pub cells: Vec<Cell>,
    /// 書き込みに使うGitHub App。なければghにログインしているアカウントで書き込む
    #[serde(default)]
    pub github_app: Option<GithubApp>,
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
    /// ホームの下で書かせる（読ませもする）場所（`~/.gradle` のようなキャッシュ）。書き方は `read` と同じ。
    /// AIが書き換えたビルドがここに書いたものは、使う人が手元で動かすツールにも効く
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
    /// 標準のcell（triage、implement、review）なら省略できる
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
            Error::new(e.to_string().trim_end().to_string())
                .with_hint("TOMLの書き方と項目名を確かめてください")
        })?;
        for cell in &mut config.cells {
            if let Some(prompt) = &mut cell.prompt {
                *prompt = base.join(&*prompt);
            }
        }
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

    /// 見つかった問題をすべて返す。1つずつ直しては実行し直す手間を減らすため、最初の1つで止めない。
    fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();

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
            for domain in &p.sandbox.network {
                if !is_valid_domain(domain) {
                    problems.push(format!(
                        "product「{}」の sandbox.network の「{domain}」はドメインではありません（例: registry.npmjs.org、*.gradle.org）",
                        p.name
                    ));
                }
            }
            for name in &p.cells {
                let Some(cell) = self.cell(name) else {
                    problems.push(format!(
                        "product「{}」のcell「{name}」が [[cell]] に定義されていません",
                        p.name
                    ));
                    continue;
                };
                // 遷移先のcellも、同じproductで動いていないと拾われない。
                for next in &cell.next {
                    if self.cell(next).is_some() && !p.cells.contains(next) {
                        problems.push(format!(
                            "product「{}」のcell「{name}」は「{next}」に進めますが、「{next}」がこのproductの cells にありません",
                            p.name
                        ));
                    }
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
            for next in &c.next {
                if next == &c.name {
                    problems.push(format!("cell「{}」の next に自分自身があります", c.name));
                } else if !transition::is_reserved(next) && self.cell(next).is_none() {
                    problems.push(format!(
                        "cell「{}」の next の「{next}」が [[cell]] に定義されていません（cellの名前か {} を書きます）",
                        c.name,
                        transition::DONE
                    ));
                }
            }
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
            if c.timeout_minutes == 0 {
                problems.push(format!(
                    "cell「{}」の timeout_minutes は1以上にしてください",
                    c.name
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
        assert!(message.contains("「deploy」が [[cell]]"), "{message}");
        assert!(!message.contains("「done」"), "{message}");
        assert!(!message.contains("「needs-human」"), "{message}");
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
    fn products_can_open_the_sandbox() {
        let text = VALID.replace(
            "cells = [\"triage\", \"review\"]",
            "cells = [\"triage\", \"review\"]\n\n[product.sandbox]\nread = [\"~/.cargo\", \"/opt/sdk\"]\nwrite = [\"~/.gradle\"]\nread_placed = [\"app/google-services.json\"]\nnetwork = [\"registry.npmjs.org\", \"*.gradle.org\"]",
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
        assert_eq!(
            sandbox.read_placed,
            vec![PathBuf::from("app/google-services.json")]
        );
        assert_eq!(sandbox.network, vec!["registry.npmjs.org", "*.gradle.org"]);

        let bad = text
            .replace("\"/opt/sdk\"", "\"opt/sdk\", \"~/../etc\"")
            .replace("app/google-services.json", "../key.jks")
            .replace("\"*.gradle.org\"", "\"https://gradle.org\", \"localhost\"");
        let message = problems_of(&bad);
        assert!(message.contains("sandbox.read の opt/sdk"), "{message}");
        assert!(message.contains("sandbox.read の /"), "{message}");
        assert!(message.contains("「../key.jks」"), "{message}");
        assert!(message.contains("「https://gradle.org」"), "{message}");
        assert!(message.contains("「localhost」"), "{message}");

        // 書かなければ、何も開けない
        let config = Config::parse(VALID, dir.path()).unwrap();
        assert!(config.products[0].sandbox.read.is_empty());
        assert!(config.products[0].sandbox.network.is_empty());
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
