//! `claude -p --settings` に渡す、cellとして動くエージェントの設定。

use std::path::{Path, PathBuf};

use serde_json::json;

use crate::config::{Config, Product};
use crate::error::{Error, Result};
use crate::github::ISOLATED_GIT_ENV;
use crate::home::Home;

/// AIにIssue/PRのラベルや状態を変えさせないためのdeny。ラベルの付け替えはCLIだけがする。
/// コメントとレビューもCLIが目印（`[agentcells] <cell> round <n>`）を付けて投稿するので、AIには書かせない。
/// ブランチのpushとPRの作成もCLIがするので、AIはworktreeの中でコミットするところまでにする。
/// `gh api` は何でもできてしまうので、まとめて止める。
/// `gh auth` は、ghのアカウントで動くときに `gh auth token` でトークンを取り出せるので止める。
pub const DENY_GH: &[&str] = &[
    "Bash(gh auth:*)",
    "Bash(git push:*)",
    "Bash(gh pr create:*)",
    "Bash(gh issue comment:*)",
    "Bash(gh pr comment:*)",
    "Bash(gh pr review:*)",
    "Bash(gh issue edit:*)",
    "Bash(gh issue close:*)",
    "Bash(gh issue reopen:*)",
    "Bash(gh issue delete:*)",
    "Bash(gh pr edit:*)",
    "Bash(gh pr close:*)",
    "Bash(gh pr reopen:*)",
    "Bash(gh pr merge:*)",
    "Bash(gh label:*)",
    "Bash(gh api:*)",
];

/// AIに許すツール。読むのはその仕事のworktreeの中だけにし、書き換えは次の2つの道だけにする。
/// - ファイルの作成と更新: Edit・Writeを、その仕事のworktreeの中だけに許す
/// - それ以外（削除、移動、コミット、gitを読む、ビルドやテスト）: `agentcells cell` を通す。CLIが範囲を確かめる
///
/// Bashは `agentcells cell` だけを許す。`ls` や `git diff` のようなコマンドでも、オプションでファイルに
/// 書けるものがある（`git diff --output` など）ので許さない。Claude Codeは、許したコマンドに
/// `>`、`;`、`&&`、`$(...)` で別の操作を付け足すことを止める（2.1.310で確かめた）。
pub const ALLOW_BASH: &str = "Bash(agentcells cell:*)";

/// 使う人のホームの下で、秘密がよく置かれる場所。ホーム全体を読ませないうえで、
/// productの `sandbox.read` に、これらを含む場所（`~/.config` など）が書かれても読ませない。
pub const SECRET_PLACES: &[&str] = &[
    ".ssh",
    ".aws",
    ".gnupg",
    ".netrc",
    ".git-credentials",
    ".docker",
    ".kube",
    ".npmrc",
    ".pypirc",
    ".config/gcloud",
    ".config/hub",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".android/adbkey",
    ".gradle/gradle.properties",
    ".m2/settings.xml",
    ".m2/settings-security.xml",
    ".gem/credentials",
    "Library/Keychains",
];

/// 1回の実行の場所。
///
/// 作業の場所は、仕事ごとに `git clone --shared --separate-git-dir=<git_dir>` で作ったcloneにする。
/// - linked worktree（`git worktree add`）だと、コミットのために元のリポジトリの `.git` を書けるようにする必要があり、
///   AIのビルドがその `.git/config` やhooksを書き換えると、あとでCLIがsandboxの外で動かすgitで任意のコードが動く
/// - sandboxは、作業ディレクトリの直下の `.git` には、許しても書かせない（2.1.310で確かめた）。
///   そのため `.git` の実体を作業ディレクトリの外（仕事ごとの使い捨ての場所）に置き、そこを書けるようにする
pub struct RunPaths<'a> {
    /// 作業するclone
    pub worktree: &'a Path,
    /// このcloneの `.git` の実体（仕事ごと）。コミットのためにsandboxから書ける
    pub git_dir: &'a Path,
    /// `--shared` で共有している、元のリポジトリのオブジェクト（`.git/objects`）。読むだけ
    pub shared_objects: &'a Path,
    /// 今の仕事のファイル（`AGENTCELLS_CONTEXT`）。`agentcells cell` が読む
    pub context: &'a Path,
    /// worktreeに置いた秘密のファイル（絶対パス）
    pub placed: &'a [PathBuf],
    /// sandboxの中で動かすプログラム（agentcellsとbetterleaks）。ホームの下にあっても動かせるようにする
    pub programs: &'a [PathBuf],
    /// ビルドやテストの一時ディレクトリ（仕事ごと）。`agentcells cell run` が `TMPDIR` で渡す
    pub tmp: &'a Path,
    /// productのキャッシュ（`caches/<product>/<ツール>`）。`agentcells cell run` が環境変数でツールを向ける
    pub caches: &'a [PathBuf],
}

