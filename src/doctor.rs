use std::ffi::OsStr;
use std::path::Path;

use crate::config::Config;
use crate::home::Home;
use crate::probe::Probe;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

/// 1つの確認項目の結果。だめなときは直し方を持つ。
#[derive(Debug, Clone)]
pub struct Check {
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
    pub fix: Option<String>,
}

impl Check {
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Ok,
            detail: detail.into(),
            fix: None,
        }
    }

    fn warn(name: &'static str, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Warn,
            detail: detail.into(),
            fix: Some(fix.into()),
        }
    }

    fn fail(name: &'static str, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Fail,
            detail: detail.into(),
            fix: Some(fix.into()),
        }
    }
}

/// すべての項目を確かめる。前の項目がだめで確かめられないものは飛ばさずに、だめとして返す。
pub fn run(home: &Home, path_env: Option<&OsStr>, probe: &dyn Probe) -> Vec<Check> {
    let mut checks = vec![check_git(probe)];
    let gh = check_gh(probe);
    let gh_found = gh.status == Status::Ok;
    checks.push(gh);
    checks.push(check_gh_auth(probe, gh_found));
    let claude = check_claude(probe);
    let claude_found = claude.status == Status::Ok;
    checks.push(claude);
    checks.push(check_claude_auth(probe, claude_found));
    checks.push(check_home(home));
    checks.push(check_path(home, path_env));
    checks.push(check_config(home));
    checks
}

pub fn has_failure(checks: &[Check]) -> bool {
    checks.iter().any(|c| c.status == Status::Fail)
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").trim().to_string()
}

fn check_git(probe: &dyn Probe) -> Check {
    match probe.run("git", &["--version"]) {
        Some(out) if out.success => Check::ok("git", first_line(&out.stdout)),
        _ => Check::fail(
            "git",
            "gitが見つかりません",
            "macOSなら `xcode-select --install` を実行してください（ほかの環境は https://git-scm.com/downloads ）",
        ),
    }
}

fn check_gh(probe: &dyn Probe) -> Check {
    match probe.run("gh", &["--version"]) {
        Some(out) if out.success => Check::ok("gh", first_line(&out.stdout)),
        _ => Check::fail(
            "gh",
            "GitHub CLI（gh）が見つかりません",
            "https://cli.github.com/ の手順で入れてください（Homebrewなら `brew install gh`）",
        ),
    }
}

fn check_gh_auth(probe: &dyn Probe, gh_found: bool) -> Check {
    if !gh_found {
        return Check::fail(
            "gh auth",
            "ghがないので確かめられません",
            "先にghを入れてください",
        );
    }
    match probe.run("gh", &["auth", "status"]) {
        Some(out) if out.success => Check::ok("gh auth", "GitHubにログインしています"),
        _ => Check::fail(
            "gh auth",
            "GitHubにログインしていません",
            "`gh auth login` を実行してください",
        ),
    }
}

fn check_claude(probe: &dyn Probe) -> Check {
    match probe.run("claude", &["--version"]) {
        Some(out) if out.success => Check::ok("claude", first_line(&out.stdout)),
        _ => Check::fail(
            "claude",
            "Claude Codeが見つかりません",
            "https://claude.com/claude-code の手順で入れてください",
        ),
    }
}

fn check_claude_auth(probe: &dyn Probe, claude_found: bool) -> Check {
    const NAME: &str = "claude auth";
    if !claude_found {
        return Check::fail(
            NAME,
            "Claude Codeがないので確かめられません",
            "先にClaude Codeを入れてください",
        );
    }
    let Some(out) = probe.run("claude", &["auth", "status"]) else {
        return Check::fail(
            NAME,
            "ログイン状態を確かめられません",
            "`claude auth status` を実行して、エラーの内容を確かめてください",
        );
    };
    let Ok(status) = serde_json::from_str::<serde_json::Value>(&out.stdout) else {
        return Check::warn(
            NAME,
            "ログイン状態の出力を読めません（Claude Codeの出力形式が変わった可能性があります）",
            "`claude auth status` を実行して、ログインしているか確かめてください",
        );
    };
    if status["loggedIn"].as_bool() != Some(true) {
        return Check::fail(
            NAME,
            "Claude Codeにログインしていません",
            "`claude auth login` を実行してください",
        );
    }
    // agentcellsはサブスクリプションの範囲で回すので、APIキーでのログインは課金が別になることを知らせる。
    match status["authMethod"].as_str() {
        Some("claude.ai") => {
            let plan = status["subscriptionType"].as_str().unwrap_or("不明");
            Check::ok(
                NAME,
                format!("サブスクリプションでログインしています（{plan}）"),
            )
        }
        method => Check::warn(
            NAME,
            format!(
                "サブスクリプション以外の方法でログインしています（{}）。APIの利用料がかかります",
                method.unwrap_or("不明")
            ),
            "サブスクリプションで使うなら `claude auth login` でclaude.aiのアカウントにログインしてください",
        ),
    }
}

