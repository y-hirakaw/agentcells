//! ビルドツールのキャッシュ。使う人のもの（`~/.gradle` など）ではなく、agentcellsが持つ場所を使わせる。
//!
//! 使う人のキャッシュをsandboxから書けるようにすると、AIが書き換えたビルドがそこにコードを置け
//! （`~/.gradle/init.d` や `~/.cargo/config.toml` など）、あとで使う人がsandboxの外で動かすツールがそれを実行する。
//! agentcellsが持つキャッシュは、sandboxの中のビルドしか使わない。
//!
//! キャッシュは同じproductの仕事で共有するので、ある仕事のビルドが置いたものが、あとの仕事のビルドで動きうる。
//! コードを動かせる設定（Gradleの `init.d` など）と、検証されずに使われる展開済みのもの（cargoの `registry/src`、
//! Goのビルドのキャッシュなど）は、コマンドを動かす前に毎回消す。ダウンロードしたもののハッシュは、
//! ダウンロードのときにしか確かめられないので、残るもの（Gradle本体、jar、wheelなど）の差し替えは防げない。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// 1つのツールのキャッシュ。
pub struct Cache {
    /// `[product.sandbox]` の `caches` に書く名前
    pub name: &'static str,
    /// 使う人のキャッシュの場所（ホームからの相対パス）。止められた場所がここなら、このキャッシュを勧める
    places: &'static [&'static str],
    /// キャッシュの中の、コードを動かせる設定や、検証されずに使われるもの。コマンドを動かす前に消す
    code: &'static [&'static str],
    /// 消したあとに置き直すファイル（キャッシュからの相対パスと中身）
    files: &'static [(&'static str, &'static str)],
}

pub const CACHES: &[Cache] = &[
    Cache {
        name: "gradle",
        places: &[".gradle"],
        code: &[
            "init.d",
            "init.gradle",
            "init.gradle.kts",
            "gradle.properties",
            // 前の仕事のデーモンは、消したあとの設定でも動き続け、次のビルドを引き受ける
            "daemon",
        ],
        files: &[],
    },
    Cache {
        name: "npm",
        places: &[".npm"],
        // _cacache は読むたびにintegrityで確かめられるが、npxで入れたものはそのまま動く
        code: &["_npx"],
        files: &[],
    },
    Cache {
        name: "yarn",
        places: &["Library/Caches/Yarn", ".yarn", ".cache/yarn"],
        code: &[],
        files: &[],
    },
    Cache {
        name: "pnpm",
        places: &["Library/pnpm", ".pnpm-store", ".local/share/pnpm"],
        code: &[],
        files: &[],
    },
    Cache {
        name: "cargo",
        places: &[".cargo/registry", ".cargo/git"],
        code: &[
            "config",
            "config.toml",
            "credentials",
            "credentials.toml",
            "bin",
            // 展開済みのソース。cargoは展開したあとは確かめないので、毎回 registry/cache の .crate から展開し直させる
            "registry/src",
        ],
        files: &[],
    },
    Cache {
        name: "maven",
        places: &[".m2"],
        code: &["settings.xml"],
        // 使う人の ~/.m2/settings.xml は読ませないので（ファイルがあって読めないとmavenが止まりうる）、空の設定を使わせる
        files: &[("settings.xml", "<settings/>\n")],
    },
    Cache {
        name: "pip",
        places: &["Library/Caches/pip", ".cache/pip"],
        code: &[],
        files: &[],
    },
    Cache {
        name: "go",
        places: &["go/pkg/mod", "Library/Caches/go-build", ".cache/go-build"],
        // コンパイル済みのものは、そのままリンクされる
        code: &["build"],
        files: &[],
    },
];

pub fn is_known(name: &str) -> bool {
    CACHES.iter().any(|c| c.name == name)
}

pub fn names() -> Vec<&'static str> {
    CACHES.iter().map(|c| c.name).collect()
}

