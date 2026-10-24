//! GitHubに書き込むときの認証。
//!
//! GitHub Appを推奨する。Appのトークンで書き込むと、コメントやPRが「<Appの名前>[bot]」になり、
//! 人間の発言と見分けられる。Appを設定していなければ、ghにログインしているアカウントのトークンで書き込む。
//!
//! トークンは引数に書かず、環境変数で渡す（`ps` で見えないように）。AIの実行には渡さない。
//! AIはBashで、ghやgitの設定ファイル、worktreeの `.git/config` を書き換えられる。
//! そのため、トークンを使うghとgitには、AIが書けるところの設定を読ませない。

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde::Serialize;
use tempfile::TempDir;

use crate::config::{Config, GithubApp};
use crate::error::{Error, Result};
use crate::home::RepoName;
use crate::probe::Probe;

/// git の credential helper がトークンを読む環境変数。
pub const GIT_TOKEN_ENV: &str = "AGENTCELLS_GITHUB_TOKEN";

/// 書き込む先。当面はgithub.comだけを扱う。
const GITHUB_HOST: &str = "github.com";

/// トークンを何に使うか。使い道に要る権限だけをトークンに付ける。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// ラベルを作る
    Labels,
    /// ブランチのpush、PRの作成、コメント、レビュー、ラベルの付け替え
    Work,
}

impl Purpose {
    /// Appに付けた権限のうち、この使い道に要るもの（Workflowsなどは付けない）。
    pub fn permissions(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Purpose::Labels => &[("issues", "write")],
            Purpose::Work => &[
                ("contents", "write"),
                ("issues", "write"),
                ("pull_requests", "write"),
            ],
        }
    }
}

/// JWTの有効期限。GitHubの上限は10分なので、それより短くする。
const JWT_LIFETIME_SECS: u64 = 9 * 60;
/// 時計のずれに備えて、発行時刻を少し前にする。
const CLOCK_SKEW_SECS: u64 = 60;

#[derive(Serialize)]
struct Claims<'a> {
    iat: u64,
    exp: u64,
    iss: &'a str,
}

/// 今の時刻（UNIX時間の秒）。JWTの有効期限に使うので、時計がおかしいときはエラーにする。
pub fn now() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|_| {
            Error::new("この機械の時計が1970年より前になっています")
                .with_hint("時計を合わせてください（GitHub AppのJWTは時刻を使います）")
        })
}

/// GitHub Appとして名乗るためのJWTを作る。
pub fn jwt(app: &GithubApp, now: u64) -> Result<String> {
    let pem = std::fs::read(&app.private_key).map_err(|e| {
        Error::new(format!(
            "GitHub Appの秘密鍵 {} を読めません: {e}",
            app.private_key.display()
        ))
        .with_hint("config.toml の [github_app] の private_key を確かめてください")
    })?;
    let key = EncodingKey::from_rsa_pem(&pem).map_err(|e| {
        Error::new(format!(
            "{} を秘密鍵として読めません: {e}",
            app.private_key.display()
        ))
        .with_hint("GitHub Appの設定画面で作った秘密鍵（.pem）を、そのまま置いてください")
    })?;
    let claims = Claims {
        iat: now.saturating_sub(CLOCK_SKEW_SECS),
        exp: now + JWT_LIFETIME_SECS,
        iss: &app.id,
    };
    encode(&Header::new(Algorithm::RS256), &claims, &key)
        .map_err(|e| Error::new(format!("GitHub AppのJWTを作れません: {e}")))
}

/// トークンを渡すghのための、空の設定ディレクトリ。
///
/// ghは `~/.config/gh/config.yml` を読むが、そこはAIも書ける（`http_unix_socket` を書かれると、通信を横取りされる）。
/// `GH_TOKEN` を渡せば設定がなくても動くので、毎回まっさらなディレクトリを使う。
fn empty_gh_config() -> Result<TempDir> {
    tempfile::Builder::new()
        .prefix("agentcells-gh-")
        .tempdir()
        .map_err(|e| Error::new(format!("ghのための一時ディレクトリを作れません: {e}")))
}

