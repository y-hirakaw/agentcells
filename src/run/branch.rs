//! 仕事のブランチ: 用意する（checkout）、ベースとのぶつかりを調べる、確かめてpushする、pushしない作業を残す。

use std::path::Path;

use super::place::Place;
use super::{Ready, Runner, git, git_out, path_str};
use crate::actions;
use crate::config::{Cell, Target};
use crate::error::{Error, Result};
use crate::github::{Credentials, ISOLATED_GIT_ENV};
use crate::probe::Probe;
use crate::prompt;
use crate::tick::Work;
use crate::transition::{self, Outcome};

/// 仕事のブランチ。
pub(super) struct Branch {
    pub(super) name: String,
    /// PRのベース（Issueなら、既定のブランチ）
    pub(super) base: String,
    /// 動かす前のブランチの先（ここから先が、AIが足したコミット）
    pub(super) start: String,
}

/// pushの結果。
pub(super) enum Pushed {
    /// 新しいコミットがない（か、pushしないcell）
    Nothing,
    /// 確かめで止めた。人間に回す理由
    Refused(String),
    /// pushした。IssueからならPRの番号
    Done(Option<u64>),
    /// 人間に回すので、PRは作らずに、ブランチだけをpushした（次の実行は、そこから続ける）。
    /// そのブランチのPRが開いていれば、その番号
    Branch(String, Option<u64>),
}

/// ブランチが、ベースのブランチとぶつかるかを、手元で調べた結果。
#[derive(Debug, PartialEq, Eq)]
pub(super) enum BaseCheck {
    /// ぶつからない（`base` は、調べたときのベースのブランチの先）
    Clean {
        base: String,
    },
    Conflict(prompt::Conflict),
    /// 調べられなかった（gitが古い、ベースのブランチを読めないなど）
    Unknown,
}

/// ブランチが、ベースのブランチ（`origin/<base>`）とぶつかるかを、`git merge-tree` で調べる。
pub(super) fn check_base(probe: &dyn Probe, worktree: &Path, branch: &Branch) -> BaseCheck {
    let Ok(wt) = path_str(worktree) else {
        return BaseCheck::Unknown;
    };
    let base = format!("origin/{}", branch.base);
    let Ok(tip) = git_out(probe, &["-C", &wt, "rev-parse", "--verify", &base]) else {
        return BaseCheck::Unknown;
    };
    let Some(out) = probe.run_with_env(
        "git",
        &[
            "-C",
            &wt,
            "merge-tree",
            "--write-tree",
            "-z",
            "--name-only",
            "--no-messages",
            &base,
            "HEAD",
        ],
        ISOLATED_GIT_ENV,
    ) else {
        return BaseCheck::Unknown;
    };
    // ぶつかりがなければ0、あれば1で終わる。木のあとに、ぶつかったファイルが並ぶ（NULで区切る。
    // 区切らないと、日本語などの名前が引用符と8進数になる）
    match out.code {
        Some(0) => BaseCheck::Clean {
            base: tip.trim().to_string(),
        },
        Some(1) => {
            let files: Vec<String> = out
                .stdout
                .split('\0')
                .skip(1)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect();
            BaseCheck::Conflict(prompt::Conflict {
                base: branch.base.clone(),
                files,
            })
        }
        _ => BaseCheck::Unknown,
    }
}

/// 残したAIの作業のうち、古いもの（`KEPT_DAYS` より前に作ったもの）を消す。
fn prune_kept(kept: &Path, now: u64) {
    let limit = KEPT_DAYS * 24 * 3600;
    let Ok(owners) = std::fs::read_dir(kept) else {
        return;
    };
    // kept/<owner>/<repo>/<番号>/<時刻>.bundle
    let dirs = |dir: std::path::PathBuf| -> Vec<std::path::PathBuf> {
        std::fs::read_dir(dir)
            .map(|e| e.flatten().map(|e| e.path()).collect())
            .unwrap_or_default()
    };
    for owner in owners.flatten().map(|e| e.path()) {
        for repo in dirs(owner.clone()) {
            for number in dirs(repo.clone()) {
                for bundle in dirs(number.clone()) {
                    let at = bundle
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .and_then(|s| s.parse::<u64>().ok());
                    if at.is_some_and(|at| at + limit < now) {
                        let _ = std::fs::remove_file(bundle);
                    }
                }
                // 空になった場所も消す（空でなければ消えない）
                let _ = std::fs::remove_dir(&number);
            }
            let _ = std::fs::remove_dir(&repo);
        }
        let _ = std::fs::remove_dir(&owner);
    }
}