/// キャッシュ `name` を `dir` に向ける環境変数。`current` は、もとの環境変数を返す（足すものに使う）。
pub fn env(
    name: &str,
    dir: &Path,
    current: impl Fn(&str) -> Option<String>,
) -> Vec<(String, String)> {
    let dir_s = dir.display().to_string();
    let append = |key: &str, value: String| match current(key) {
        Some(old) if !old.trim().is_empty() => format!("{old} {value}"),
        _ => value,
    };
    let pairs: Vec<(&str, String)> = match name {
        // デーモンを残さない。残ると、次の仕事のビルドを、前の仕事の設定のまま引き受ける
        "gradle" => vec![
            ("GRADLE_USER_HOME", dir_s),
            (
                "GRADLE_OPTS",
                append(
                    "GRADLE_OPTS",
                    "-Dorg.gradle.daemon=false -Dkotlin.compiler.execution.strategy=in-process"
                        .to_string(),
                ),
            ),
        ],
        "npm" => vec![("npm_config_cache", dir_s)],
        "yarn" => vec![("YARN_CACHE_FOLDER", dir_s)],
        "pnpm" => vec![
            ("npm_config_store_dir", dir_s.clone()),
            ("PNPM_STORE_DIR", dir_s),
        ],
        // 使う人の ~/.cargo/config.toml も読まなくなる
        "cargo" => vec![("CARGO_HOME", dir_s)],
        "maven" => vec![
            (
                "MAVEN_OPTS",
                append("MAVEN_OPTS", format!("-Dmaven.repo.local={dir_s}")),
            ),
            (
                "MAVEN_ARGS",
                append(
                    "MAVEN_ARGS",
                    format!("-s {}", dir.join("settings.xml").display()),
                ),
            ),
        ],
        "pip" => vec![("PIP_CACHE_DIR", dir_s)],
        "go" => vec![
            ("GOMODCACHE", dir.join("mod").display().to_string()),
            ("GOCACHE", dir.join("build").display().to_string()),
        ],
        _ => vec![],
    };
    pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

/// 止められた場所（ホームからの相対パス）が、使う人のキャッシュなら、代わりに使うキャッシュの名前。
pub fn for_place(relative: &Path) -> Option<&'static str> {
    let relative = relative.to_string_lossy().to_lowercase();
    CACHES
        .iter()
        .find(|c| {
            c.places.iter().any(|p| {
                let p = p.to_lowercase();
                relative == p || relative.starts_with(&format!("{p}/"))
            })
        })
        .map(|c| c.name)
}

/// productのキャッシュの場所（`<caches_dir>/<product>/<name>`）。`tick` は、今の仕事のファイル（`Context::caches`）と
/// sandboxで書ける場所（`RunPaths::caches`）の両方を、ここから作る。ずれると、ビルドが黙って使う人のキャッシュを見に行く
pub fn dirs(caches_dir: &Path, product: &str, names: &[String]) -> BTreeMap<String, PathBuf> {
    names
        .iter()
        .map(|name| (name.clone(), caches_dir.join(product).join(name)))
        .collect()
}