fn check_home(home: &Home) -> Check {
    if home.root().is_dir() {
        Check::ok("home", home.root().display().to_string())
    } else {
        Check::fail(
            "home",
            format!("{} がありません", home.root().display()),
            "`agentcells setup` を実行してください",
        )
    }
}

fn check_path(home: &Home, path_env: Option<&OsStr>) -> Check {
    let bin = home.bin_dir();
    let in_path = path_env
        .map(|p| std::env::split_paths(p).any(|dir| same_dir(&dir, &bin)))
        .unwrap_or(false);
    if in_path {
        Check::ok("PATH", format!("{} はPATHに入っています", bin.display()))
    } else {
        Check::warn(
            "PATH",
            format!("{} がPATHに入っていません", bin.display()),
            format!(
                "シェルの設定ファイル（zshなら ~/.zshrc）に `export PATH=\"{}:$PATH\"` を書き足して、ターミナルを開き直してください",
                bin.display()
            ),
        )
    }
}

fn check_config(home: &Home) -> Check {
    const NAME: &str = "config";
    let path = home.config_path();
    if !path.exists() {
        return Check::warn(
            NAME,
            format!("{} がありません", path.display()),
            "`agentcells setup` を実行すると雛形が置かれます",
        );
    }
    match Config::load(&path) {
        Err(err) => Check::fail(
            NAME,
            err.message,
            err.hint
                .unwrap_or_else(|| "設定ファイルを直してください".to_string()),
        ),
        Ok(config) if config.products.is_empty() => Check::warn(
            NAME,
            "productがまだありません",
            format!(
                "{} に [[product]] と [[cell]] を書いてください（雛形のコメントに例があります）",
                path.display()
            ),
        ),
        Ok(config) => Check::ok(
            NAME,
            format!(
                "product {}件、cell {}件",
                config.products.len(),
                config.cells.len()
            ),
        ),
    }
}

fn same_dir(a: &Path, b: &Path) -> bool {
    a == b || matches!((a.canonicalize(), b.canonicalize()), (Ok(x), Ok(y)) if x == y)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::fake::FakeProbe;
    use std::ffi::OsString;

    fn healthy() -> FakeProbe {
        FakeProbe::default()
            .with("git --version", true, "git version 2.50.1\n")
            .with("gh --version", true, "gh version 2.87.2\n")
            .with("gh auth status", true, "")
            .with("claude --version", true, "2.1.310 (Claude Code)\n")
            .with(
                "claude auth status",
                true,
                r#"{"loggedIn":true,"authMethod":"claude.ai","subscriptionType":"pro"}"#,
            )
    }

    fn find<'a>(checks: &'a [Check], name: &str) -> &'a Check {
        checks.iter().find(|c| c.name == name).unwrap()
    }

    const CONFIG: &str = r#"
[[product]]
name = "app"
issues = "octo/app"
repos = ["octo/app"]
cells = ["triage"]