/// トークンを渡してghを動かすときの環境変数。
/// GitHub Enterpriseを向いた `GH_HOST` があっても、github.comに送る。
fn gh_token_env<'a>(token: &'a str, gh_config: &'a Path) -> Result<Vec<(&'static str, &'a str)>> {
    let gh_config = gh_config.to_str().ok_or_else(|| {
        Error::new(format!(
            "一時ディレクトリのパスに使えない文字があります: {}",
            gh_config.display()
        ))
    })?;
    Ok(vec![
        ("GH_TOKEN", token),
        ("GH_HOST", GITHUB_HOST),
        ("GH_CONFIG_DIR", gh_config),
    ])
}

/// GitHub Appとして `gh api` を呼ぶ。JWTは環境変数で渡す（GitHubは `token` 方式で送ったJWTも受け付ける）。
fn api_as_app(probe: &dyn Probe, jwt: &str, args: &[&str]) -> Result<serde_json::Value> {
    api_as_app_or_not_found(probe, jwt, args)?.ok_or_else(|| {
        let path = args.iter().find(|a| a.starts_with('/')).unwrap_or(&"");
        ApiError {
            not_found: true,
            message: format!("{path} が見つかりません (HTTP 404)"),
        }
        .into()
    })
}

/// 404のときは `None` を返す。
fn api_as_app_or_not_found(
    probe: &dyn Probe,
    jwt: &str,
    args: &[&str],
) -> Result<Option<serde_json::Value>> {
    let args: Vec<&str> = std::iter::once("api").chain(args.iter().copied()).collect();
    let gh_config = empty_gh_config()?;
    let out = probe
        .run_with_env("gh", &args, &gh_token_env(jwt, gh_config.path())?)
        .ok_or_else(|| {
            Error::new("ghが見つかりません").with_hint("`agentcells doctor` で確かめてください")
        })?;
    if !out.success {
        let err = ApiError::from_stderr(&out.stderr);
        return if err.not_found {
            Ok(None)
        } else {
            Err(err.into())
        };
    }
    serde_json::from_str(&out.stdout)
        .map(Some)
        .map_err(|e| Error::new(format!("GitHubの応答を読めません: {e}")))
}

/// `gh api` の失敗。404は「ない」として扱いたいので見分ける。
struct ApiError {
    not_found: bool,
    message: String,
}

impl ApiError {
    fn from_stderr(stderr: &str) -> Self {
        let message = stderr.lines().last().unwrap_or("").trim().to_string();
        Self {
            not_found: message.contains("HTTP 404"),
            message,
        }
    }
}

impl From<ApiError> for Error {
    fn from(e: ApiError) -> Self {
        let hint = if e.message.contains("HTTP 401") {
            "config.toml の [github_app] の id と秘密鍵が、同じGitHub Appのものか確かめてください"
        } else if e.message.contains("HTTP 422") {
            "GitHub Appの設定画面の Permissions で、Contents・Issues・Pull requests を Read and write にしてください"
        } else {
            "時間をおいてもう一度実行し、続くようならエラーの内容を確かめてください"
        };
        Error::new(format!(
            "GitHub Appとしての呼び出しに失敗しました: {}",
            e.message
        ))
        .with_hint(hint)
    }
}

/// GitHub Appの名前（URLに使われる slug）と、Appに付けた権限。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppInfo {
    pub slug: String,
    pub permissions: BTreeMap<String, String>,
}

impl AppInfo {
    /// `Purpose::Work` に要る権限のうち、Appに付いていないもの。
    pub fn missing_permissions(&self) -> Vec<String> {
        Purpose::Work
            .permissions()
            .iter()
            .filter(|(name, level)| {
                let has = self.permissions.get(*name).map(String::as_str);
                !(has == Some(*level) || (*level == "read" && has == Some("write")))
            })
            .map(|(name, level)| format!("{name}: {level}"))
            .collect()
    }
}

pub fn app_info(probe: &dyn Probe, jwt: &str) -> Result<AppInfo> {
    let app = api_as_app(probe, jwt, &["/app"])?;
    let slug = app["slug"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| Error::new("GitHubの応答にAppの名前がありません"))?;
    let permissions = app["permissions"]
        .as_object()
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default();
    Ok(AppInfo { slug, permissions })
}

/// リポジトリに入っているGitHub AppのインストールのID。入っていなければ `None`。
pub fn installation_id(probe: &dyn Probe, jwt: &str, repo: &RepoName) -> Result<Option<u64>> {
    let path = format!("/repos/{repo}/installation");
    let Some(installation) = api_as_app_or_not_found(probe, jwt, &[&path])? else {
        return Ok(None);
    };
    installation["id"]
        .as_u64()
        .map(Some)
        .ok_or_else(|| Error::new("GitHubの応答にインストールのIDがありません"))
}