/// キャッシュの中の、コードを動かせる設定を消す。ある仕事のビルドが置いたものを、あとの仕事で動かさないため。
pub fn clean(name: &str, dir: &Path) -> Result<()> {
    let Some(cache) = CACHES.iter().find(|c| c.name == name) else {
        return Ok(());
    };
    for entry in cache.code {
        let path = dir.join(entry);
        // シンボリックリンクはたどらずに、リンクそのものを消す
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let removed = if meta.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        removed.map_err(|e| {
            Error::new(format!("キャッシュの {} を消せません: {e}", path.display()))
                .with_hint("人間に回してください（needs-human）")
        })?;
    }
    for (file, content) in cache.files {
        let path = dir.join(file);
        std::fs::create_dir_all(dir)
            .and_then(|()| std::fs::write(&path, content))
            .map_err(|e| {
                Error::new(format!("キャッシュの {} を置けません: {e}", path.display()))
                    .with_hint("人間に回してください（needs-human）")
            })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn points_tools_at_the_given_dir() {
        let dir = Path::new("/c/app/gradle");
        let none = |_: &str| None;
        let gradle = env("gradle", dir, none);
        assert_eq!(
            gradle[0],
            ("GRADLE_USER_HOME".to_string(), "/c/app/gradle".to_string())
        );
        assert!(gradle[1].1.contains("-Dorg.gradle.daemon=false"));
        let maven = env("maven", Path::new("/c/m2"), |key| {
            (key == "MAVEN_OPTS").then(|| "-Xmx1g".to_string())
        });
        assert_eq!(
            maven,
            vec![
                (
                    "MAVEN_OPTS".to_string(),
                    "-Xmx1g -Dmaven.repo.local=/c/m2".to_string()
                ),
                (
                    "MAVEN_ARGS".to_string(),
                    "-s /c/m2/settings.xml".to_string()
                ),
            ]
        );
        assert_eq!(env("go", Path::new("/c/go"), none).len(), 2);
        for name in names() {
            assert!(!env(name, dir, none).is_empty(), "{name}");
        }
    }

    #[test]
    fn clean_removes_settings_that_can_run_code() {
        let dir = tempfile::tempdir().unwrap();
        let gradle = dir.path().join("gradle");
        std::fs::create_dir_all(gradle.join("init.d")).unwrap();
        std::fs::write(gradle.join("init.d/x.gradle"), "evil").unwrap();
        std::fs::write(
            gradle.join("gradle.properties"),
            "org.gradle.jvmargs=-javaagent:x",
        )
        .unwrap();
        std::fs::create_dir_all(gradle.join("caches")).unwrap();
        clean("gradle", &gradle).unwrap();
        assert!(!gradle.join("init.d").exists());
        assert!(!gradle.join("gradle.properties").exists());
        // ダウンロードしたものは残す
        assert!(gradle.join("caches").exists());

        let cargo = dir.path().join("cargo");
        std::fs::create_dir_all(&cargo).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.path().join("gradle"), cargo.join("bin")).unwrap();
        std::fs::write(cargo.join("config.toml"), "[build]\nrustc-wrapper = \"x\"").unwrap();
        std::fs::create_dir_all(cargo.join("registry/src/index/serde-1.0.0")).unwrap();
        std::fs::create_dir_all(cargo.join("registry/cache")).unwrap();
        clean("cargo", &cargo).unwrap();
        assert!(!cargo.join("config.toml").exists());
        // 展開済みのソースは消し、ダウンロードした .crate は残す
        assert!(!cargo.join("registry/src").exists());
        assert!(cargo.join("registry/cache").exists());
        assert!(std::fs::symlink_metadata(cargo.join("bin")).is_err());
        // リンク先は消さない
        assert!(gradle.join("caches").exists());

        // 使う人の設定の代わりに、空の設定を置き直す
        let maven = dir.path().join("maven");
        std::fs::create_dir_all(&maven).unwrap();
        std::fs::write(
            maven.join("settings.xml"),
            "<settings><mirrors>evil</mirrors></settings>",
        )
        .unwrap();
        clean("maven", &maven).unwrap();
        assert_eq!(
            std::fs::read_to_string(maven.join("settings.xml")).unwrap(),
            "<settings/>\n"
        );
    }

    #[test]
    fn dirs_are_per_product() {
        let dirs = dirs(Path::new("/h/caches"), "app", &["gradle".to_string()]);
        assert_eq!(dirs["gradle"], PathBuf::from("/h/caches/app/gradle"));
    }

    #[test]
    fn maps_blocked_places_to_caches() {
        assert_eq!(
            for_place(Path::new(".gradle/caches/x.lock")),
            Some("gradle")
        );
        assert_eq!(for_place(Path::new(".npm/_cacache")), Some("npm"));
        assert_eq!(for_place(Path::new(".cargo/registry/index")), Some("cargo"));
        // ~/.cargo/bin は、キャッシュではなくツールチェーン
        assert_eq!(for_place(Path::new(".cargo/bin/cargo")), None);
        assert_eq!(for_place(Path::new(".gradlefoo")), None);
    }
}
