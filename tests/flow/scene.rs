//! 場面の書き方。偽のGitHubと偽のclaudeを用意し、人の操作を書き、tickを回して、結果を確かめる。
//!
//! ```ignore
//! let s = Scene::new(CONFIG);
//! let i = s.issue(OP, "タイトル", "本文");
//! s.as_user(OP).assign(i, OP).label(i, "ac:triage");
//! s.claude("triage").returns(json!({ "next": "assess", "comment": "..." }));
//! s.tick();
//! s.expect(i).labels(&["ac:assess"]);
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;

use agentcells::home::Home;
use agentcells::programs::Programs;
use agentcells::tick::{self, Ended};
use serde_json::Value;

use crate::flow::github::{self, Comment, FakeGithub, Item};

/// cellを使う人
pub const OP: &str = "op";
/// 書き込みのアカウント（ghにログインしているアカウント）。[`Setup`] で、operatorと同じにもできる
pub const WRITER: &str = "agent";
/// ほかの人（書き込みの権限を持つ）
pub const STRANGER: &str = "stranger";
/// Issueと作業のリポジトリ
pub const REPO: &str = "octo/app";
/// リポジトリにあるラベル
pub const LABELS: &[&str] = &[
    "ac:triage",
    "ac:assess",
    "ac:implement",
    "ac:review",
    "ac:working",
    "ac:needs-human",
    "ac:in-review",
];

/// 場面の前提。
pub struct Setup<'a> {
    /// 書き込みのアカウント（ghにログインしているアカウント）
    pub writer: &'a str,
    /// リポジトリにあるラベル（この書き方で返る）
    pub labels: &'a [&'a str],
}

impl Default for Setup<'_> {
    fn default() -> Self {
        Setup {
            writer: WRITER,
            labels: LABELS,
        }
    }
}

pub struct Scene {
    /// 失敗したら、調べられるよう残す
    root: Option<tempfile::TempDir>,
    home: Home,
    pub gh: FakeGithub,
    programs: Programs,
}

impl Scene {
    /// `config` は config.toml の中身。リポジトリは [`REPO`] だけ。
    pub fn new(config: &str) -> Self {
        Self::with(config, Setup::default())
    }