[[cell]]
name = "triage"
on = "issue"
label = "ac:triage"
prompt = "triage.md"
"#;

    /// PATHに入っていて、正しい設定ファイルがある置き場所を作る。
    fn setup_home() -> (tempfile::TempDir, Home, OsString) {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::new(dir.path().join(".agentcells"));
        std::fs::create_dir_all(home.bin_dir()).unwrap();
        std::fs::write(home.config_path(), CONFIG).unwrap();
        std::fs::write(home.root().join("triage.md"), "prompt").unwrap();
        let path = std::env::join_paths([home.bin_dir(), "/usr/bin".into()]).unwrap();
        (dir, home, path)
    }

    #[test]
    fn missing_config_warns_and_points_to_setup() {
        let (_dir, home, path) = setup_home();
        std::fs::remove_file(home.config_path()).unwrap();
        let check = find(&run(&home, Some(&path), &healthy()), "config").clone();
        assert_eq!(check.status, Status::Warn);
        assert!(check.fix.unwrap().contains("agentcells setup"));
    }

    #[test]
    fn invalid_config_fails_with_problems() {
        let (_dir, home, path) = setup_home();
        std::fs::remove_file(home.root().join("triage.md")).unwrap();
        let check = find(&run(&home, Some(&path), &healthy()), "config").clone();
        assert_eq!(check.status, Status::Fail);
        assert!(check.detail.contains("triage.md"), "{}", check.detail);
    }

    #[test]
    fn config_without_products_warns() {
        let (_dir, home, path) = setup_home();
        std::fs::write(home.config_path(), crate::config::TEMPLATE).unwrap();
        let check = find(&run(&home, Some(&path), &healthy()), "config").clone();
        assert_eq!(check.status, Status::Warn);
        assert!(check.fix.unwrap().contains("[[product]]"));
    }

    #[test]
    fn all_ok_when_everything_is_ready() {
        let (_dir, home, path) = setup_home();
        let checks = run(&home, Some(&path), &healthy());
        for c in &checks {
            assert_eq!(c.status, Status::Ok, "{}: {}", c.name, c.detail);
        }
        assert!(!has_failure(&checks));
    }

    #[test]
    fn missing_gh_also_fails_auth_with_fix() {
        let (_dir, home, path) = setup_home();
        let mut probe = healthy();
        probe.outputs.remove("gh --version");
        let checks = run(&home, Some(&path), &probe);
        assert_eq!(find(&checks, "gh").status, Status::Fail);
        assert_eq!(find(&checks, "gh auth").status, Status::Fail);
        assert!(find(&checks, "gh").fix.is_some());
        assert!(has_failure(&checks));
    }

    #[test]
    fn gh_not_logged_in_fails() {
        let (_dir, home, path) = setup_home();
        let probe = healthy().with("gh auth status", false, "");
        let check = find(&run(&home, Some(&path), &probe), "gh auth").clone();
        assert_eq!(check.status, Status::Fail);
        assert!(check.fix.unwrap().contains("gh auth login"));
    }

    #[test]
    fn claude_not_logged_in_fails() {
        let (_dir, home, path) = setup_home();
        let probe = healthy().with("claude auth status", false, r#"{"loggedIn":false}"#);
        let check = find(&run(&home, Some(&path), &probe), "claude auth").clone();
        assert_eq!(check.status, Status::Fail);
        assert!(check.fix.unwrap().contains("claude auth login"));
    }

    #[test]
    fn claude_with_api_key_warns_about_billing() {
        let (_dir, home, path) = setup_home();
        let probe = healthy().with(
            "claude auth status",
            true,
            r#"{"loggedIn":true,"authMethod":"api_key"}"#,
        );
        let check = find(&run(&home, Some(&path), &probe), "claude auth").clone();
        assert_eq!(check.status, Status::Warn);
        assert!(check.detail.contains("api_key"));
    }

    #[test]
    fn unreadable_claude_auth_output_warns() {
        let (_dir, home, path) = setup_home();
        let probe = healthy().with("claude auth status", true, "Logged in");
        let check = find(&run(&home, Some(&path), &probe), "claude auth").clone();
        assert_eq!(check.status, Status::Warn);
    }

    #[test]
    fn missing_home_fails_and_points_to_setup() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::new(dir.path().join("missing"));
        let check = find(&run(&home, None, &healthy()), "home").clone();
        assert_eq!(check.status, Status::Fail);
        assert!(check.fix.unwrap().contains("agentcells setup"));
    }

    #[test]
    fn bin_outside_path_warns_with_export_line() {
        let (_dir, home, _path) = setup_home();
        let path = OsString::from("/usr/bin:/bin");
        let check = find(&run(&home, Some(&path), &healthy()), "PATH").clone();
        assert_eq!(check.status, Status::Warn);
        assert!(check.fix.unwrap().contains("export PATH="));
    }
}