/// そのリポジトリだけに使え、使い道に要る権限だけを持つトークンを作る（有効期限は1時間）。
fn installation_token(
    probe: &dyn Probe,
    jwt: &str,
    installation: u64,
    repo: &RepoName,
    purpose: Purpose,
) -> Result<String> {
    let path = format!("/app/installations/{installation}/access_tokens");
    let mut fields = vec![format!("repositories[]={}", repo.name)];
    fields.extend(
        purpose
            .permissions()
            .iter()
            .map(|(name, level)| format!("permissions[{name}]={level}")),
    );
    let mut args = vec!["-X", "POST", path.as_str()];
    for field in &fields {
        args.extend(["-f", field.as_str()]);
    }
    let token = api_as_app(probe, jwt, &args)?;
    token["token"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| Error::new("GitHubの応答にトークンがありません"))
}

/// どのアカウントで書き込むか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// GitHub App（そのリポジトリだけに使えるトークン）
    App,
    /// ghにログインしているアカウント
    Gh,
}

/// 書き込みに使う認証。どちらのアカウントでも、トークンを1つ持ち、ghとgitに環境変数で渡す。
///
/// ghのアカウントでも、ghの設定（`~/.config/gh`）はAIが書けるので、トークンを最初に1回だけ取り出し、
/// あとはAppと同じように、まっさらな設定のghとgitで使う。
pub struct Credentials {
    pub source: Source,
    token: String,
    gh_config: TempDir,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Credentials {{ source: {:?}, token: <hidden> }}",
            self.source
        )
    }
}

impl Credentials {
    pub fn new(source: Source, token: String) -> Result<Self> {
        Ok(Self {
            source,
            token,
            gh_config: empty_gh_config()?,
        })
    }
}

/// トークンを使わないgitにも付ける環境変数。使う人とシステムのgitの設定は読まない
/// （AIは `git config --global` も実行できるため）。パスワードの入力を求めて止まらないようにする。
pub const ISOLATED_GIT_ENV: &[(&str, &str)] = &[
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_TERMINAL_PROMPT", "0"),
];

impl Credentials {
    /// `gh` で書き込むときに足す環境変数。
    pub fn gh_env(&self) -> Result<Vec<(&'static str, &str)>> {
        gh_token_env(&self.token, self.gh_config.path())
    }

    /// トークンを使う `git push` に付ける引数。
    ///
    /// pushは、CLIがその場で作ったbareリポジトリから行う（`pr::push_branch`）。
    /// そのうえで、念のため次も付ける。
    /// - 使う人やリポジトリの設定にあるcredential helperを、空の helper で外す
    /// - helperはgithub.com宛てにだけ付ける。URLをほかのホストに向けられても、トークンを渡さない
    /// - hooks（pre-pushなど）を動かさない
    pub fn git_args(&self) -> Vec<String> {
        let helper = format!(
            "!f() {{ test \"$1\" = get || exit 0; echo username=x-access-token; echo \"password=${GIT_TOKEN_ENV}\"; }}; f"
        );
        [
            "-c".to_string(),
            "credential.helper=".to_string(),
            "-c".to_string(),
            format!("credential.https://{GITHUB_HOST}.helper={helper}"),
            "-c".to_string(),
            "core.hooksPath=/dev/null".to_string(),
        ]
        .to_vec()
    }

    /// トークンを使う `git push` で足す環境変数。
    ///
    /// `GIT_ALLOW_PROTOCOL=https` で、https以外の通信を止める。設定より優先されるので、
    /// `url.ext::<コマンド>.insteadOf` のような書き換えでコマンドを動かされない。
    pub fn git_env(&self) -> Vec<(&'static str, &str)> {
        let mut env = ISOLATED_GIT_ENV.to_vec();
        env.push(("GIT_ALLOW_PROTOCOL", "https"));
        env.push((GIT_TOKEN_ENV, &self.token));
        env
    }
}

