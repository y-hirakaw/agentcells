//! Issue/PRへの書き込み（ラベル、コメント、レビュー）。

use serde::Deserialize;

use crate::config::Target;
use crate::error::{Error, Result};
use crate::github::Credentials;
use crate::home::RepoName;
use crate::probe::Probe;

/// GitHubのIssue/PRへの読み書き。トークンは、そのたびに渡す（作る時を、AIが動いていないときに限るため）。
pub(super) struct Issue<'a> {
    pub(super) probe: &'a dyn Probe,
    pub(super) repo: &'a RepoName,
    pub(super) number: u64,
    pub(super) kind: Target,
}

impl Issue<'_> {
    /// `gh issue` か `gh pr`。
    fn sub(&self) -> &'static str {
        match self.kind {
            Target::Issue => "issue",
            Target::PullRequest => "pr",
        }
    }

    /// PRのレビューを投稿する（`POST /repos/{owner}/{repo}/pulls/{number}/reviews`）。
    pub(super) fn review(
        &self,
        credentials: &Credentials,
        payload: &serde_json::Value,
    ) -> Result<()> {
        let path = format!("repos/{}/pulls/{}/reviews", self.repo, self.number);
        self.gh(
            credentials,
            &["api", "-X", "POST", &path, "--input", "-", "--silent"],
            Some(&payload.to_string()),
        )
        .map(|_| ())
    }

    pub(super) fn gh(
        &self,
        credentials: &Credentials,
        args: &[&str],
        input: Option<&str>,
    ) -> Result<String> {
        let env = credentials.gh_env()?;
        let out = self
            .probe
            .run_with_input("gh", args, &env, input)
            .ok_or_else(|| {
                Error::new("ghが見つかりません").with_hint("`agentcells doctor` で確かめてください")
            })?;
        if !out.success {
            return Err(Error::new(format!(
                "gh {} {} が失敗しました: {}",
                args.first().unwrap_or(&""),
                args.get(1).unwrap_or(&""),
                out.stderr.trim()
            )));
        }
        Ok(out.stdout)
    }

    pub(super) fn edit_labels(
        &self,
        credentials: &Credentials,
        add: &[&str],
        remove: &[&str],
    ) -> Result<()> {
        if add.is_empty() && remove.is_empty() {
            return Ok(());
        }
        let repo = self.repo.to_string();
        let number = self.number.to_string();
        let add = add.join(",");
        let remove = remove.join(",");
        let mut args = vec![self.sub(), "edit", number.as_str(), "--repo", repo.as_str()];
        if !add.is_empty() {
            args.extend(["--add-label", add.as_str()]);
        }
        if !remove.is_empty() {
            args.extend(["--remove-label", remove.as_str()]);
        }
        self.gh(credentials, &args, None).map(|_| ())
    }

    pub(super) fn comment(&self, credentials: &Credentials, body: &str) -> Result<()> {
        let repo = self.repo.to_string();
        let number = self.number.to_string();
        self.gh(
            credentials,
            &[
                self.sub(),
                "comment",
                number.as_str(),
                "--repo",
                repo.as_str(),
                "--body-file",
                "-",
            ],
            Some(body),
        )
        .map(|_| ())
    }

    pub(super) fn close(&self, credentials: &Credentials) -> Result<()> {
        let repo = self.repo.to_string();
        let number = self.number.to_string();
        self.gh(
            credentials,
            &["issue", "close", number.as_str(), "--repo", repo.as_str()],
            None,
        )
        .map(|_| ())
    }

    pub(super) fn labels(&self, credentials: &Credentials) -> Result<Vec<String>> {
        let repo = self.repo.to_string();
        let number = self.number.to_string();
        let out = self.gh(
            credentials,
            &[
                self.sub(),
                "view",
                number.as_str(),
                "--repo",
                repo.as_str(),
                "--json",
                "labels",
            ],
            None,
        )?;
        #[derive(Deserialize)]
        struct Labels {
            labels: Vec<Named>,
        }
        #[derive(Deserialize)]
        struct Named {
            name: String,
        }
        let labels: Labels = serde_json::from_str(&out)
            .map_err(|e| Error::new(format!("ラベルを読めません: {e}")))?;
        Ok(labels.labels.into_iter().map(|l| l.name).collect())
    }
}

/// 外すラベルのうち、付いているもので、付けるものに入っていないもの（大文字・小文字を区別しない）。
pub(super) fn present<'a>(labels: &[String], remove: &[&'a str], add: &[&str]) -> Vec<&'a str> {
    let mut out: Vec<&'a str> = Vec::new();
    for r in remove {
        if labels.iter().any(|l| l.eq_ignore_ascii_case(r))
            && !add.iter().any(|a| a.eq_ignore_ascii_case(r))
            && !out.iter().any(|o| o.eq_ignore_ascii_case(r))
        {
            out.push(r);
        }
    }
    out
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn only_present_labels_are_removed() {
        let labels = vec!["ac:Working".to_string(), "ac:needs-human".to_string()];
        assert_eq!(
            present(
                &labels,
                &["ac:triage", "ac:working", "ac:needs-human", "ac:in-review"],
                &["ac:needs-human"]
            ),
            vec!["ac:working"]
        );
    }
}