/// 1回の実行で渡す設定を作る。2つの守りを重ねる。
///
/// Claude Codeのツールの権限:
/// - 読む（Read）のは、worktreeの中だけ
/// - ファイルの作成と更新（Edit、Write）は、worktreeの中だけ。`.git` と置いた秘密のファイルは除く
/// - Bashは `agentcells cell` だけ。WebFetchは使わせない
///
/// sandbox（Bashで動くもの。`agentcells cell run` のビルドやテストも含む）:
/// - 書けるのは、作業するcloneと、その `.git` の実体（仕事ごとの使い捨て）だけ。元のリポジトリには書けない
/// - 使う人のホームの下は読ませない。読めるのは、今の仕事のclone、共有しているオブジェクト、今の仕事のファイル、
///   動かすプログラム、productの `sandbox.read` に書いた場所だけ
/// - `~/.agentcells`（App の秘密鍵、`files/`、`config.toml`、ほかの仕事のclone）、ghの設定、秘密がよく置かれる場所、
///   置いた秘密のファイル（`sandbox.read_placed` に書いたものを除く）は、上で開けた場所の中にあっても読ませない
///   （sandboxは、より細かく書いた決まりを優先する。2.1.310で確かめた）
/// - 外への通信は、productの `sandbox.network` に書いた先だけ。ほかの先は、聞かずに止める（`strictAllowlist`）
/// - sandboxの外でコマンドを動かすことは許さない
///
/// 信頼していないディレクトリではprojectの設定のallowが無視されるので、cellに要るallowもここで渡す。
pub fn runtime_settings(
    home: &Home,
    config: &Config,
    product: &Product,
    run: &RunPaths,
) -> Result<serde_json::Value> {
    let problems = sandbox_problems(home, config);
    if !problems.is_empty() {
        return Err(Error::new(problems.join("\n"))
            .with_hint("config.toml の [product.sandbox] を直してください"));
    }
    let worktree = run.worktree;
    let mut allow = vec![ALLOW_BASH.to_string()];
    // Edit の決まりは、Write を含むファイルを書き換えるツールすべてに効く（Write の決まりは使われない。2.1.310で確かめた）
    for tool in ["Read", "Edit"] {
        rule(&mut allow, tool, worktree, "/**")?;
    }

    let mut deny: Vec<String> = DENY_GH.iter().map(|s| s.to_string()).collect();
    // 外へ送れる道になるので、sandboxを通らないWebFetchは使わせない
    deny.push("WebFetch".to_string());
    // .git（linked worktreeではリポジトリを指すファイル）を書き換えさせない
    rule(&mut deny, "Edit", &worktree.join(".git"), "")?;
    rule(&mut deny, "Edit", &worktree.join(".git"), "/**")?;
    // Claude Codeの設定（hooksなど）を書かせない。agentcellsはリポジトリの設定を読まないが、
    // 人がそのブランチをClaude Codeで開いたときに、AIが書いたhooksが動かないように
    rule(&mut deny, "Edit", &worktree.join(".claude"), "/**")?;
    for path in run.placed {
        rule(&mut deny, "Edit", path, "")?;
        // Readのdenyはsandboxの読めない場所にも足される（2.1.310で確かめた）ので、
        // ビルドに読ませるファイルは、AIのReadからも読める（ビルドを通せば読めるのと同じ）
        if !reads_placed(product, worktree, path) {
            rule(&mut deny, "Read", path, "")?;
        }
    }

    let mut deny_read = forms(&user_home()?)?;
    for place in protected_places(home, config)? {
        deny_read.extend(forms(&place)?);
    }
    for path in run.placed {
        if !reads_placed(product, worktree, path) {
            deny_read.extend(forms(path)?);
        }
    }
    // Claude Codeは、sandboxの中のシェルに `/tmp/claude-<uid>` を使わせる。そこは使う人のほかのセッションと共有だが、
    // 読み書きを止めると、Claude Code自身がコマンドの出力やcwdを扱えず、すべてのコマンドが失敗する（2.1.310で確かめた）。
    // 止められないので、ビルドやテストには、仕事ごとの一時ディレクトリを渡すだけにする（docs/design.mdの残る危険）
    let mut allow_read = forms(worktree)?;
    allow_read.extend(forms(run.git_dir)?);
    allow_read.extend(forms(run.shared_objects)?);
    allow_read.extend(forms(run.context)?);
    allow_read.extend(forms(run.tmp)?);
    for program in run.programs {
        allow_read.extend(forms(program)?);
    }
    let mut allow_write = [forms(run.git_dir)?, forms(run.tmp)?].concat();
    for cache in run.caches {
        allow_read.extend(forms(cache)?);
        allow_write.extend(forms(cache)?);
    }
    for (key, path) in product.sandbox.opened() {
        allow_read.extend(forms(path)?);
        if key == "write" {
            allow_write.extend(forms(path)?);
        }
    }
    // 書き込みは、denyWriteがallowWriteより細かくなくても勝つ（2.1.310で確かめた）。`~/.agentcells` を入れると、
    // その中のこの仕事の .git と一時ディレクトリにも書けなくなるので入れない。`write` には、それを含む場所を書かせない
    let mut deny_write = Vec::new();
    for place in protected_places(home, config)?
        .into_iter()
        .filter(|p| p != home.root())
        .chain(write_protected_places()?)
    {
        deny_write.extend(forms(&place)?);
    }

    // sandboxの中では ~/.gitconfig が読めず、gitが止まる。使う人とシステムのgitの設定は読ませない
    let env: serde_json::Map<String, serde_json::Value> = ISOLATED_GIT_ENV
        .iter()
        .map(|(k, v)| (k.to_string(), json!(v)))
        .collect();

    Ok(json!({
        // hooksはsandboxの外で動くので、どこから来たものも動かさない
        "disableAllHooks": true,
        "env": env,
        "permissions": { "allow": allow, "deny": deny },
        "sandbox": {
            "enabled": true,
            "allowUnsandboxedCommands": false,
            "filesystem": {
                "allowWrite": dedup(allow_write),
                "denyWrite": dedup(deny_write),
                "denyRead": dedup(deny_read),
                "allowRead": dedup(allow_read),
            },
            "network": {
                "allowedDomains": product.sandbox.network,
                "strictAllowlist": true,
                "allowLocalBinding": product.sandbox.bind_localhost,
            }
        }
    }))
}

