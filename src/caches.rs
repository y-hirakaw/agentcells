//! ビルドツールのキャッシュ。使う人のもの（`~/.gradle` など）ではなく、agentcellsが持つ場所を使わせる。
//!
//! 使う人のキャッシュをsandboxから書けるようにすると、AIが書き換えたビルドがそこにコードを置け
//! （`~/.gradle/init.d` や `~/.cargo/config.toml` など）、あとで使う人がsandboxの外で動かすツールがそれを実行する。
//! agentcellsが持つキャッシュは、sandboxの中のビルドしか使わない。

use std::path::{Path, PathBuf};

/// 1つのツールのキャッシュ。
pub struct Cache {
    /// `[product.sandbox]` の `caches` に書く名前
    pub name: &'static str,
    /// 使う人のキャッシュの場所（ホームからの相対パス）。止められた場所がここなら、このキャッシュを勧める
    places: &'static [&'static str],
}

pub const CACHES: &[Cache] = &[
    Cache {
        name: "gradle",
        places: &[".gradle"],
    },
    Cache {
        name: "npm",
        places: &[".npm"],
    },
    Cache {
        name: "yarn",
        places: &["Library/Caches/Yarn", ".yarn", ".cache/yarn"],
    },
    Cache {
        name: "pnpm",
        places: &["Library/pnpm", ".pnpm-store", ".local/share/pnpm"],
    },
    Cache {
        name: "cargo",
        places: &[".cargo/registry", ".cargo/git"],
    },
    Cache {
        name: "maven",
        places: &[".m2"],
    },
    Cache {
        name: "pip",
        places: &["Library/Caches/pip", ".cache/pip"],
    },
    Cache {
        name: "go",
        places: &["go/pkg/mod", "Library/Caches/go-build", ".cache/go-build"],
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
        "pnpm" => vec![("npm_config_store_dir", dir_s)],
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

/// productのキャッシュを置く場所の名前（`<product>/<name>`）。
pub fn relative_dir(product: &str, name: &str) -> PathBuf {
    Path::new(product).join(name)
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
