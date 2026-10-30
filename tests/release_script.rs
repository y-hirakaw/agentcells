//! `scripts/release.sh`（版を上げるスクリプト）を、写した場所で動かして確かめる。
//! CIでは、ubuntu（mawk、GNU sort）と macOS（BSDのawk）の両方で動く。

use std::path::Path;
use std::process::{Command, Output};

const CARGO_TOML: &str = "[package]\nname = \"agentcells\"\nversion = \"0.9.2\"\n\n[dependencies]\nfoo = { version = \"1.0.0\" }\n";
const CARGO_LOCK: &str = "version = 4\n\n[[package]]\nname = \"agentcells\"\nversion = \"0.9.2\"\ndependencies = [\n \"foo\",\n]\n\n[[package]]\nname = \"foo\"\nversion = \"1.0.0\"\n";
const CHANGELOG: &str =
    "# CHANGELOG\n\n説明\n\n## [0.9.2] - 2026-01-01\n\n### 直したこと\n\n- 前のこと\n";

fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("scripts")).unwrap();
    std::fs::create_dir_all(root.join("changes")).unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/release.sh"),
        root.join("scripts/release.sh"),
    )
    .unwrap();
    std::fs::write(root.join("Cargo.toml"), CARGO_TOML).unwrap();
    std::fs::write(root.join("Cargo.lock"), CARGO_LOCK).unwrap();
    std::fs::write(root.join("CHANGELOG.md"), CHANGELOG).unwrap();
    dir
}

fn release(root: &Path, args: &[&str]) -> Output {
    Command::new("bash")
        .arg(root.join("scripts/release.sh"))
        .args(args)
        // 呼ぶ人のロケールによらず動くことも確かめる
        .env("LC_ALL", "en_US.UTF-8")
        .output()
        .unwrap()
}