/// productの `sandbox.read` と `sandbox.write` の問題。読ませない場所や書かせない場所の中を開けると、
/// sandboxはそちらを優先して開けてしまうので断る。
pub fn sandbox_problems(home: &Home, config: &Config) -> Vec<String> {
    let (places, write_places) = match (protected_places(home, config), write_protected_places()) {
        (Ok(places), Ok(write_places)) => (places, write_places),
        (Err(e), _) | (_, Err(e)) => return vec![e.message],
    };
    let mut problems = Vec::new();
    for product in &config.products {
        for (key, path) in product.sandbox.opened() {
            let Ok(written) = path_forms(path) else {
                continue;
            };
            let guarded: Vec<&PathBuf> = if key == "write" {
                places.iter().chain(&write_places).collect()
            } else {
                places.iter().collect()
            };
            let place = guarded.into_iter().find(|place| {
                let Ok(place_forms) = path_forms(place) else {
                    return false;
                };
                written
                    .iter()
                    .any(|p| place_forms.iter().any(|q| starts_with_ignore_case(p, q)))
            });
            if let Some(place) = place {
                problems.push(format!(
                    "product「{}」の sandbox.{key} の {} は、開けられない場所（{}）の中なので書けません",
                    product.name,
                    path.display(),
                    place.display()
                ));
            } else if key == "write"
                && wide_places(&user_home_or_root())
                    .iter()
                    .any(|w| same_path(w, path))
            {
                problems.push(format!(
                    "product「{}」の sandbox.write の {} は広すぎます。書かせない場所を並べきれないので、書けません。要る場所だけを書いてください（キャッシュなら caches を使います）",
                    product.name,
                    path.display()
                ));
            } else if key == "write" && contains(&written, home.root()) {
                problems.push(format!(
                    "product「{}」の sandbox.write の {} は、agentcellsの置き場所（{}）を含むので書けません",
                    product.name,
                    path.display(),
                    home.root().display()
                ));
            }
        }
    }
    problems
}

/// 書けるが、勧めない `[product.sandbox]` の書き方。`doctor` が知らせる。
pub fn sandbox_warnings(config: &Config) -> Vec<String> {
    let Ok(user_home) = user_home() else {
        return Vec::new();
    };
    let wide = wide_places(&user_home);
    let mut warnings = Vec::new();
    for product in &config.products {
        for path in &product.sandbox.read {
            if wide.iter().any(|w| same_path(w, path)) {
                warnings.push(format!(
                    "product「{}」の sandbox.read の {} は広すぎます。並べ忘れた場所の秘密も読めるようになります。要る場所だけを書いてください",
                    product.name,
                    path.display()
                ));
            }
        }
        for path in &product.sandbox.write {
            warnings.push(format!(
                "product「{}」の sandbox.write の {} には、AIが書き換えたビルドがコードを置けます。手元（sandboxの外）で動かすツールがそれを実行しうるので、キャッシュなら caches を使ってください",
                product.name,
                path.display()
            ));
        }
        if product.sandbox.bind_localhost {
            warnings.push(format!(
                "product「{}」の sandbox.bind_localhost を有効にすると、手元で動いているほかのサーバー（プロキシ、DockerのAPI、DBなど）にもつながります。そこを通して外へ送ったり、sandboxの外でコードを動かしたりできることがあります",
                product.name
            ));
        }
        for domain in &product.sandbox.network {
            if domain.starts_with("*.") {
                warnings.push(format!(
                    "product「{}」の sandbox.network の「{domain}」はワイルドカードです。誰でも書ける置き場所（クラウドのストレージなど）を含むと、外へ送る道になります。取得元のホストを具体的に書いてください",
                    product.name
                ));
            }
        }
    }
    warnings
}