/// 残したAIの作業を消すまでの日数。
const KEPT_DAYS: u64 = 30;

/// AIが足したコミット（動かす前のブランチの先から）を、gitのbundleとして `path`（`kept` の下）に残す。
/// 秘密らしいもので断った作業も入るので、`kept` から下はほかのユーザーに読ませない。
fn keep_bundle(
    probe: &dyn Probe,
    staging: &Path,
    branch: &Branch,
    kept: &Path,
    path: &Path,
) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let private = |dir: &Path| {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .and_then(|()| std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)))
            .map_err(|e| Error::new(format!("{} を作れません: {e}", dir.display())))
    };
    private(kept)?;
    if let Some(dir) = path.parent() {
        private(dir)?;
    }
    let staging = path_str(staging)?;
    let out = path_str(path)?;
    let head = format!("refs/heads/{}", branch.name);
    let start = format!("^{}", branch.start);
    git(
        probe,
        &[
            "-C", &staging, "bundle", "create", "-q", &out, &head, &start,
        ],
        ISOLATED_GIT_ENV,
    )?;
    // AIが読んだファイルの中身も入りうるので、ほかのユーザーに読ませない
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| Error::new(format!("{} の権限を変えられません: {e}", path.display())))
}

impl Runner<'_> {
    /// 作業の場所で、仕事のブランチを用意する（claudeを動かす前の、CLIが作ったばかりのcloneで行う）。
    /// - Issue: `ac/<owner>-<repo>-<番号>`。前の実行でpushしていれば、その続きから始める
    /// - PR: PRのブランチ。フォークからのPRは動かさない
    /// - レビューのcellも、PRのブランチを見る（pushはしない）
    pub(super) fn checkout(
        &self,
        work: &Work,
        task: &prompt::Task,
        place: &Place,
    ) -> Result<Option<Branch>> {
        let wt = path_str(&place.worktree)?;
        let (name, base) = match work.item {
            Target::Issue => {
                let base = git_out(self.probe, &["-C", &wt, "symbolic-ref", "--short", "HEAD"])?;
                (
                    crate::pr::branch_name(&work.repo, work.number),
                    base.trim().to_string(),
                )
            }
            Target::PullRequest => {
                let d = &task.discussion;
                let (Some(head), Some(base)) = (&d.head, &d.base) else {
                    return Err(Error::new("PRのブランチを読めません"));
                };
                if d.head_repo.as_deref() != Some(&work.repo.to_string()) {
                    return Err(Error::new(format!(
                        "フォーク（{}）からのPRは、動かせません",
                        d.head_repo.as_deref().unwrap_or("不明")
                    )));
                }
                (head.clone(), base.clone())
            }
        };
        let remote = format!("refs/remotes/origin/{name}");
        let exists = git_out(
            self.probe,
            &["-C", &wt, "rev-parse", "-q", "--verify", &remote],
        )
        .is_ok();
        if work.item == Target::PullRequest && !exists {
            return Err(Error::new(format!("PRのブランチ {name} が見つかりません")));
        }
        let mut args = vec![
            "-C",
            wt.as_str(),
            "-c",
            "core.hooksPath=/dev/null",
            "checkout",
            "-q",
            "-B",
            name.as_str(),
        ];
        let origin = format!("origin/{name}");
        if exists {
            args.push(origin.as_str());
        }
        git_out(self.probe, &args)?;
        let start = git_out(self.probe, &["-C", &wt, "rev-parse", "HEAD"])?
            .trim()
            .to_string();
        Ok(Some(Branch { name, base, start }))
    }

    /// pushしなかったAIの作業（コミット）を、手元に残し、取り出し方を返す。実行の記録の場所は、同じIssue/PRの
    /// 次の実行で空になるので、別の場所（`kept/`）に、時刻を付けて残す。
    fn keep_work(&self, place: &Place, staging: &Path, branch: &Branch) -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        let kept = self.home.root().join("kept");
        prune_kept(&kept, now);
        let name = format!("{}/{now}.bundle", place.relative.display());
        match keep_bundle(self.probe, staging, branch, &kept, &kept.join(&name)) {
            Ok(()) => format!(
                "AIの作業（コミット）は、agentcellsを動かしている人の手元の ~/.agentcells/kept/{name} に残しました（{KEPT_DAYS}日で消します）。作業するリポジトリで `git fetch ~/.agentcells/kept/{name} {}:ai-work` を実行すると、ブランチ ai-work として取り出せます",
                branch.name
            ),
            Err(e) => format!(
                "AIの作業（コミット）を手元に残せませんでした: {}",
                e.message
            ),
        }
    }

    /// pushしないで終わるとき、AIが足したコミットがあれば手元に残し、取り出し方を返す。なければ `None`。
    pub(super) fn keep_unpushed(&self, place: &Place, branch: &Branch) -> Option<String> {
        let staging = crate::pr::stage(self.probe, &place.worktree, &branch.name).ok()?;
        let staging_path = path_str(staging.path()).ok()?;
        let range = format!("{}..refs/heads/{}", branch.start, branch.name);
        let count = git_out(
            self.probe,
            &["-C", &staging_path, "rev-list", "--count", &range],
        )
        .ok()?;
        (count.trim() != "0").then(|| self.keep_work(place, staging.path(), branch))
    }

    /// AIが足したコミットを確かめてpushする。IssueからでPRに進むなら、PRを作る（あれば、それを使う）。
    /// 確かめもpushも、CLIがその場で作ったbareリポジトリから行う（AIが書き換えられる .git/config を読まない）。
    #[allow(clippy::too_many_arguments)]
    pub(super) fn push(
        &self,
        work: &Work,
        cell: &Cell,
        task: &prompt::Task,
        place: &Place,
        ready: &Ready,
        branch: &Branch,
        outcome: &Outcome,
        round: u32,
        write: &Credentials,
    ) -> Result<Pushed> {
        let staging = crate::pr::stage(self.probe, &place.worktree, &branch.name)?;
        let staging_path = path_str(staging.path())?;
        let mut env = ISOLATED_GIT_ENV.to_vec();
        env.push(("GIT_ALLOW_PROTOCOL", "file"));
        let base_ref = format!("+refs/heads/{0}:refs/heads/{0}", branch.base);
        git(
            self.probe,
            &[
                "-C",
                &staging_path,
                "fetch",
                "-q",
                "--no-tags",
                &path_str(&place.cache)?,
                &base_ref,
            ],
            &env,
        )?;
        let range = format!("{}..refs/heads/{}", branch.start, branch.name);
        let count = git_out(
            self.probe,
            &["-C", &staging_path, "rev-list", "--count", &range],
        )?;
        if count.trim() == "0" {
            return Ok(Pushed::Nothing);
        }
        // IssueからPRに進まないcell（triageなど）が足したコミットは、pushしない
        let wants_pr = work.item == Target::Issue
            && self
                .config
                .cell(&outcome.next)
                .is_some_and(|next| !next.on.contains(&Target::Issue));
        // PRを作るcell（implementなど。PRを扱うcellに進めるもの）が、途中で人間に回すときは、PRは作らずに
        // ブランチだけをpushする。pushしないと、作業場所ごと消えて、次の実行が最初からやり直しになるため
        let keeps_branch = work.item == Target::Issue
            && outcome.next == transition::NEEDS_HUMAN
            && cell.next.iter().any(|n| {
                self.config
                    .cell(n)
                    .is_some_and(|next| !next.on.contains(&Target::Issue))
            });
        if work.item == Target::Issue && !wants_pr && !keeps_branch {
            return Ok(Pushed::Nothing);
        }
        if let Some(refusal) = crate::pr::verify(
            self.probe,
            staging.path(),
            &branch.start,
            &format!("refs/heads/{}", branch.base),
            &branch.name,
            &ready.placed_blobs,
            self.scanner,
        )? {
            // pushしない作業も、人が読んだり取り出したりできるように、手元に残す（作業場所は消えるため）
            let text = format!(
                "pushしませんでした: {}\n\n次にすること: {}\n\n{}",
                refusal.reason,
                refusal.next,
                self.keep_work(place, staging.path(), branch)
            );
            return Ok(Pushed::Refused(text));
        }
        crate::pr::push_staged(self.probe, write, staging.path(), &work.repo, &branch.name)?;
        if work.item == Target::PullRequest {
            return Ok(Pushed::Done(None));
        }
        if keeps_branch {
            // 同じブランチのPRが開いていれば（人がPRのあるIssueに付け直したときなど）、そのPRに足したことになる
            let open = crate::pr::find_open(
                self.probe,
                write,
                &work.repo,
                &branch.name,
                &self.actors.writer,
            )?;
            return Ok(Pushed::Branch(branch.name.clone(), open));
        }
        let number = match crate::pr::find_open(
            self.probe,
            write,
            &work.repo,
            &branch.name,
            &self.actors.writer,
        )? {
            Some(number) => number,
            None => {
                let body = crate::pr::body(
                    &cell.name,
                    round,
                    outcome.comment.trim(),
                    &work.repo,
                    work.number,
                );
                let found = actions::scan_text(self.scanner, &body)?;
                if !found.is_empty() {
                    return Ok(Pushed::Refused(format!(
                        "PRの本文に秘密らしいものがあったので、PRを作りませんでした（ブランチ {} はpushしました）。agentcellsを動かしている人は、手元の記録（{}）を確かめてください",
                        branch.name,
                        place.run_name()
                    )));
                }
                let args = crate::pr::create_args(
                    &work.repo,
                    &branch.base,
                    &branch.name,
                    &task.discussion.title,
                    &body,
                    &self.actors.operator,
                );
                crate::pr::create(self.probe, write, &args)?
            }
        };
        Ok(Pushed::Done(Some(number)))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::process::Command;

    use super::*;

    #[test]
    fn refused_work_can_be_fetched_from_the_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let git = |cwd: &Path, args: &[&str]| {
            let out = Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
                .args(args)
                .current_dir(cwd)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("a.txt"), "a").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "base"]);
        let start = git(&repo, &["rev-parse", "HEAD"]);
        git(&repo, &["checkout", "-q", "-b", "ac/x"]);
        std::fs::write(repo.join("b.txt"), "b").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "ai work"]);
        let head = git(&repo, &["rev-parse", "HEAD"]);
        let branch = Branch {
            name: "ac/x".into(),
            base: "main".into(),
            start: start.clone(),
        };
        let kept = dir.path().join("kept");
        let bundle = kept.join("octo/app/3/100.bundle");
        keep_bundle(&crate::probe::SystemProbe, &repo, &branch, &kept, &bundle).unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&bundle), 0o600);
        assert_eq!(mode(&kept), 0o700);
        assert_eq!(mode(&kept.join("octo/app/3")), 0o700);
        // 人の手元のclone（AIの作業はない）に、書いたとおりのコマンドで取り出せる
        let human = dir.path().join("human");
        let repo_arg = repo.to_str().unwrap();
        git(
            dir.path(),
            &["clone", "-q", "-b", "main", repo_arg, "human"],
        );
        git(
            &human,
            &["fetch", "-q", bundle.to_str().unwrap(), "ac/x:ai-work"],
        );
        assert_eq!(git(&human, &["rev-parse", "ai-work"]), head);

        // 古いものは消す
        prune_kept(&kept, 100 + KEPT_DAYS * 24 * 3600 - 1);
        assert!(bundle.exists());
        prune_kept(&kept, 100 + KEPT_DAYS * 24 * 3600 + 1);
        assert!(!bundle.exists());
        assert!(!kept.join("octo").exists());
        assert!(kept.exists());
    }

    #[test]
    fn finds_files_that_conflict_with_the_base() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("日本.txt"), "a").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "base"]);
        git(&["checkout", "-q", "-b", "ac/x"]);
        std::fs::write(repo.join("日本.txt"), "pr").unwrap();
        git(&["commit", "-qam", "pr"]);
        git(&["checkout", "-q", "main"]);
        std::fs::write(repo.join("b.txt"), "b").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "main"]);
        let main = git(&["rev-parse", "HEAD"]);
        git(&["update-ref", "refs/remotes/origin/main", &main]);
        git(&["checkout", "-q", "ac/x"]);
        let branch = Branch {
            name: "ac/x".into(),
            base: "main".into(),
            start: String::new(),
        };
        let probe = crate::probe::SystemProbe;
        // 別のファイルを変えただけなら、ぶつからない
        assert_eq!(
            check_base(&probe, &repo, &branch),
            BaseCheck::Clean { base: main.clone() }
        );
        git(&["checkout", "-q", "main"]);
        std::fs::write(repo.join("日本.txt"), "main").unwrap();
        git(&["commit", "-qam", "main2"]);
        let main = git(&["rev-parse", "HEAD"]);
        git(&["update-ref", "refs/remotes/origin/main", &main]);
        git(&["checkout", "-q", "ac/x"]);
        assert_eq!(
            check_base(&probe, &repo, &branch),
            BaseCheck::Conflict(prompt::Conflict {
                base: "main".into(),
                files: vec!["日本.txt".into()],
            })
        );
        // ベースのブランチを読めなければ、分からない
        let gone = Branch {
            base: "gone".into(),
            ..branch
        };
        assert_eq!(check_base(&probe, &repo, &gone), BaseCheck::Unknown);
    }
}