fn read(root: &Path, name: &str) -> String {
    std::fs::read_to_string(root.join(name)).unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn collects_changes_into_a_minor_release() {
    let dir = repo();
    let root = dir.path();
    // 後ろの空白や CR（Windowsの改行）がある見出しも読む
    std::fs::write(
        root.join("changes/10.md"),
        "\r\n### 直したこと  \r\n\r\n- 直した10\r\n\r\n",
    )
    .unwrap();
    std::fs::write(
        root.join("changes/9.md"),
        "### 足したこと\n\n- 足した9\n  - 細かいこと\n\n### 直したこと\n\n- 直した9\n",
    )
    .unwrap();

    // 試すだけなら、何も変えない
    let dry = release(root, &["--dry-run"]);
    assert!(dry.status.success(), "{}", stderr(&dry));
    assert!(String::from_utf8_lossy(&dry.stdout).contains("0.9.2 → 0.10.0"));
    assert_eq!(read(root, "Cargo.toml"), CARGO_TOML);
    assert!(root.join("changes/9.md").exists());

    // 動かしている間に日付が変わってもよいように、前と後の日付を読む
    let today = || {
        let out = Command::new("date").arg("+%Y-%m-%d").output().unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    };
    let before = today();
    let out = release(root, &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    let after = today();
    let changelog = read(root, "CHANGELOG.md");
    // 見出しごとに、Issue番号の順（9 が 10 より先）にまとめ、前の版の上に入れる
    let expected = |date: &str| {
        format!(
            "# CHANGELOG\n\n説明\n\n## [0.10.0] - {date}\n\n### 足したこと\n\n- 足した9\n  - 細かいこと\n\n### 直したこと\n\n- 直した9\n- 直した10\n\n## [0.9.2] - 2026-01-01\n\n### 直したこと\n\n- 前のこと\n"
        )
    };
    assert!(
        changelog == expected(&before) || changelog == expected(&after),
        "{changelog}"
    );
    assert!(
        read(root, "Cargo.toml")
            .contains("[package]\nname = \"agentcells\"\nversion = \"0.10.0\"\n")
    );
    // ほかのパッケージの版は変えない
    assert!(read(root, "Cargo.toml").contains("foo = { version = \"1.0.0\" }"));
    let lock = read(root, "Cargo.lock");
    assert!(
        lock.contains("name = \"agentcells\"\nversion = \"0.10.0\"\n"),
        "{lock}"
    );
    assert!(
        lock.contains("name = \"foo\"\nversion = \"1.0.0\"\n"),
        "{lock}"
    );
    assert_eq!(std::fs::read_dir(root.join("changes")).unwrap().count(), 0);
    assert!(release(root, &["--check"]).status.success());
}

#[test]
fn fixes_only_raise_the_patch() {
    let dir = repo();
    let root = dir.path();
    std::fs::write(root.join("changes/3.md"), "### 直したこと\n\n- 直した\n").unwrap();
    let out = release(root, &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(read(root, "CHANGELOG.md").contains("## [0.9.3] - "));
    assert!(!read(root, "CHANGELOG.md").contains("### 足したこと"));
    assert!(read(root, "Cargo.lock").contains("version = \"0.9.3\""));
}

#[test]
fn refuses_unknown_headings_and_text_before_a_heading() {
    let dir = repo();
    let root = dir.path();
    let out = release(root, &[]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("まとめるファイルがありません"));

    std::fs::write(root.join("changes/1.md"), "前置き\n### 直したこと\n- x\n").unwrap();
    std::fs::write(root.join("changes/2.md"), "### その他\n- y\n").unwrap();
    let out = release(root, &[]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(
        err.contains("changes/1.md: 見出しの前に中身があります"),
        "{err}"
    );
    assert!(
        err.contains("changes/2.md: 見出し「### その他」は使えません"),
        "{err}"
    );
    // 何も変えない
    assert_eq!(read(root, "Cargo.toml"), CARGO_TOML);
    assert_eq!(read(root, "CHANGELOG.md"), CHANGELOG);
    assert!(root.join("changes/1.md").exists());
}

#[test]
fn check_finds_versions_that_drift_apart() {
    let dir = repo();
    let root = dir.path();
    assert!(release(root, &["--check"]).status.success());

    std::fs::write(
        root.join("Cargo.lock"),
        CARGO_LOCK.replacen("0.9.2", "0.9.1", 1),
    )
    .unwrap();
    let out = release(root, &["--check"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("Cargo.lock の agentcells の版（0.9.1）"),
        "{}",
        stderr(&out)
    );

    std::fs::write(root.join("Cargo.lock"), CARGO_LOCK).unwrap();
    std::fs::write(
        root.join("CHANGELOG.md"),
        CHANGELOG.replace("0.9.2", "0.9.0"),
    )
    .unwrap();
    let out = release(root, &["--check"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("CHANGELOG.md の最新の版（0.9.0）"),
        "{}",
        stderr(&out)
    );
    // ずれていれば、上げる前に止める
    std::fs::write(root.join("changes/1.md"), "### 直したこと\n- x\n").unwrap();
    assert!(!release(root, &[]).status.success());
    assert!(root.join("changes/1.md").exists());
}

#[test]
fn changes_nothing_when_a_file_cannot_be_written() {
    use std::os::unix::fs::PermissionsExt;
    let dir = repo();
    let root = dir.path();
    std::fs::write(root.join("changes/1.md"), "### 直したこと\n- x\n").unwrap();
    std::fs::set_permissions(
        root.join("Cargo.lock"),
        std::fs::Permissions::from_mode(0o444),
    )
    .unwrap();
    let out = release(root, &[]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("Cargo.lock に書けません"),
        "{}",
        stderr(&out)
    );
    assert_eq!(read(root, "CHANGELOG.md"), CHANGELOG);
    assert_eq!(read(root, "Cargo.toml"), CARGO_TOML);
    assert!(root.join("changes/1.md").exists());
}

/// このリポジトリの版がそろっていること（CIでも確かめる）。
#[test]
fn this_repository_is_consistent() {
    let out = release(Path::new(env!("CARGO_MANIFEST_DIR")), &["--check"]);
    assert!(out.status.success(), "{}", stderr(&out));
}