/// 同じ場所か。大文字・小文字は問わない。
fn same_path(a: &Path, b: &Path) -> bool {
    let a = a.to_string_lossy().to_lowercase();
    let b = b.to_string_lossy().to_lowercase();
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

/// `write` で広い場所を開けても、書かせない場所。書かれると、使う人が手元で動かすClaude Codeや、
/// ログインのたびに動くものに、AIが書いたものが効いてしまう。
pub const WRITE_PROTECTED_PLACES: &[&str] = &[
    ".claude",
    ".claude.json",
    ".zshrc",
    ".zshenv",
    ".zprofile",
    ".zlogin",
    ".bashrc",
    ".bash_profile",
    ".bash_login",
    ".profile",
    ".gitconfig",
    ".config/git",
    ".local/bin",
    "bin",
    "Library/LaunchAgents",
];

fn write_protected_places() -> Result<Vec<PathBuf>> {
    let user_home = user_home()?;
    Ok(WRITE_PROTECTED_PLACES
        .iter()
        .map(|p| user_home.join(p))
        .collect())
}

/// 開けると、並べる形の意味がなくなる広い場所。
fn wide_places(user_home: &Path) -> Vec<PathBuf> {
    [
        "",
        ".config",
        ".local",
        "Library",
        "Documents",
        "Desktop",
        "Downloads",
    ]
    .iter()
    .map(|p| user_home.join(p))
    .collect()
}

fn user_home_or_root() -> PathBuf {
    user_home().unwrap_or_else(|_| PathBuf::from("/"))
}

/// 開けた場所の中にあっても読ませない場所。
fn protected_places(home: &Home, config: &Config) -> Result<Vec<PathBuf>> {
    let mut places = vec![home.root().to_path_buf()];
    if let Some(app) = &config.github_app {
        places.push(app.private_key.clone());
    }
    if let Some(dir) = gh_config_dir(|key| std::env::var_os(key)) {
        places.push(dir);
    }
    let user_home = user_home()?;
    for place in SECRET_PLACES {
        places.push(user_home.join(place));
    }
    Ok(places)
}

fn user_home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .ok_or_else(|| {
            Error::new("ホームディレクトリがわかりません")
                .with_hint("環境変数 HOME を設定してください")
        })
}

/// 置いたファイルを、productの `sandbox.read_placed` でビルドに読ませるか。
/// macOSの標準のファイルシステムは大文字・小文字を区別しないので、macOSでは大文字・小文字を問わず比べる。
/// 区別するファイルシステムで問わずに比べると、書いたものとは別のファイルまで開けてしまう。
fn reads_placed(product: &Product, worktree: &Path, placed: &Path) -> bool {
    let Ok(relative) = placed.strip_prefix(worktree) else {
        return false;
    };
    let fold = |p: &Path| {
        let s = p.to_string_lossy().into_owned();
        if cfg!(target_os = "macos") {
            s.to_lowercase()
        } else {
            s
        }
    };
    let relative = fold(relative);
    product
        .sandbox
        .read_placed
        .iter()
        .any(|p| fold(p) == relative)
}

/// `paths` のどれかが、`inner` を含むか（`inner` がその中か、同じか）。
fn contains(paths: &[PathBuf], inner: &Path) -> bool {
    let Ok(inner_forms) = path_forms(inner) else {
        return false;
    };
    paths
        .iter()
        .any(|p| inner_forms.iter().any(|i| starts_with_ignore_case(i, p)))
}

/// `path` が `base` か、その中か。大文字・小文字は問わない（macOSの標準のファイルシステムでは同じ場所を指す）。
fn starts_with_ignore_case(path: &Path, base: &Path) -> bool {
    let path = path.to_string_lossy().to_lowercase();
    let base = base.to_string_lossy().to_lowercase();
    let base = base.trim_end_matches('/');
    path == base || path.starts_with(&format!("{base}/"))
}

fn dedup(mut v: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    v.retain(|x| seen.insert(x.clone()));
    v
}

/// ghの設定ディレクトリ。ghと同じく `GH_CONFIG_DIR`、`XDG_CONFIG_HOME/gh`、`~/.config/gh` の順に探す。
fn gh_config_dir(var: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    let absolute = |p: PathBuf| p.is_absolute().then_some(p);
    if let Some(dir) = var("GH_CONFIG_DIR").map(PathBuf::from) {
        return absolute(dir);
    }
    if let Some(dir) = var("XDG_CONFIG_HOME").map(PathBuf::from) {
        return absolute(dir.join("gh"));
    }
    var("HOME")
        .map(PathBuf::from)
        .and_then(|home| absolute(home.join(".config/gh")))
}

