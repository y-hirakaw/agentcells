//! ビルドツールのキャッシュ。使う人のもの（`~/.gradle` など）ではなく、agentcellsが持つ場所を使わせる。
//!
//! 使う人のキャッシュをsandboxから書けるようにすると、AIが書き換えたビルドがそこにコードを置け
//! （`~/.gradle/init.d` や `~/.cargo/config.toml` など）、あとで使う人がsandboxの外で動かすツールがそれを実行する。
//! agentcellsが持つキャッシュは、sandboxの中のビルドしか使わない。
//!
//! キャッシュは同じproductの仕事で共有するので、ある仕事のビルドが置いたものが、あとの仕事のビルドで動きうる。
//! コードを動かせる設定（Gradleの `init.d` など）は、コマンドを動かす前に毎回消す。キャッシュの中身
//! （Gradle本体やjar）の差し替えは残るので、ビルドの側で検証する（`distributionSha256Sum` など）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// 1つのツールのキャッシュ。
pub struct Cache {
    /// `[product.sandbox]` の `caches` に書く名前
    pub name: &'static str,
    /// 使う人のキャッシュの場所（ホームからの相対パス）。止められた場所がここなら、このキャッシュを勧める
    places: &'static [&'static str],
    /// キャッシュの中の、コードを動かせる設定。コマンドを動かす前に消す
    code: &'static [&'static str],
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
        ],
    },
    Cache {
        name: "npm",
        places: &[".npm"],
        code: &[],
    },
    Cache {
        name: "yarn",
        places: &["Library/Caches/Yarn", ".yarn", ".cache/yarn"],
        code: &[],
    },
    Cache {
        name: "pnpm",
        places: &["Library/pnpm", ".pnpm-store", ".local/share/pnpm"],
        code: &[],
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
        ],
    },
    Cache {
        name: "maven",
        places: &[".m2"],
        code: &[],
    },
    Cache {
        name: "pip",
        places: &["Library/Caches/pip", ".cache/pip"],
        code: &[],
    },
    Cache {
        name: "go",
        places: &["go/pkg/mod", "Library/Caches/go-build", ".cache/go-build"],
        code: &[],
    },
];

pub fn is_known(name: &str) -> bool {
    CACHES.iter().any(|c| c.name == name)
}

pub fn names() -> Vec<&'static str> {
    CACHES.iter().map(|c| c.name).collect()
}

/// キャッシュ `name` を `dir` に向ける環境変数。`maven_opts` は、もとの `MAVEN_OPTS`。
pub fn env(name: &str, dir: &Path, maven_opts: Option<&str>) -> Vec<(String, String)> {
    let dir_s = dir.display().to_string();
    let pairs: Vec<(&str, String)> = match name {
        "gradle" => vec![("GRADLE_USER_HOME", dir_s)],
        "npm" => vec![("npm_config_cache", dir_s)],
        "yarn" => vec![("YARN_CACHE_FOLDER", dir_s)],
        "pnpm" => vec![
            ("npm_config_store_dir", dir_s.clone()),
            ("PNPM_STORE_DIR", dir_s),
        ],
        // 使う人の ~/.cargo/config.toml も読まなくなる
        "cargo" => vec![("CARGO_HOME", dir_s)],
        "maven" => {
            let local = format!("-Dmaven.repo.local={dir_s}");
            let opts = match maven_opts {
                Some(opts) if !opts.trim().is_empty() => format!("{opts} {local}"),
                _ => local,
            };
            vec![("MAVEN_OPTS", opts)]
        }
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn points_tools_at_the_given_dir() {
        let dir = Path::new("/c/app/gradle");
        assert_eq!(
            env("gradle", dir, None),
            vec![("GRADLE_USER_HOME".to_string(), "/c/app/gradle".to_string())]
        );
        assert_eq!(
            env("maven", Path::new("/c/m2"), Some("-Xmx1g")),
            vec![(
                "MAVEN_OPTS".to_string(),
                "-Xmx1g -Dmaven.repo.local=/c/m2".to_string()
            )]
        );
        assert_eq!(env("go", Path::new("/c/go"), None).len(), 2);
        for name in names() {
            assert!(!env(name, dir, None).is_empty(), "{name}");
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
        clean("cargo", &cargo).unwrap();
        assert!(!cargo.join("config.toml").exists());
        assert!(std::fs::symlink_metadata(cargo.join("bin")).is_err());
        // リンク先は消さない
        assert!(gradle.join("caches").exists());
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
