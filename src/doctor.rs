use std::ffi::OsStr;
use std::path::Path;

use crate::config::Config;
use crate::github;
use crate::home::Home;
use crate::labels;
use crate::probe::Probe;
use crate::settings;

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
/// `schedule` は、定期実行のplistの場所。launchdのない環境では `None`（確かめない）。
pub fn run(
    home: &Home,
    path_env: Option<&OsStr>,
    schedule: Option<&Path>,
    probe: &dyn Probe,
) -> Vec<Check> {
    let mut checks = vec![check_git(probe)];
    let gh = check_gh(probe);
    let gh_found = gh.status == Status::Ok;
    checks.push(gh);
    checks.push(check_gh_auth(probe, gh_found));
    let claude = check_claude(probe);
    let claude_found = claude.status == Status::Ok;
    checks.push(claude);
    checks.push(check_claude_auth(probe, claude_found));
    checks.push(check_betterleaks(probe));
    checks.push(check_home(home));
    if let Some(note) = crate::run::stuck(home) {
        checks.push(Check::fail(
            "stuck",
            note,
            "残ったプロセスを止めてください（PIDは上のとおり。詳しくは ~/.agentcells/runs/ の error.txt）",
        ));
    }
    checks.push(check_path(home, path_env));
    if let Some(plist) = schedule {
        checks.push(check_schedule(home, plist, probe));
    }
    let (config_check, config) = check_config(home);
    // 設定が正しくないと、書き間違えたcellのラベルが抜けて「そろっています」と出ることがあるので、確かめない
    let config_ok = config_check.status != Status::Fail;
    checks.push(config_check);
    checks.push(check_github(config.as_ref(), probe));
    if let Some(config) = &config
        && config_ok
        && !config.products.is_empty()
    {
        checks.push(check_labels(config, probe));
    }
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

/// AIのコミットに秘密が入っていないかを、betterleaksで調べる。ないと、AIはコミットできない。
fn check_betterleaks(probe: &dyn Probe) -> Check {
    match probe.run("betterleaks", &["version"]) {
        Some(out) if out.success => Check::ok("betterleaks", first_line(&out.stdout)),
        _ => Check::fail(
            "betterleaks",
            "betterleaksが見つかりません。AIのコミットに秘密が入っていないかを調べるのに使います（ないとコミットできません）",
            "Homebrewなら `brew install betterleaks`（ほかの環境は https://github.com/betterleaks/betterleaks ）",
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

fn check_schedule(home: &Home, plist: &Path, probe: &dyn Probe) -> Check {
    const NAME: &str = "schedule";
    match crate::schedule::state(probe, plist) {
        crate::schedule::State::NotInstalled => Check::warn(
            NAME,
            "定期実行が登録されていません（tickは、手で動かしたときだけ動きます）",
            "`agentcells schedule install --every 10` で、10分ごとに動くように登録できます",
        ),
        crate::schedule::State::NotLoaded => Check::warn(
            NAME,
            format!(
                "{} はありますが、launchdに読み込まれていません",
                plist.display()
            ),
            "`agentcells schedule install` で登録し直してください",
        ),
        crate::schedule::State::MissingProgram(program) => Check::fail(
            NAME,
            format!(
                "定期実行が動かすagentcells（{}）がありません",
                program.display()
            ),
            "install.sh で入れ直すか、`agentcells schedule install` で登録し直してください",
        ),
        crate::schedule::State::Loaded { every } => {
            let every = every.map_or(String::new(), |m| format!("{m}分ごとに"));
            Check::ok(
                NAME,
                format!(
                    "{every}tickを動かしています（ログ: {}）",
                    crate::schedule::log_path(home).display()
                ),
            )
        }
    }
}

/// 設定ファイルを確かめる。読めたら、ほかの確認に使えるように設定も返す。
/// 中身に問題があっても、TOMLとして読めれば返す（プロンプトがなくても、GitHubの確認は進められるように）。
fn check_config(home: &Home) -> (Check, Option<Config>) {
    const NAME: &str = "config";
    let path = home.config_path();
    if !path.exists() {
        let check = Check::warn(
            NAME,
            format!("{} がありません", path.display()),
            "`agentcells setup` を実行すると雛形が置かれます",
        );
        return (check, None);
    }
    let config = match Config::load(&path) {
        Ok(config) => config,
        Err(err) => {
            let check = Check::fail(
                NAME,
                err.message,
                err.hint
                    .unwrap_or_else(|| "設定ファイルを直してください".to_string()),
            );
            return (check, Config::load_unchecked(&path));
        }
    };
    let problems = settings::sandbox_problems(home, &config);
    if !problems.is_empty() {
        let check = Check::fail(
            NAME,
            problems.join("\n"),
            "[product.sandbox] から、その場所を外してください",
        );
        return (check, Some(config));
    }
    let warnings = settings::sandbox_warnings(&config);
    let check = match &config {
        config if config.products.is_empty() => Check::warn(
            NAME,
            "productがまだありません",
            format!(
                "{} に [[product]] と [[cell]] を書いてください（雛形のコメントに例があります）",
                path.display()
            ),
        ),
        _ if !warnings.is_empty() => Check::warn(
            NAME,
            warnings.join("\n"),
            "[product.sandbox] で開けた場所は、AIが書き換えたビルドから使えます。要るものだけにしてください",
        ),
        config => Check::ok(
            NAME,
            format!(
                "product {}件、cell {}件",
                config.products.len(),
                config.cells.len()
            ),
        ),
    };
    (check, Some(config))
}

/// cellが使うラベルが、リポジトリにそろっているか。足りなければ、作り方を出す（作りはしない）。
fn check_labels(config: &Config, probe: &dyn Probe) -> Check {
    const NAME: &str = "labels";
    match labels::missing(config, probe) {
        Ok(missing) if missing.is_empty() => Check::ok(NAME, "必要なラベルがそろっています"),
        Ok(missing) => Check::fail(
            NAME,
            format!(
                "足りないラベルがあります:\n{}",
                labels::describe_missing(&missing)
            ),
            "`agentcells labels sync` で作れます（あるラベルは触りません）",
        ),
        Err(e) => Check::warn(
            NAME,
            format!("ラベルを確かめられません: {}", e.message),
            e.hint
                .unwrap_or_else(|| "ghのログインとリポジトリの名前を確かめてください".to_string()),
        ),
    }
}

/// 書き込みに使うアカウントを確かめる。GitHub Appを推奨し、なければghのアカウントで動くことを知らせる。
fn check_github(config: Option<&Config>, probe: &dyn Probe) -> Check {
    const NAME: &str = "github";
    let Some(config) = config else {
        return Check::warn(
            NAME,
            "設定ファイルを読めないので、書き込みに使うアカウントを確かめられません",
            "先に config の問題を直してください",
        );
    };
    let Some(app) = &config.github_app else {
        return Check::warn(
            NAME,
            "ghにログインしているあなたのアカウントで書き込みます。AIのコメントやPRがあなたの名前になり、人間の発言と見分けにくくなります",
            "GitHub Appを作り、config.toml の [github_app] に書くのがおすすめです（手順は README の「GitHub App」）",
        );
    };
    // 鍵の権限はネットワークに出ずに確かめられるので先に見て、ほかの結果と一緒に知らせる。
    let loose = loose_permissions(&app.private_key).map(|mode| {
        format!(
            "秘密鍵 {} をほかのユーザーも読めます（権限 {mode:o}）。`chmod 600 {}` を実行してください",
            app.private_key.display(),
            app.private_key.display()
        )
    });
    let with_loose = |fix: String| match &loose {
        Some(note) => format!("{fix}。あわせて、{note}"),
        None => fix,
    };
    // Appとしての呼び出しにはcurlを使う（JWTをBearerで送るため）
    if probe
        .run("curl", &["--version"])
        .is_none_or(|out| !out.success)
    {
        return Check::fail(
            NAME,
            "GitHub Appを使うのに要るcurlが見つかりません",
            with_loose(
                "curlを入れてください（Debian/Ubuntuなら `sudo apt install curl`）".to_string(),
            ),
        );
    }
    let fail = |err: crate::error::Error| {
        Check::fail(
            NAME,
            err.message,
            with_loose(
                err.hint
                    .unwrap_or_else(|| "[github_app] の設定を確かめてください".to_string()),
            ),
        )
    };
    let jwt = match github::now().and_then(|now| github::jwt(app, now)) {
        Ok(jwt) => jwt,
        Err(err) => return fail(err),
    };
    let info = match github::app_info(probe, &jwt) {
        Ok(info) => info,
        Err(err) => return fail(err),
    };
    let slug = &info.slug;
    let missing_permissions = info.missing_permissions();
    if !missing_permissions.is_empty() {
        return Check::fail(
            NAME,
            format!(
                "GitHub App「{slug}」に要る権限が足りません: {}",
                missing_permissions.join(", ")
            ),
            with_loose(format!(
                "https://github.com/settings/apps/{slug}/permissions で Read and write にし、インストール先で新しい権限を承認してください"
            )),
        );
    }
    let mut missing = Vec::new();
    for repo in config.repos() {
        match github::installation_id(probe, &jwt, repo) {
            Ok(Some(_)) => {}
            Ok(None) => missing.push(repo.to_string()),
            Err(err) => return fail(err),
        }
    }
    if !missing.is_empty() {
        return Check::fail(
            NAME,
            format!(
                "GitHub App「{slug}」が次のリポジトリにインストールされていないか、リポジトリが見つかりません: {}",
                missing.join(", ")
            ),
            with_loose(format!(
                "https://github.com/apps/{slug}/installations/new からインストールしてください。リポジトリ名も確かめてください"
            )),
        );
    }
    let detail = format!(
        "GitHub App「{slug}」で書き込みます（{}件のリポジトリ）",
        config.repos().len()
    );
    match loose {
        Some(note) => Check::warn(NAME, detail, note),
        None => Check::ok(NAME, detail),
    }
}

/// 持ち主以外も読み書きできる権限なら、その権限を返す。
#[cfg(unix)]
fn loose_permissions(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path).ok()?.permissions().mode() & 0o777;
    (mode & 0o077 != 0).then_some(mode)
}

#[cfg(not(unix))]
fn loose_permissions(_path: &Path) -> Option<u32> {
    None
}

fn same_dir(a: &Path, b: &Path) -> bool {
    a == b || matches!((a.canonicalize(), b.canonicalize()), (Ok(x), Ok(y)) if x == y)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::fake;
    use crate::probe::fake::FakeProbe;
    use std::ffi::OsString;

    fn healthy() -> FakeProbe {
        with_app(FakeProbe::default())
            .with("git --version", true, "git version 2.50.1\n")
            .with("gh --version", true, "gh version 2.87.2\n")
            .with("gh auth status", true, "")
            .with("claude --version", true, "2.1.310 (Claude Code)\n")
            .with("betterleaks version", true, "1.9.0\n")
            .with(
                "gh label list --repo octo/app --json name --limit 1000",
                true,
                r#"[{"name":"ac:working"},{"name":"ac:needs-human"},{"name":"ac:in-review"},{"name":"ac:triage"}]"#,
            )
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
operator = "me"

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

    const GITHUB_APP: &str = r#"
[github_app]
id = "Iv23liTEST"
private_key = "github-app.pem"
"#;

    /// GitHub Appの呼び出しに答える。JWTは標準入力で渡るので、引数は毎回同じになる。
    fn with_app(probe: FakeProbe) -> FakeProbe {
        probe
            .with("curl --version", true, "curl 8.7.1\n")
            .with_output(
                fake::key("GET", "/app", None),
                fake::response(
                    200,
                    r#"{"slug":"agentcells-bot","permissions":{"contents":"write","issues":"write","pull_requests":"write","metadata":"read"}}"#,
                ),
            )
            .with_output(
                fake::key("GET", "/repos/octo/app/installation", None),
                fake::response(200, r#"{"id":42}"#),
            )
    }

    /// PATHに入っていて、GitHub Appを使う正しい設定ファイルがある置き場所を作る。
    fn setup_home() -> (tempfile::TempDir, Home, OsString) {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::new(dir.path().join(".agentcells"));
        std::fs::create_dir_all(home.bin_dir()).unwrap();
        std::fs::write(home.config_path(), format!("{CONFIG}{GITHUB_APP}")).unwrap();
        std::fs::write(home.root().join("triage.md"), "prompt").unwrap();
        let key = home.root().join("github-app.pem");
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/github-app-test-key.pem"),
            &key,
        )
        .unwrap();
        set_mode(&key, 0o600);
        let path = std::env::join_paths([home.bin_dir(), "/usr/bin".into()]).unwrap();
        (dir, home, path)
    }

    #[test]
    fn missing_config_warns_and_points_to_setup() {
        let (_dir, home, path) = setup_home();
        std::fs::remove_file(home.config_path()).unwrap();
        let check = find(&run(&home, Some(&path), None, &healthy()), "config").clone();
        assert_eq!(check.status, Status::Warn);
        assert!(check.fix.unwrap().contains("agentcells setup"));
    }

    #[test]
    fn invalid_config_fails_with_problems() {
        let (_dir, home, path) = setup_home();
        std::fs::remove_file(home.root().join("triage.md")).unwrap();
        let check = find(&run(&home, Some(&path), None, &healthy()), "config").clone();
        assert_eq!(check.status, Status::Fail);
        assert!(check.detail.contains("triage.md"), "{}", check.detail);
    }

    #[test]
    fn config_without_products_warns() {
        let (_dir, home, path) = setup_home();
        std::fs::write(home.config_path(), crate::config::TEMPLATE).unwrap();
        let check = find(&run(&home, Some(&path), None, &healthy()), "config").clone();
        assert_eq!(check.status, Status::Warn);
        assert!(check.fix.unwrap().contains("[[product]]"));
    }

    #[test]
    fn schedule_is_checked_only_when_launchd_is_available() {
        let (dir, home, path) = setup_home();
        let checks = run(&home, Some(&path), None, &healthy());
        assert!(checks.iter().all(|c| c.name != "schedule"));
        let plist = crate::schedule::plist_path(dir.path());
        let check = find(
            &run(&home, Some(&path), Some(&plist), &healthy()),
            "schedule",
        )
        .clone();
        assert_eq!(check.status, Status::Warn);
        assert!(check.fix.unwrap().contains("agentcells schedule install"));
    }

    #[test]
    fn all_ok_when_everything_is_ready() {
        let (_dir, home, path) = setup_home();
        let checks = run(&home, Some(&path), None, &healthy());
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
        let checks = run(&home, Some(&path), None, &probe);
        assert_eq!(find(&checks, "gh").status, Status::Fail);
        assert_eq!(find(&checks, "gh auth").status, Status::Fail);
        assert!(find(&checks, "gh").fix.is_some());
        assert!(has_failure(&checks));
    }

    #[test]
    fn gh_not_logged_in_fails() {
        let (_dir, home, path) = setup_home();
        let probe = healthy().with("gh auth status", false, "");
        let check = find(&run(&home, Some(&path), None, &probe), "gh auth").clone();
        assert_eq!(check.status, Status::Fail);
        assert!(check.fix.unwrap().contains("gh auth login"));
    }

    #[test]
    fn claude_not_logged_in_fails() {
        let (_dir, home, path) = setup_home();
        let probe = healthy().with("claude auth status", false, r#"{"loggedIn":false}"#);
        let check = find(&run(&home, Some(&path), None, &probe), "claude auth").clone();
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
        let check = find(&run(&home, Some(&path), None, &probe), "claude auth").clone();
        assert_eq!(check.status, Status::Warn);
        assert!(check.detail.contains("api_key"));
    }

    #[test]
    fn unreadable_claude_auth_output_warns() {
        let (_dir, home, path) = setup_home();
        let probe = healthy().with("claude auth status", true, "Logged in");
        let check = find(&run(&home, Some(&path), None, &probe), "claude auth").clone();
        assert_eq!(check.status, Status::Warn);
    }

    #[test]
    fn missing_home_fails_and_points_to_setup() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::new(dir.path().join("missing"));
        let check = find(&run(&home, None, None, &healthy()), "home").clone();
        assert_eq!(check.status, Status::Fail);
        assert!(check.fix.unwrap().contains("agentcells setup"));
    }

    fn set_mode(path: &Path, mode: u32) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        #[cfg(not(unix))]
        let _ = (path, mode);
    }

    fn installation(repo: &str) -> String {
        fake::key("GET", &format!("/repos/{repo}/installation"), None)
    }

    fn not_found() -> crate::probe::Output {
        fake::response(404, r#"{"message":"Not Found"}"#)
    }

    #[test]
    fn github_app_without_needed_permissions_fails() {
        let (_dir, home, path) = setup_home();
        let probe = healthy().with_output(
            fake::key("GET", "/app", None),
            fake::response(
                200,
                r#"{"slug":"agentcells-bot","permissions":{"contents":"write","metadata":"read"}}"#,
            ),
        );
        let check = find(&run(&home, Some(&path), None, &probe), "github").clone();
        assert_eq!(check.status, Status::Fail);
        assert!(check.detail.contains("issues: write"), "{}", check.detail);
        assert!(check.fix.unwrap().contains("/permissions"));
    }

    #[test]
    fn github_app_without_curl_fails() {
        let (_dir, home, path) = setup_home();
        let mut probe = healthy();
        probe.outputs.remove("curl --version");
        let check = find(&run(&home, Some(&path), None, &probe), "github").clone();
        assert_eq!(check.status, Status::Fail);
        assert!(check.detail.contains("curl"));
    }

    #[cfg(unix)]
    #[test]
    fn readable_private_key_is_reported_even_without_curl() {
        let (_dir, home, path) = setup_home();
        set_mode(&home.root().join("github-app.pem"), 0o644);
        let mut probe = healthy();
        probe.outputs.remove("curl --version");
        let check = find(&run(&home, Some(&path), None, &probe), "github").clone();
        assert_eq!(check.status, Status::Fail);
        assert!(check.fix.unwrap().contains("chmod 600"));
    }

    #[test]
    fn github_is_checked_even_when_prompts_are_missing() {
        let (_dir, home, path) = setup_home();
        std::fs::remove_file(home.root().join("triage.md")).unwrap();
        let checks = run(&home, Some(&path), None, &healthy());
        assert_eq!(find(&checks, "config").status, Status::Fail);
        let github = find(&checks, "github");
        assert_eq!(github.status, Status::Ok, "{}", github.detail);
    }

    #[test]
    fn github_check_warns_when_config_is_missing() {
        let (_dir, home, path) = setup_home();
        std::fs::remove_file(home.config_path()).unwrap();
        let check = find(&run(&home, Some(&path), None, &healthy()), "github").clone();
        assert_eq!(check.status, Status::Warn);
        assert!(check.fix.unwrap().contains("config"));
    }

    #[test]
    fn broken_private_key_fails_without_calling_github() {
        let (_dir, home, path) = setup_home();
        std::fs::write(home.root().join("github-app.pem"), "not a key").unwrap();
        let probe = healthy();
        let check = find(&run(&home, Some(&path), None, &probe), "github").clone();
        assert_eq!(check.status, Status::Fail);
        assert!(
            check.detail.contains("秘密鍵として読めません"),
            "{}",
            check.detail
        );
        assert!(
            !probe
                .calls
                .borrow()
                .iter()
                .any(|c| c.starts_with("curl --disable"))
        );
    }

    #[test]
    fn lists_every_repo_without_the_app() {
        let (_dir, home, path) = setup_home();
        let config = CONFIG.replace(
            "repos = [\"octo/app\"]",
            "repos = [\"octo/app\", \"octo/web\", \"octo/api\"]",
        );
        std::fs::write(home.config_path(), format!("{config}{GITHUB_APP}")).unwrap();
        let probe = healthy()
            .with_output(installation("octo/web"), fake::response(200, r#"{"id":7}"#))
            .with_output(installation("octo/api"), not_found())
            .with_output(installation("octo/app"), not_found());
        let check = find(&run(&home, Some(&path), None, &probe), "github").clone();
        assert_eq!(check.status, Status::Fail);
        assert!(
            check.detail.ends_with("octo/api, octo/app"),
            "{}",
            check.detail
        );
    }

    #[test]
    fn server_errors_are_not_reported_as_missing_installations() {
        let (_dir, home, path) = setup_home();
        let probe = healthy().with_output(
            installation("octo/app"),
            fake::response(500, r#"{"message":"Server Error"}"#),
        );
        let check = find(&run(&home, Some(&path), None, &probe), "github").clone();
        assert_eq!(check.status, Status::Fail);
        assert!(check.detail.contains("HTTP 500"), "{}", check.detail);
        assert!(!check.detail.contains("インストールされていない"));
    }

    #[test]
    fn without_github_app_warns_that_ai_speaks_as_you() {
        let (_dir, home, path) = setup_home();
        std::fs::write(home.config_path(), CONFIG).unwrap();
        let check = find(&run(&home, Some(&path), None, &healthy()), "github").clone();
        assert_eq!(check.status, Status::Warn);
        assert!(
            check.detail.contains("あなたのアカウント"),
            "{}",
            check.detail
        );
        assert!(check.fix.unwrap().contains("[github_app]"));
    }

    #[test]
    fn github_app_not_installed_fails_with_install_url() {
        let (_dir, home, path) = setup_home();
        let probe = healthy().with_output(installation("octo/app"), not_found());
        let check = find(&run(&home, Some(&path), None, &probe), "github").clone();
        assert_eq!(check.status, Status::Fail);
        assert!(check.detail.contains("octo/app"), "{}", check.detail);
        assert!(
            check
                .fix
                .unwrap()
                .contains("https://github.com/apps/agentcells-bot/installations/new")
        );
    }

    #[test]
    fn github_app_with_wrong_key_fails() {
        let (_dir, home, path) = setup_home();
        let probe = healthy().with_output(
            fake::key("GET", "/app", None),
            fake::response(
                401,
                r#"{"message":"A JSON web token could not be decoded"}"#,
            ),
        );
        let check = find(&run(&home, Some(&path), None, &probe), "github").clone();
        assert_eq!(check.status, Status::Fail);
        assert!(check.fix.unwrap().contains("[github_app]"));
    }

    #[cfg(unix)]
    #[test]
    fn readable_private_key_warns_with_chmod() {
        let (_dir, home, path) = setup_home();
        set_mode(&home.root().join("github-app.pem"), 0o644);
        let check = find(&run(&home, Some(&path), None, &healthy()), "github").clone();
        assert_eq!(check.status, Status::Warn);
        assert!(check.fix.unwrap().contains("chmod 600"));
    }

    #[cfg(unix)]
    #[test]
    fn readable_private_key_is_reported_even_when_another_check_fails() {
        let (_dir, home, path) = setup_home();
        set_mode(&home.root().join("github-app.pem"), 0o644);
        let probe = healthy().with_output(installation("octo/app"), not_found());
        let check = find(&run(&home, Some(&path), None, &probe), "github").clone();
        assert_eq!(check.status, Status::Fail);
        assert!(check.fix.unwrap().contains("chmod 600"));
    }

    #[test]
    fn bin_outside_path_warns_with_export_line() {
        let (_dir, home, _path) = setup_home();
        let path = OsString::from("/usr/bin:/bin");
        let check = find(&run(&home, Some(&path), None, &healthy()), "PATH").clone();
        assert_eq!(check.status, Status::Warn);
        assert!(check.fix.unwrap().contains("export PATH="));
    }

    #[test]
    fn wide_sandbox_reads_warn() {
        let (_dir, home, path) = setup_home();
        let text = CONFIG.replace(
            "cells = [\"triage\"]",
            "cells = [\"triage\"]\n\n[product.sandbox]\nread = [\"~\"]",
        );
        std::fs::write(home.config_path(), format!("{text}{GITHUB_APP}")).unwrap();
        let check = find(&run(&home, Some(&path), None, &healthy()), "config").clone();
        assert_eq!(check.status, Status::Warn);
        assert!(check.detail.contains("広すぎます"), "{}", check.detail);
    }

    #[test]
    fn missing_labels_fail_and_point_to_labels_sync() {
        let (_dir, home, path) = setup_home();
        let mut probe = healthy();
        probe.outputs.insert(
            "gh label list --repo octo/app --json name --limit 1000".to_string(),
            crate::probe::Output {
                success: true,
                code: Some(0),
                stdout: r#"[{"name":"ac:working"}]"#.to_string(),
                stderr: String::new(),
            },
        );
        let check = find(&run(&home, Some(&path), None, &probe), "labels").clone();
        assert_eq!(check.status, Status::Fail);
        assert!(
            check
                .detail
                .contains("octo/app: ac:needs-human, ac:in-review, ac:triage"),
            "{}",
            check.detail
        );
        assert!(check.fix.unwrap().contains("agentcells labels sync"));
    }

    #[test]
    fn missing_betterleaks_fails_with_install_hint() {
        let check = check_betterleaks(&FakeProbe::default().with_fallback(false));
        assert_eq!(check.status, Status::Fail);
        assert!(check.fix.unwrap().contains("brew install betterleaks"));
    }

    #[test]
    fn sandbox_reads_into_protected_places_fail_the_config_check() {
        let (_dir, home, path) = setup_home();
        let text = CONFIG.replace(
            "cells = [\"triage\"]",
            &format!(
                "cells = [\"triage\"]\n\n[product.sandbox]\nread = [\"{}\"]",
                home.root().join("files").display()
            ),
        );
        std::fs::write(home.config_path(), format!("{text}{GITHUB_APP}")).unwrap();
        let check = find(&run(&home, Some(&path), None, &healthy()), "config").clone();
        assert_eq!(check.status, Status::Fail);
        assert!(
            check.detail.contains("開けられない場所"),
            "{}",
            check.detail
        );
    }
}