/// `Read(//絶対パス)` の形で、ツールとパスの決まりを足す（先頭の / を重ねる）。
fn rule(rules: &mut Vec<String>, tool: &str, path: &Path, suffix: &str) -> Result<()> {
    for p in path_forms(path)? {
        let rule = format!("{tool}(/{}{suffix})", p.display());
        if !rules.contains(&rule) {
            rules.push(rule);
        }
    }
    Ok(())
}

/// sandboxに書くパス。
fn forms(path: &Path) -> Result<Vec<String>> {
    Ok(path_forms(path)?
        .iter()
        .map(|p| p.display().to_string())
        .collect())
}

/// Claude Codeが比べるパスと形が違うと効かないので、書いたままのパスと、
/// シンボリックリンクや `..` を解いたパス（macOSの /var → /private/var など）の両方を返す。
fn path_forms(path: &Path) -> Result<Vec<PathBuf>> {
    if !path.is_absolute() {
        return Err(Error::new(format!(
            "許可や禁止に書くパスは絶対パスにしてください: {}",
            path.display()
        )));
    }
    let mut paths = vec![path.to_path_buf()];
    if let Some(resolved) = resolve(path)
        && resolved != path
    {
        paths.push(resolved);
    }
    Ok(paths)
}

/// シンボリックリンクと `..` を解く。まだないファイルは、親ディレクトリを解いてつなぐ。
fn resolve(path: &Path) -> Option<PathBuf> {
    if let Ok(p) = path.canonicalize() {
        return Some(p);
    }
    let parent = path.parent()?.canonicalize().ok()?;
    Some(parent.join(path.file_name()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{GithubApp, Sandbox};
    use crate::home::RepoName;

    fn home() -> Home {
        Home::new(PathBuf::from("/home/me/.agentcells"))
    }

    fn product(sandbox: Sandbox) -> Product {
        Product {
            name: "app".to_string(),
            issues: RepoName::parse("octo/app").unwrap(),
            repos: vec![RepoName::parse("octo/app").unwrap()],
            cells: vec!["triage".to_string()],
            commands: Default::default(),
            sandbox,
        }
    }

    fn settings_with(config: &Config, product: &Product, placed: &[PathBuf]) -> serde_json::Value {
        let programs = [PathBuf::from("/home/me/.agentcells/bin/agentcells")];
        let run = RunPaths {
            worktree: Path::new("/w/octo/app/12"),
            git_dir: Path::new("/home/me/.agentcells/gitdirs/octo/app/12"),
            shared_objects: Path::new("/home/me/.agentcells/repos/octo/app/.git/objects"),
            context: Path::new("/home/me/.agentcells/runs/1/context.json"),
            placed,
            programs: &programs,
            tmp: Path::new("/home/me/.agentcells/tmp/12"),
            caches: &[],
        };
        runtime_settings(&home(), config, product, &run).unwrap()
    }

    fn settings_of(config: &Config, placed: &[PathBuf]) -> serde_json::Value {
        settings_with(config, &product(Sandbox::default()), placed)
    }

    fn user_home() -> PathBuf {
        super::user_home().unwrap()
    }

    fn strings(v: &serde_json::Value) -> Vec<String> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|d| d.as_str().unwrap().to_string())
            .collect()
    }

    fn rules_of(config: &Config, placed: &[PathBuf], kind: &str) -> Vec<String> {
        strings(&settings_of(config, placed)["permissions"][kind])
    }

    fn deny_of(config: &Config, placed: &[PathBuf]) -> Vec<String> {
        rules_of(config, placed, "deny")
    }

    #[test]
    fn allows_reading_and_writing_only_inside_the_worktree() {
        let allow = rules_of(&Config::default(), &[], "allow");
        assert_eq!(
            allow,
            vec![
                "Bash(agentcells cell:*)",
                "Read(//w/octo/app/12/**)",
                "Edit(//w/octo/app/12/**)",
            ]
        );
        let deny = deny_of(
            &Config::default(),
            &[PathBuf::from("/w/octo/app/12/key.jks")],
        );
        for rule in [
            "Edit(//w/octo/app/12/.git)",
            "Edit(//w/octo/app/12/.git/**)",
            "Edit(//w/octo/app/12/.claude/**)",
            "Read(//w/octo/app/12/key.jks)",
            "Edit(//w/octo/app/12/key.jks)",
        ] {
            assert!(deny.contains(&rule.to_string()), "{rule} should be denied");
        }
    }

    #[test]
    fn sandbox_writes_only_to_the_clone_and_hides_agentcells_home() {
        let config = Config {
            github_app: Some(GithubApp {
                id: "Iv23li".to_string(),
                private_key: PathBuf::from("/keys/app.pem"),
            }),
            ..Config::default()
        };
        let settings = settings_of(&config, &[PathBuf::from("/w/octo/app/12/key.jks")]);
        let sandbox = &settings["sandbox"];
        assert_eq!(sandbox["enabled"], true);
        // sandboxの外でコマンドを動かさせない
        assert_eq!(sandbox["allowUnsandboxedCommands"], false);
        let fs = &sandbox["filesystem"];
        // 書けるのは、この仕事の .git の実体と一時ディレクトリだけ。元のリポジトリには書けない
        assert_eq!(
            strings(&fs["allowWrite"]),
            vec![
                "/home/me/.agentcells/gitdirs/octo/app/12",
                "/home/me/.agentcells/tmp/12"
            ]
        );
        let deny_read = strings(&fs["denyRead"]);
        for path in [
            "/home/me/.agentcells",
            "/keys/app.pem",
            "/w/octo/app/12/key.jks",
        ] {
            assert!(
                deny_read.contains(&path.to_string()),
                "{path}: {deny_read:?}"
            );
        }
        let allow_read = strings(&fs["allowRead"]);
        for path in [
            "/w/octo/app/12",
            "/home/me/.agentcells/gitdirs/octo/app/12",
            "/home/me/.agentcells/repos/octo/app/.git/objects",
            "/home/me/.agentcells/runs/1/context.json",
        ] {
            assert!(
                allow_read.contains(&path.to_string()),
                "{path}: {allow_read:?}"
            );
        }
    }

    #[test]
    fn denies_label_and_state_changes_through_gh() {
        let deny = deny_of(&Config::default(), &[]);
        for rule in [
            "Bash(gh issue edit:*)",
            "Bash(gh pr edit:*)",
            "Bash(gh label:*)",
            "Bash(gh api:*)",
            "Bash(gh pr comment:*)",
            "Bash(gh pr review:*)",
            "Bash(gh pr create:*)",
            "Bash(git push:*)",
            "Bash(gh auth:*)",
        ] {
            assert!(deny.iter().any(|d| d == rule), "{rule} should be denied");
        }
        // 読むだけのコマンドは止めない
        assert!(!deny.iter().any(|d| d.contains("pr view")));
    }

    #[test]
    fn denies_placed_secret_files_by_absolute_path() {
        let deny = deny_of(
            &Config::default(),
            &[PathBuf::from("/work/app/local.properties")],
        );
        assert!(deny.contains(&"Read(//work/app/local.properties)".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn denies_both_the_written_and_the_resolved_path() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("app.pem"), "key").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let config = Config {
            github_app: Some(GithubApp {
                id: "Iv23li".to_string(),
                // config.toml からの相対パスで書くと .. が残る
                private_key: link.join("../link/app.pem"),
            }),
            ..Config::default()
        };
        let home = Home::new(link.clone());
        let run = RunPaths {
            worktree: &link,
            git_dir: &link,
            shared_objects: &link,
            context: &link,
            placed: &[],
            programs: &[],
            tmp: Path::new("/t"),
            caches: &[],
        };
        let settings =
            runtime_settings(&home, &config, &product(Sandbox::default()), &run).unwrap();
        let deny_read = strings(&settings["sandbox"]["filesystem"]["denyRead"]);

        let resolved = real.canonicalize().unwrap();
        for path in [resolved.join("app.pem"), resolved.clone(), link.clone()] {
            let path = path.display().to_string();
            assert!(
                deny_read.contains(&path),
                "{path} should be denied: {deny_read:?}"
            );
        }
    }

    #[test]
    fn finds_the_gh_config_dir_like_gh_does() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |key: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, v)| std::ffi::OsString::from(v))
            }
        };
        assert_eq!(
            gh_config_dir(env(&[("GH_CONFIG_DIR", "/x/gh"), ("HOME", "/home/me")])),
            Some(PathBuf::from("/x/gh"))
        );
        assert_eq!(
            gh_config_dir(env(&[
                ("XDG_CONFIG_HOME", "/x/config"),
                ("HOME", "/home/me")
            ])),
            Some(PathBuf::from("/x/config/gh"))
        );
        assert_eq!(
            gh_config_dir(env(&[("HOME", "/home/me")])),
            Some(PathBuf::from("/home/me/.config/gh"))
        );
        assert_eq!(gh_config_dir(env(&[("GH_CONFIG_DIR", "relative")])), None);
    }

    #[test]
    fn rejects_relative_secret_paths() {
        let placed = [PathBuf::from("local.properties")];
        let run = RunPaths {
            worktree: Path::new("/w"),
            git_dir: Path::new("/g"),
            shared_objects: Path::new("/r/.git/objects"),
            context: Path::new("/c.json"),
            placed: &placed,
            programs: &[],
            tmp: Path::new("/t"),
            caches: &[],
        };
        assert!(
            runtime_settings(
                &home(),
                &Config::default(),
                &product(Sandbox::default()),
                &run
            )
            .is_err()
        );
    }

    #[test]
    fn sandbox_hides_the_user_home_and_opens_only_listed_places() {
        let product = product(Sandbox {
            read: vec![PathBuf::from("/opt/sdk"), user_home().join(".cargo")],
            read_placed: vec![],
            network: vec!["registry.npmjs.org".to_string()],
            ..Sandbox::default()
        });
        let settings = settings_with(&Config::default(), &product, &[]);
        let fs = &settings["sandbox"]["filesystem"];
        let deny_read = strings(&fs["denyRead"]);
        let user_home = user_home().display().to_string();
        assert!(deny_read.contains(&user_home), "{deny_read:?}");
        // 開けた場所の中でも、秘密がよく置かれる場所は読ませない
        assert!(
            deny_read.contains(&format!("{user_home}/.ssh")),
            "{deny_read:?}"
        );
        let allow_read = strings(&fs["allowRead"]);
        for path in [
            "/opt/sdk".to_string(),
            format!("{user_home}/.cargo"),
            "/home/me/.agentcells/bin/agentcells".to_string(),
        ] {
            assert!(allow_read.contains(&path), "{path}: {allow_read:?}");
        }
        let network = &settings["sandbox"]["network"];
        assert_eq!(
            strings(&network["allowedDomains"]),
            vec!["registry.npmjs.org"]
        );
        // 書いていない先へは、聞かずに止める
        assert_eq!(network["strictAllowlist"], true);
        // sandboxの中では ~/.gitconfig が読めないので、gitに使う人の設定を読ませない
        assert_eq!(settings["env"]["GIT_CONFIG_GLOBAL"], "/dev/null");
        assert_eq!(settings["disableAllHooks"], true);
        assert_eq!(network["allowLocalBinding"], false);
        // ビルドやテストには、仕事ごとの一時ディレクトリを使わせる
        assert!(allow_read.contains(&"/home/me/.agentcells/tmp/12".to_string()));
        assert!(strings(&fs["allowWrite"]).contains(&"/home/me/.agentcells/tmp/12".to_string()));

        // 書かなければ、どこへも通信できない
        let settings = settings_of(&Config::default(), &[]);
        assert!(strings(&settings["sandbox"]["network"]["allowedDomains"]).is_empty());
        assert!(deny_of(&Config::default(), &[]).contains(&"WebFetch".to_string()));
    }

    #[test]
    fn read_placed_files_are_open_to_builds_and_the_read_tool_but_not_to_edits() {
        // macOSでは大文字・小文字を問わずに比べる
        let written = if cfg!(target_os = "macos") {
            "App/Google-Services.json"
        } else {
            "app/google-services.json"
        };
        let product = product(Sandbox {
            read_placed: vec![PathBuf::from(written)],
            ..Sandbox::default()
        });
        let placed = [
            PathBuf::from("/w/octo/app/12/app/google-services.json"),
            PathBuf::from("/w/octo/app/12/key.jks"),
        ];
        let settings = settings_with(&Config::default(), &product, &placed);
        let deny_read = strings(&settings["sandbox"]["filesystem"]["denyRead"]);
        assert!(!deny_read.contains(&"/w/octo/app/12/app/google-services.json".to_string()));
        assert!(deny_read.contains(&"/w/octo/app/12/key.jks".to_string()));
        // Readのdenyはsandboxにも効くので、ビルドに読ませるファイルはReadでも読める。書き換えはさせない
        let deny = strings(&settings["permissions"]["deny"]);
        assert!(!deny.contains(&"Read(//w/octo/app/12/app/google-services.json)".to_string()));
        assert!(deny.contains(&"Edit(//w/octo/app/12/app/google-services.json)".to_string()));
        assert!(deny.contains(&"Read(//w/octo/app/12/key.jks)".to_string()));
    }

    #[test]
    fn sandbox_read_cannot_reopen_protected_places() {
        let config = |read: Vec<PathBuf>| Config {
            products: vec![product(Sandbox {
                read,
                ..Sandbox::default()
            })],
            ..Config::default()
        };
        for bad in [
            PathBuf::from("/home/me/.agentcells/files"),
            PathBuf::from("/HOME/me/.agentcells"),
            user_home().join(".ssh/config"),
            user_home().join(".AWS"),
        ] {
            let problems = sandbox_problems(&home(), &config(vec![bad.clone()]));
            assert_eq!(problems.len(), 1, "{bad:?}: {problems:?}");
            assert!(problems[0].contains("開けられない場所"), "{problems:?}");
        }
        // 読ませない場所を含む、広い場所は書ける（中の読ませない場所は、より細かい決まりで止まる）
        let wide = config(vec![user_home().join(".config"), user_home()]);
        assert!(sandbox_problems(&home(), &wide).is_empty());

        let bad = config(vec![user_home().join(".ssh")]);
        let run = RunPaths {
            worktree: Path::new("/w"),
            git_dir: Path::new("/g"),
            shared_objects: Path::new("/r/.git/objects"),
            context: Path::new("/c.json"),
            placed: &[],
            programs: &[],
            tmp: Path::new("/t"),
            caches: &[],
        };
        assert!(runtime_settings(&home(), &bad, &bad.products[0], &run).is_err());
    }

    #[test]
    fn write_opens_places_for_writing_but_never_shell_or_claude_settings() {
        let product = product(Sandbox {
            write: vec![user_home().join(".gradle")],
            ..Sandbox::default()
        });
        let settings = settings_with(&Config::default(), &product, &[]);
        let fs = &settings["sandbox"]["filesystem"];
        let gradle = user_home().join(".gradle").display().to_string();
        assert!(strings(&fs["allowWrite"]).contains(&gradle));
        assert!(strings(&fs["allowRead"]).contains(&gradle));
        let deny_write = strings(&fs["denyWrite"]);
        for place in [".claude", ".zshrc", "Library/LaunchAgents"] {
            let place = user_home().join(place).display().to_string();
            assert!(deny_write.contains(&place), "{place}: {deny_write:?}");
        }
        // ~/.agentcells を入れると、この仕事の .git と一時ディレクトリにも書けなくなる
        assert!(!deny_write.contains(&"/home/me/.agentcells".to_string()));

        // 書かせない場所の中は、write に書けない（read なら書ける）
        let config = |sandbox: Sandbox| Config {
            products: vec![self::product(sandbox)],
            ..Config::default()
        };
        let bad = config(Sandbox {
            write: vec![user_home().join(".claude/hooks")],
            ..Sandbox::default()
        });
        assert_eq!(sandbox_problems(&home(), &bad).len(), 1);
        // agentcellsの置き場所を含む場所も、write に書けない
        let wide = config(Sandbox {
            write: vec![PathBuf::from("/home/me")],
            ..Sandbox::default()
        });
        let problems = sandbox_problems(&home(), &wide);
        assert!(problems[0].contains("含むので"), "{problems:?}");
        let fine = config(Sandbox {
            read: vec![user_home().join(".claude/hooks")],
            ..Sandbox::default()
        });
        assert!(sandbox_problems(&home(), &fine).is_empty());
    }

    #[test]
    fn caches_are_writable_in_agentcells_home() {
        let programs: [PathBuf; 0] = [];
        let caches = [PathBuf::from("/home/me/.agentcells/caches/app/gradle")];
        let run = RunPaths {
            worktree: Path::new("/w/octo/app/12"),
            git_dir: Path::new("/home/me/.agentcells/gitdirs/octo/app/12"),
            shared_objects: Path::new("/home/me/.agentcells/repos/octo/app/.git/objects"),
            context: Path::new("/home/me/.agentcells/runs/1/context.json"),
            placed: &[],
            programs: &programs,
            tmp: Path::new("/home/me/.agentcells/tmp/octo/app/12"),
            caches: &caches,
        };
        let settings = runtime_settings(
            &home(),
            &Config::default(),
            &product(Sandbox::default()),
            &run,
        )
        .unwrap();
        let fs = &settings["sandbox"]["filesystem"];
        let cache = "/home/me/.agentcells/caches/app/gradle".to_string();
        assert!(strings(&fs["allowWrite"]).contains(&cache));
        assert!(strings(&fs["allowRead"]).contains(&cache));
    }

    #[test]
    fn wide_writes_are_refused_and_writes_and_local_binding_warn() {
        let config = |sandbox: Sandbox| Config {
            products: vec![product(sandbox)],
            ..Config::default()
        };
        for wide in [".config", "Library", ".local"] {
            let bad = config(Sandbox {
                write: vec![user_home().join(wide)],
                ..Sandbox::default()
            });
            let problems = sandbox_problems(&home(), &bad);
            assert!(
                problems.iter().any(|p| p.contains("広すぎます")),
                "{wide}: {problems:?}"
            );
        }
        let risky = config(Sandbox {
            write: vec![user_home().join(".gradle")],
            bind_localhost: true,
            ..Sandbox::default()
        });
        assert!(sandbox_problems(&home(), &risky).is_empty());
        let warnings = sandbox_warnings(&risky);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[0].contains("caches を使って"), "{warnings:?}");
        assert!(warnings[1].contains("DockerのAPI"), "{warnings:?}");
    }

    #[test]
    fn warns_about_wide_reads_and_wildcard_hosts() {
        let config = Config {
            products: vec![product(Sandbox {
                read: vec![
                    user_home(),
                    user_home().join(".config"),
                    user_home().join(".cargo"),
                ],
                network: vec![
                    "*.amazonaws.com".to_string(),
                    "registry.npmjs.org".to_string(),
                ],
                ..Sandbox::default()
            })],
            ..Config::default()
        };
        let warnings = sandbox_warnings(&config);
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(warnings[0].contains("広すぎます"));
        assert!(warnings[2].contains("「*.amazonaws.com」"));
    }
}