    pub fn with(config: &str, setup: Setup) -> Self {
        let root = tempfile::tempdir().expect("一時ディレクトリを作れません");
        let home = Home::new(root.path().join("home"));
        std::fs::create_dir_all(home.root()).unwrap();
        std::fs::write(home.config_path(), config).unwrap();

        let bare = root.path().join("remotes").join(format!("{REPO}.git"));
        init_remote(&bare, &root.path().join("seed"));

        // 始まりは今の時刻（CLIは、手元の時計とGitHubの時刻を比べるところがあるため）
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let gh = FakeGithub::new(now);
        {
            let mut state = gh.state.borrow_mut();
            for login in [OP, WRITER, STRANGER] {
                state
                    .tokens
                    .insert(format!("tok-{login}"), login.to_string());
            }
            state.gh_token = format!("tok-{}", setup.writer);
            state.repos.insert(
                REPO.to_string(),
                github::Repo {
                    // octo はorganizationで、op はそのメンバー。書き込みのアカウントとほかの人は、招かれた人
                    owner: "octo".to_string(),
                    members: vec![OP.to_string()],
                    collaborators: vec![WRITER.to_string(), STRANGER.to_string()],
                    labels: setup.labels.iter().map(|l| l.to_string()).collect(),
                    bare,
                    ..github::Repo::default()
                },
            );
        }

        let bin = root.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let claude_dir = root.path().join("claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let me = std::env::current_exe().unwrap();
        let claude = script(
            &bin.join("claude"),
            &format!(
                "exec '{}' --fake-claude '{}' \"$@\"\n",
                me.display(),
                claude_dir.display()
            ),
        );
        // 何も見つけない betterleaks
        let scanner = script(&bin.join("betterleaks"), "cat > /dev/null\nexit 0\n");
        let programs = Programs::Fixed {
            agentcells: PathBuf::from(env!("CARGO_BIN_EXE_agentcells")),
            claude,
            scanner,
        };
        Scene {
            root: Some(root),
            home,
            gh,
            programs,
        }
    }

    fn root(&self) -> &Path {
        self.root.as_ref().expect("場面のフォルダ").path()
    }

    fn claude_dir(&self) -> PathBuf {
        self.root().join("claude")
    }

    /// Issueを作る（`author` が書く）。
    pub fn issue(&self, author: &str, title: &str, body: &str) -> u64 {
        self.gh
            .state
            .borrow_mut()
            .open_issue(REPO, author, title, body)
    }

    /// 人の操作。ghを通さず、偽のGitHubに直接書く。
    pub fn as_user<'a>(&'a self, login: &'a str) -> User<'a> {
        User { scene: self, login }
    }

    /// cellの次の回に返す結果を用意する。
    pub fn claude(&self, cell: &str) -> Reply {
        Reply {
            dir: self.claude_dir().join("replies").join(cell),
        }
    }

    /// 時計を止める（`true`）か、また進める。止めている間の書き込みは、すべて同じ秒になる。
    /// 止めるときは1秒進めてから止める（止める前の書き込みとは、別の秒にする）。
    pub fn freeze_clock(&self, frozen: bool) {
        let mut state = self.gh.state.borrow_mut();
        if frozen && !state.frozen {
            state.now();
        }
        state.frozen = frozen;
    }

    /// tickを1回動かす。次のどれかがあれば失敗にする。
    /// - 偽物が解けなかった呼び出しか、偽物が失敗を返した呼び出し
    /// - 場面の誤り（返す結果がない、スキーマに合わない、用意した結果が使われずに残った）
    pub fn tick(&self) -> Ended {
        let options = tick::Options {
            dry_run: false,
            max_minutes: 10,
            ignore_pause: false,
            model: None,
        };
        let ended = tick::drive(&self.home, &self.gh, &self.programs, &options)
            .unwrap_or_else(|e| panic!("tickが失敗しました: {}", e.message));
        let state = self.gh.state.borrow();
        assert!(
            state.unknown.is_empty(),
            "偽物が知らない呼び出し: {:#?}",
            state.unknown
        );
        assert!(
            state.failed.is_empty(),
            "偽物が失敗を返した呼び出し: {:#?}",
            state.failed
        );
        drop(state);
        let mut problems: Vec<String> = std::fs::read_dir(self.claude_dir().join("problems"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| std::fs::read_to_string(e.path()).ok())
            .collect();
        for cell in std::fs::read_dir(self.claude_dir().join("replies"))
            .into_iter()
            .flatten()
            .flatten()
        {
            let cell = cell.file_name().to_string_lossy().to_string();
            let prepared = std::fs::read_dir(self.claude_dir().join("replies").join(&cell))
                .map(|d| d.count() as u32)
                .unwrap_or(0);
            if prepared > self.runs_of(&cell) {
                problems.push(format!(
                    "{cell} に用意した結果が使われていません（{prepared}回分のうち{}回）",
                    self.runs_of(&cell)
                ));
            }
        }
        assert!(problems.is_empty(), "場面の誤り: {problems:#?}");
        ended
    }

    /// このtickまでに見送ったものの理由（記録の `Skipped`）。
    pub fn skipped(&self) -> Vec<String> {
        agentcells::record::read(&self.home)
            .into_iter()
            .filter_map(|line| match line.event {
                agentcells::record::Event::Skipped { reason, .. } => Some(reason),
                _ => None,
            })
            .collect()
    }

    /// cellの n 回目（1から）に渡ったプロンプト（system と user）。
    pub fn prompt_of(&self, cell: &str, n: u32) -> Prompt {
        let dir = self.claude_dir().join("calls").join(format!("{cell}-{n}"));
        let read = |name: &str| {
            std::fs::read_to_string(dir.join(name))
                .unwrap_or_else(|_| panic!("{cell} の {n} 回目は動いていません"))
        };
        Prompt {
            system: read("system.txt"),
            user: read("prompt.txt"),
            schema: serde_json::from_str(&read("schema.json")).unwrap(),
        }
    }

    /// cellが動いた回数。
    pub fn runs_of(&self, cell: &str) -> u32 {
        let calls = self.claude_dir().join("calls");
        (1..)
            .take_while(|n| calls.join(format!("{cell}-{n}")).exists())
            .count() as u32
    }

    pub fn expect(&self, number: u64) -> Expect {
        let item = self.gh.state.borrow_mut().item(REPO, number).clone();
        Expect { item }
    }
}

impl Drop for Scene {
    /// 失敗したら、場面のフォルダ（プロンプト、作業データ）を残し、その中に偽のGitHubの状態を書く。
    fn drop(&mut self) {
        if std::thread::panicking()
            && let Some(root) = self.root.take()
        {
            let root = root.keep();
            let path = root.join("github.json");
            github::dump(&self.gh.state.borrow(), &path);
            eprintln!(
                "  場面のフォルダ: {}（偽のGitHubの状態は github.json）",
                root.display()
            );
        }
    }
}

pub struct User<'a> {
    scene: &'a Scene,
    login: &'a str,
}

impl User<'_> {
    pub fn label(&self, number: u64, label: &str) -> &Self {
        let mut state = self.scene.gh.state.borrow_mut();
        let at = state.now();
        state
            .add_labels(REPO, number, self.login, &[label], &at)
            .unwrap_or_else(|e| panic!("{e}"));
        self
    }

