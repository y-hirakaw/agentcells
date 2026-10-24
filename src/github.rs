//! GitHubに書き込むときの認証。
//!
//! GitHub Appを推奨する。Appのトークンで書き込むと、コメントやPRが「<Appの名前>[bot]」になり、
//! 人間の発言と見分けられる。Appを設定していなければ、ghにログインしているアカウントで書き込む。
//!
//! トークンは引数に書かず、環境変数で渡す（`ps` で見えないように）。AIの実行には渡さない。

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde::Serialize;

use crate::config::{Config, GithubApp};
use crate::error::{Error, Result};
use crate::home::RepoName;
use crate::probe::Probe;

/// git の credential helper がトークンを読む環境変数。
pub const GIT_TOKEN_ENV: &str = "AGENTCELLS_GITHUB_TOKEN";

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

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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

/// GitHub Appとして `gh api` を呼ぶ。JWTは環境変数で渡す（GitHubは `token` 方式で送ったJWTも受け付ける）。
fn api_as_app(probe: &dyn Probe, jwt: &str, args: &[&str]) -> Result<serde_json::Value> {
    api_as_app_or_not_found(probe, jwt, args)?.ok_or_else(|| {
        ApiError {
            not_found: true,
            message: format!("{} が見つかりません (HTTP 404)", args.join(" ")),
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
    let out = probe
        .run_with_env("gh", &args, &[("GH_TOKEN", jwt)])
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

/// GitHub Appの名前（URLに使われる slug）。
pub fn app_slug(probe: &dyn Probe, jwt: &str) -> Result<String> {
    let app = api_as_app(probe, jwt, &["/app"])?;
    app["slug"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| Error::new("GitHubの応答にAppの名前がありません"))
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

/// そのリポジトリだけに使えるトークンを作る（有効期限は1時間）。
fn installation_token(
    probe: &dyn Probe,
    jwt: &str,
    installation: u64,
    repo: &RepoName,
) -> Result<String> {
    let path = format!("/app/installations/{installation}/access_tokens");
    let repository = format!("repositories[]={}", repo.name);
    let token = api_as_app(probe, jwt, &["-X", "POST", &path, "-f", &repository])?;
    token["token"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| Error::new("GitHubの応答にトークンがありません"))
}

/// 書き込みに使う認証。
pub enum Credentials {
    /// GitHub Appのトークン（そのリポジトリだけに使える）
    App { token: String },
    /// ghにログインしているアカウント
    Gh,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Credentials::App { .. } => f.write_str("Credentials::App { token: <hidden> }"),
            Credentials::Gh => f.write_str("Credentials::Gh"),
        }
    }
}

impl Credentials {
    /// `gh` で書き込むときに足す環境変数。
    pub fn gh_env(&self) -> Vec<(&'static str, &str)> {
        match self {
            Credentials::App { token } => vec![("GH_TOKEN", token)],
            Credentials::Gh => Vec::new(),
        }
    }

    /// `git push` などに付ける引数。使う人のgitの設定（credential helper）に頼らず、決まった認証を使う。
    pub fn git_args(&self) -> Vec<String> {
        let helper = match self {
            Credentials::App { .. } => format!(
                "credential.helper=!f() {{ echo username=x-access-token; echo \"password=${GIT_TOKEN_ENV}\"; }}; f"
            ),
            Credentials::Gh => "credential.helper=!gh auth git-credential".to_string(),
        };
        // 空の helper で、使う人の設定にある helper を外す
        ["-c", "credential.helper=", "-c", &helper]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    /// `git push` などで足す環境変数。
    pub fn git_env(&self) -> Vec<(&'static str, &str)> {
        match self {
            Credentials::App { token } => vec![(GIT_TOKEN_ENV, token)],
            Credentials::Gh => Vec::new(),
        }
    }
}

/// リポジトリに書き込むための認証を用意する。
pub fn credentials(config: &Config, probe: &dyn Probe, repo: &RepoName) -> Result<Credentials> {
    let Some(app) = &config.github_app else {
        return Ok(Credentials::Gh);
    };
    let jwt = jwt(app, now())?;
    let installation = installation_id(probe, &jwt, repo)?.ok_or_else(|| {
        Error::new(format!("GitHub Appが {repo} にインストールされていません"))
            .with_hint("`agentcells doctor` でインストールのURLを確かめてください")
    })?;
    let token = installation_token(probe, &jwt, installation, repo)?;
    Ok(Credentials::App { token })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::fake::FakeProbe;
    use jsonwebtoken::{DecodingKey, Validation, decode};
    use std::path::PathBuf;

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
        let now = now();
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
        let err = jwt(&app, now()).unwrap_err();
        assert!(
            err.message.contains("秘密鍵として読めません"),
            "{}",
            err.message
        );
        assert!(err.hint.unwrap().contains(".pem"));
    }

    #[test]
    fn app_calls_pass_the_jwt_in_env_not_args() {
        let probe = FakeProbe::default().with("gh api /app", true, r#"{"slug":"agentcells-bot"}"#);
        assert_eq!(app_slug(&probe, "JWT").unwrap(), "agentcells-bot");
        assert!(!probe.calls.borrow()[0].contains("JWT"));
        assert_eq!(
            probe.envs.borrow()[0],
            vec![("GH_TOKEN".to_string(), "JWT".to_string())]
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
        let err = app_slug(&probe, "JWT").unwrap_err();
        assert!(err.message.contains("HTTP 401"), "{}", err.message);
        assert!(err.hint.unwrap().contains("[github_app]"));
    }

    #[test]
    fn credentials_use_gh_without_an_app() {
        let probe = FakeProbe::default();
        let creds = credentials(&Config::default(), &probe, &repo("octo/app")).unwrap();
        assert!(matches!(creds, Credentials::Gh));
        assert!(creds.gh_env().is_empty());
        assert!(
            creds
                .git_args()
                .join(" ")
                .contains("!gh auth git-credential")
        );
        assert!(probe.calls.borrow().is_empty());
    }

    #[test]
    fn credentials_with_an_app_are_scoped_to_the_repo() {
        let probe = FakeProbe::default()
            .with("gh api /repos/octo/app/installation", true, r#"{"id":42}"#)
            .with(
                "gh api -X POST /app/installations/42/access_tokens -f repositories[]=app",
                true,
                r#"{"token":"ghs_secret"}"#,
            );
        let config = Config {
            github_app: Some(app()),
            ..Config::default()
        };
        let creds = credentials(&config, &probe, &repo("octo/app")).unwrap();

        assert_eq!(creds.gh_env(), vec![("GH_TOKEN", "ghs_secret")]);
        assert_eq!(creds.git_env(), vec![(GIT_TOKEN_ENV, "ghs_secret")]);
        // トークンはgitの引数にも、Debugの表示にも出さない
        assert!(!creds.git_args().join(" ").contains("ghs_secret"));
        assert!(!format!("{creds:?}").contains("ghs_secret"));
        assert!(probe.calls.borrow().iter().all(|c| !c.contains("ghs_")));
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
        let err = credentials(&config, &probe, &repo("octo/app")).unwrap_err();
        assert!(err.message.contains("インストールされていません"));
    }
}