/// リポジトリに書き込むための認証を用意する。
pub fn credentials(
    config: &Config,
    probe: &dyn Probe,
    repo: &RepoName,
    purpose: Purpose,
) -> Result<Credentials> {
    let Some(app) = &config.github_app else {
        return Credentials::new(Source::Gh, gh_account_token(probe)?);
    };
    let jwt = jwt(app, now()?)?;
    let installation = installation_id(probe, &jwt, repo)?.ok_or_else(|| {
        Error::new(format!(
            "GitHub Appが {repo} にインストールされていないか、{repo} が見つかりません"
        ))
        .with_hint("`agentcells doctor` でインストールのURLを確かめてください")
    })?;
    let token = installation_token(probe, &jwt, installation, repo, purpose)?;
    Credentials::new(Source::App, token)
}

/// ghにログインしているアカウントのトークンを取り出す。`gh auth token` は通信しないので、
/// ghの設定にある通信の設定（`http_unix_socket` など）の影響を受けない。
fn gh_account_token(probe: &dyn Probe) -> Result<String> {
    let out = probe
        .run("gh", &["auth", "token", "--hostname", GITHUB_HOST])
        .ok_or_else(|| {
            Error::new("ghが見つかりません").with_hint("`agentcells doctor` で確かめてください")
        })?;
    let token = out.stdout.trim();
    if !out.success || token.is_empty() {
        return Err(Error::new("ghのアカウントのトークンを取り出せません")
            .with_hint("`gh auth login` でgithub.comにログインしてください"));
    }
    Ok(token.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::fake::FakeProbe;
    use jsonwebtoken::{DecodingKey, Validation, decode};
    use std::path::{Path, PathBuf};

    const PUBLIC_KEY: &str = include_str!("../testdata/github-app-test-key.pub");

    fn app() -> GithubApp {
        GithubApp {
            id: "Iv23liTEST".to_string(),
            private_key: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("testdata/github-app-test-key.pem"),
        }
    }

    fn repo(s: &str) -> RepoName {
        RepoName::parse(s).unwrap()
    }

    #[derive(serde::Deserialize)]
    struct Decoded {
        iat: u64,
        exp: u64,
        iss: String,
    }

    #[test]
    fn jwt_is_signed_with_rs256_and_short_lived() {
        let now = now().unwrap();
        let token = jwt(&app(), now).unwrap();
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_required_spec_claims(&["exp", "iss"]);
        validation.set_issuer(&["Iv23liTEST"]);
        let key = DecodingKey::from_rsa_pem(PUBLIC_KEY.as_bytes()).unwrap();
        let claims = decode::<Decoded>(&token, &key, &validation).unwrap().claims;
        assert_eq!(claims.iss, "Iv23liTEST");
        assert_eq!(claims.iat, now - CLOCK_SKEW_SECS);
        // GitHubは10分より長いJWTを受け付けない
        assert!(claims.exp - claims.iat <= 10 * 60);
    }

    #[test]
    fn jwt_reports_a_broken_key_with_hint() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.pem");
        std::fs::write(&path, "not a key").unwrap();
        let mut app = app();
        app.private_key = path;
        let err = jwt(&app, now().unwrap()).unwrap_err();
        assert!(
            err.message.contains("秘密鍵として読めません"),
            "{}",
            err.message
        );
        assert!(err.hint.unwrap().contains(".pem"));
    }

    #[test]
    fn app_calls_pass_the_jwt_in_env_with_an_empty_gh_config() {
        let probe = FakeProbe::default().with(
            "gh api /app",
            true,
            r#"{"slug":"agentcells-bot","permissions":{"contents":"write","metadata":"read"}}"#,
        );
        let info = app_info(&probe, "JWT").unwrap();
        assert_eq!(info.slug, "agentcells-bot");
        assert_eq!(info.permissions["contents"], "write");
        assert!(!probe.calls.borrow()[0].contains("JWT"));

        let env: BTreeMap<String, String> = probe.envs.borrow()[0].iter().cloned().collect();
        assert_eq!(env["GH_TOKEN"], "JWT");
        assert_eq!(env["GH_HOST"], "github.com");
        // AIが書けるghの設定（~/.config/gh）は読ませない。使い終わった一時ディレクトリは消えている
        assert!(env["GH_CONFIG_DIR"].contains("agentcells-gh-"));
        assert!(!Path::new(&env["GH_CONFIG_DIR"]).exists());
    }

    #[test]
    fn missing_permissions_lists_what_the_work_needs() {
        let info = AppInfo {
            slug: "bot".to_string(),
            permissions: [
                ("contents", "write"),
                ("issues", "read"),
                ("metadata", "read"),
            ]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        };
        assert_eq!(
            info.missing_permissions(),
            vec!["issues: write", "pull_requests: write"]
        );
    }

    #[test]
    fn installation_is_none_when_not_installed() {
        let mut probe = FakeProbe::default();
        probe.outputs.insert(
            "gh api /repos/octo/app/installation".to_string(),
            crate::probe::Output {
                success: false,
                stdout: String::new(),
                stderr: "gh: Not Found (HTTP 404)\n".to_string(),
            },
        );
        assert_eq!(
            installation_id(&probe, "JWT", &repo("octo/app")).unwrap(),
            None
        );
    }

    #[test]
    fn bad_credentials_point_to_the_app_settings() {
        let mut probe = FakeProbe::default();
        probe.outputs.insert(
            "gh api /app".to_string(),
            crate::probe::Output {
                success: false,
                stdout: String::new(),
                stderr: "gh: A JSON web token could not be decoded (HTTP 401)\n".to_string(),
            },
        );
        let err = app_info(&probe, "JWT").unwrap_err();
        assert!(err.message.contains("HTTP 401"), "{}", err.message);
        assert!(err.hint.unwrap().contains("[github_app]"));
    }

    #[test]
    fn without_an_app_the_gh_token_is_used_with_an_empty_gh_config() {
        let probe =
            FakeProbe::default().with("gh auth token --hostname github.com", true, "gho_me\n");
        let creds =
            credentials(&Config::default(), &probe, &repo("octo/app"), Purpose::Work).unwrap();
        assert_eq!(creds.source, Source::Gh);
        // AIが書けるghの設定を読まないよう、Appのときと同じように渡す
        let env: BTreeMap<&str, &str> = creds.gh_env().unwrap().into_iter().collect();
        assert_eq!(env["GH_TOKEN"], "gho_me");
        assert!(Path::new(env["GH_CONFIG_DIR"]).is_dir());
        assert!(creds.git_env().contains(&(GIT_TOKEN_ENV, "gho_me")));
        assert!(!format!("{creds:?}").contains("gho_me"));
    }

    #[test]
    fn without_an_app_a_logged_out_gh_points_to_login() {
        let probe = FakeProbe::default().with("gh auth token --hostname github.com", false, "");
        let err =
            credentials(&Config::default(), &probe, &repo("octo/app"), Purpose::Work).unwrap_err();
        assert!(err.hint.unwrap().contains("gh auth login"));
    }

    #[test]
    fn credentials_with_an_app_are_scoped_to_the_repo() {
        let probe = FakeProbe::default()
            .with("gh api /repos/octo/app/installation", true, r#"{"id":42}"#)
            .with(
                "gh api -X POST /app/installations/42/access_tokens -f repositories[]=app \
                 -f permissions[contents]=write -f permissions[issues]=write \
                 -f permissions[pull_requests]=write",
                true,
                r#"{"token":"ghs_secret"}"#,
            );
        let config = Config {
            github_app: Some(app()),
            ..Config::default()
        };
        let creds = credentials(&config, &probe, &repo("octo/app"), Purpose::Work).unwrap();

        assert_eq!(creds.source, Source::App);
        let env: BTreeMap<&str, &str> = creds.gh_env().unwrap().into_iter().collect();
        assert_eq!(env["GH_TOKEN"], "ghs_secret");
        assert!(Path::new(env["GH_CONFIG_DIR"]).is_dir());
        assert!(creds.git_env().contains(&(GIT_TOKEN_ENV, "ghs_secret")));
        // トークンはgitの引数にも、Debugの表示にも出さない
        assert!(!creds.git_args().join(" ").contains("ghs_secret"));
        assert!(!format!("{creds:?}").contains("ghs_secret"));
        assert!(probe.calls.borrow().iter().all(|c| !c.contains("ghs_")));
    }

    #[test]
    fn label_tokens_only_get_issue_permissions() {
        let probe = FakeProbe::default()
            .with("gh api /repos/octo/app/installation", true, r#"{"id":42}"#)
            .with(
                "gh api -X POST /app/installations/42/access_tokens -f repositories[]=app \
                 -f permissions[issues]=write",
                true,
                r#"{"token":"ghs_labels"}"#,
            );
        let config = Config {
            github_app: Some(app()),
            ..Config::default()
        };
        credentials(&config, &probe, &repo("octo/app"), Purpose::Labels).unwrap();
    }

    #[test]
    fn credentials_fail_when_the_app_is_not_installed() {
        let mut probe = FakeProbe::default();
        probe.outputs.insert(
            "gh api /repos/octo/app/installation".to_string(),
            crate::probe::Output {
                success: false,
                stdout: String::new(),
                stderr: "gh: Not Found (HTTP 404)\n".to_string(),
            },
        );
        let config = Config {
            github_app: Some(app()),
            ..Config::default()
        };
        let err = credentials(&config, &probe, &repo("octo/app"), Purpose::Work).unwrap_err();
        assert!(err.message.contains("インストールされていないか"));
    }

    /// 本物のgitで、helperとhooksの扱いを確かめる。
    #[cfg(unix)]
    mod git {
        use super::*;
        use std::io::Write;
        use std::path::Path;
        use std::process::{Command, Output, Stdio};

        fn git(dir: &Path, args: &[&str]) {
            let out = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
        }

        /// AIが書き換えたかもしれない設定（別のhelperとhooksPath）を持つリポジトリを作る。
        fn tampered_repo() -> tempfile::TempDir {
            let dir = tempfile::tempdir().unwrap();
            git(dir.path(), &["init", "-q", "work"]);
            let work = dir.path().join("work");
            git(
                &work,
                &[
                    "config",
                    "credential.helper",
                    "!f() { echo password=from-repo-config; }; f",
                ],
            );
            let hooks = dir.path().join("hooks");
            std::fs::create_dir(&hooks).unwrap();
            let hook = hooks.join("pre-push");
            let marker = dir.path().join("hook-ran");
            std::fs::write(
                &hook,
                format!(
                    "#!/bin/sh\necho \"$AGENTCELLS_GITHUB_TOKEN\" > '{}'\n",
                    marker.display()
                ),
            )
            .unwrap();
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            git(
                &work,
                &["config", "core.hooksPath", hooks.to_str().unwrap()],
            );
            dir
        }

        fn fill(work: &Path, creds: &Credentials, host: &str) -> Output {
            let mut child = Command::new("git")
                .arg("-C")
                .arg(work)
                .args(creds.git_args())
                .args(["credential", "fill"])
                .envs(creds.git_env())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            write!(
                child.stdin.take().unwrap(),
                "protocol=https\nhost={host}\n\n"
            )
            .unwrap();
            child.wait_with_output().unwrap()
        }

        fn token() -> Credentials {
            Credentials::new(Source::App, "ghs_test".to_string()).unwrap()
        }

        #[test]
        fn helper_gives_the_token_to_github_only() {
            let dir = tampered_repo();
            let work = dir.path().join("work");

            let out = fill(&work, &token(), "github.com");
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(out.status.success(), "{out:?}");
            assert!(stdout.contains("username=x-access-token"), "{stdout}");
            assert!(stdout.contains("password=ghs_test"), "{stdout}");
            assert!(!stdout.contains("from-repo-config"), "{stdout}");

            // remoteのURLやinsteadOfでほかのホストに向けられても、トークンは渡さない
            let out = fill(&work, &token(), "evil.example.com");
            assert!(!out.status.success(), "{out:?}");
            assert!(!String::from_utf8_lossy(&out.stdout).contains("ghs_test"));
        }

        #[test]
        fn push_does_not_run_hooks() {
            let dir = tampered_repo();
            let work = dir.path().join("work");
            git(dir.path(), &["init", "-q", "--bare", "remote.git"]);
            git(&work, &["commit", "-q", "--allow-empty", "-m", "x"]);

            let creds = token();
            let out = Command::new("git")
                .arg("-C")
                .arg(&work)
                .args(creds.git_args())
                .args(["push", "-q", "../remote.git", "HEAD:refs/heads/ac/test"])
                .envs(creds.git_env())
                // ローカルのパスにpushして確かめるため、ここだけfileを許す
                .env("GIT_ALLOW_PROTOCOL", "file")
                .output()
                .unwrap();
            assert!(out.status.success(), "{out:?}");
            assert!(!dir.path().join("hook-ran").exists());
        }
    }
}