    pub fn unlabel(&self, number: u64, label: &str) -> &Self {
        let mut state = self.scene.gh.state.borrow_mut();
        let at = state.now();
        state
            .remove_labels(REPO, number, self.login, &[label], &at)
            .unwrap_or_else(|e| panic!("{e}"));
        self
    }

    pub fn assign(&self, number: u64, assignee: &str) -> &Self {
        self.scene
            .gh
            .state
            .borrow_mut()
            .assign(REPO, number, self.login, assignee);
        self
    }

    pub fn comment(&self, number: u64, body: &str) -> &Self {
        self.scene
            .gh
            .state
            .borrow_mut()
            .comment(REPO, number, self.login, body);
        self
    }
}

pub struct Reply {
    dir: PathBuf,
}

impl Reply {
    /// 次の回に返す結果（`structured_output`）。
    pub fn returns(&self, output: Value) {
        std::fs::create_dir_all(&self.dir).unwrap();
        let n = (1..)
            .find(|n| !self.dir.join(format!("{n}.json")).exists())
            .unwrap();
        std::fs::write(self.dir.join(format!("{n}.json")), output.to_string()).unwrap();
    }
}

pub struct Prompt {
    pub system: String,
    pub user: String,
    pub schema: Value,
}

impl Prompt {
    /// 選べる進め先（スキーマの `next` の enum）。
    pub fn choices(&self) -> Vec<String> {
        self.schema["properties"]["next"]["enum"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect()
    }
}

pub struct Expect {
    pub item: Item,
}

impl Expect {
    /// 付いているラベルが、これだけ（順は問わない。書き方は、大文字・小文字も同じ）。
    pub fn labels(&self, expected: &[&str]) -> &Self {
        let mut got: Vec<&str> = self.item.labels.iter().map(String::as_str).collect();
        got.sort();
        let mut want: Vec<&str> = expected.to_vec();
        want.sort();
        assert_eq!(got, want, "#{} のラベル", self.item.number);
        self
    }

    pub fn open(&self, open: bool) -> &Self {
        assert_eq!(self.item.open, open, "#{} が開いているか", self.item.number);
        self
    }

    /// `author` のコメントの数。
    pub fn comments_by(&self, author: &str, count: usize) -> &Self {
        let got = self
            .item
            .comments
            .iter()
            .filter(|c| c.author == author)
            .count();
        assert_eq!(
            got, count,
            "#{} の {author} のコメントの数",
            self.item.number
        );
        self
    }

    /// 最後のコメント。
    pub fn last_comment(&self) -> &Comment {
        self.item
            .comments
            .last()
            .unwrap_or_else(|| panic!("#{} にコメントがありません", self.item.number))
    }
}

/// 実行できるshのスクリプトを書く。
fn script(path: &Path, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_path_buf()
}

/// GitHubの代わりのbareリポジトリを作り、最初のコミットを置く。使う人のgitの設定は読まない。
fn init_remote(bare: &Path, seed: &Path) {
    let git = |dir: &Path, args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("gitを動かせません");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    std::fs::create_dir_all(bare).unwrap();
    git(bare, &["init", "-q", "--bare", "-b", "main"]);
    std::fs::create_dir_all(seed).unwrap();
    git(seed, &["init", "-q", "-b", "main"]);
    std::fs::write(seed.join("README.md"), "# app\n").unwrap();
    git(seed, &["add", "README.md"]);
    git(
        seed,
        &[
            "-c",
            "user.name=octo",
            "-c",
            "user.email=octo@example.com",
            "commit",
            "-q",
            "-m",
            "first",
        ],
    );
    let bare = bare.display().to_string();
    git(seed, &["push", "-q", &bare, "main"]);
}
