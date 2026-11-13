//! PRで変わったら、AIのレビューだけで通さないファイル。守るファイル（productの `protected`）と違い、
//! 設定に書かなくても効く。レビューのcellがPRで動くときは特に確かめさせ、人に回すときに、人が差分を読むよう
//! 書き足す。外からのIssueに仕込まれた指示で、これらを変えるPRが、AIのレビューだけで通らないようにする。
//!
//! - 依存とビルドの設定: 依存の取得元と版、ビルドやインストールのときに動くコード、CIを決める。
//!   マージしたあとに、CIや使う人の手元で、sandboxの外で動く（cellのsandboxが効くのは、cellが動いている間だけ）
//! - エージェントへの指示: CLAUDE.md、AGENTS.md など。マージしたあとに、開発のエージェント（sandboxの外で動く）
//!   とcellが、指示として読む。ドキュメントの直しに見えるので、人も読み飛ばしやすい
//!
//! どちらも網羅ではない。テストやスクリプトも、マージしたあとに手元やCIで動く。

/// 依存とビルドの設定の、ファイルの名前（大文字と小文字は区別しない）。
const BUILD_NAMES: &[&str] = &[
    // Rust
    "cargo.toml",
    "cargo.lock",
    "build.rs",
    "rust-toolchain",
    "rust-toolchain.toml",
    // JavaScript
    "package.json",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "pnpm-workspace.yaml",
    ".pnpmfile.cjs",
    "bun.lock",
    "bun.lockb",
    "bunfig.toml",
    "deno.json",
    "deno.jsonc",
    "deno.lock",
    ".npmrc",
    ".yarnrc",
    ".yarnrc.yml",
    // Go
    "go.mod",
    "go.sum",
    "go.work",
    "go.work.sum",
    // Python
    "constraints.txt",
    "pipfile",
    "pipfile.lock",
    "pyproject.toml",
    "poetry.lock",
    "uv.lock",
    "setup.py",
    "setup.cfg",
    "pip.conf",
    // Ruby
    "gemfile",
    "gemfile.lock",
    // JVM
    "gradle.properties",
    "gradle-wrapper.properties",
    "gradle-wrapper.jar",
    "gradlew",
    "gradlew.bat",
    "libs.versions.toml",
    "verification-metadata.xml",
    "pom.xml",
    "mvnw",
    "mvnw.cmd",
    "build.sbt",
    // Swift、iOS
    "package.swift",
    "package.resolved",
    "podfile",
    "podfile.lock",
    "cartfile",
    "cartfile.resolved",
    "project.pbxproj",
    // Unity
    "packages-lock.json",
    // .NET
    "packages.lock.json",
    "nuget.config",
    "global.json",
    // PHP、Dart、Elixir、Nix
    "composer.json",
    "composer.lock",
    "pubspec.yaml",
    "pubspec.lock",
    "mix.exs",
    "mix.lock",
    "flake.nix",
    "flake.lock",
    // ビルドと、gitやCIで動くもの
    "makefile",
    "gnumakefile",
    "cmakelists.txt",
    "dockerfile",
    "containerfile",
    ".gitmodules",
    ".gitattributes",
    ".pre-commit-config.yaml",
    ".gitlab-ci.yml",
    "jenkinsfile",
    "azure-pipelines.yml",
    "renovate.json",
];

/// 依存とビルドの設定の、拡張子（大文字と小文字は区別しない）。
const BUILD_EXTENSIONS: &[&str] = &[
    "gemspec",
    "podspec",
    "gradle",
    "csproj",
    "fsproj",
    "vbproj",
    "props",
    "targets",
    "mk",
    "cmake",
    "dockerfile",
];

/// 依存とビルドの設定の、ディレクトリ（この下のすべて。どの深さにあっても）。
const BUILD_DIRS: &[&str] = &[
    ".cargo",
    ".github",
    ".githooks",
    ".husky",
    ".mvn",
    ".yarn",
    "buildsrc",
    ".circleci",
    ".buildkite",
];

/// エージェントへの指示の、ファイルの名前（大文字と小文字は区別しない。どの深さにあっても）。
const AGENT_NAMES: &[&str] = &[
    "claude.md",
    "claude.local.md",
    "agents.md",
    "gemini.md",
    ".cursorrules",
    ".windsurfrules",
    ".clinerules",
    "copilot-instructions.md",
    // 開発のエージェントが起動するプログラムを決める
    ".mcp.json",
];

/// エージェントへの指示の、ディレクトリ（この下のすべて）。
const AGENT_DIRS: &[&str] = &[
    ".claude",
    ".cursor",
    ".codex",
    ".gemini",
    ".windsurf",
    ".clinerules",
    ".kiro",
];

/// `path`（リポジトリからの相対パス）を、小文字の部分に分ける。
fn parts(path: &str) -> Vec<String> {
    path.to_lowercase()
        .split('/')
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect()
}

/// ディレクトリのどれかの下か。ディレクトリをファイルやsubmoduleに置き換えたときも当たるように、
/// 最後の部分もディレクトリとして見る。
fn under(parts: &[String], dirs: &[&str]) -> bool {
    parts.iter().any(|p| dirs.contains(&p.as_str()))
}

/// `path`（リポジトリからの相対パス）が、依存とビルドの設定のファイルか。
pub fn is_build_file(path: &str) -> bool {
    let parts = parts(path);
    let Some(name) = parts.last().map(String::as_str) else {
        return false;
    };
    let extension = name.rsplit_once('.').map(|(_, ext)| ext);
    under(&parts, BUILD_DIRS)
        || BUILD_NAMES.contains(&name)
        || extension.is_some_and(|ext| BUILD_EXTENSIONS.contains(&ext))
        || name.ends_with(".gradle.kts")
        || name.starts_with("dockerfile.")
        // requirements.txt、requirements-dev.txt、requirements/base.txt
        || (name.starts_with("requirements") && name.ends_with(".txt"))
        || (extension == Some("txt") && parts.iter().rev().nth(1).is_some_and(|d| d == "requirements"))
        // Unityのパッケージ
        || parts.ends_with(&["packages".to_string(), "manifest.json".to_string()])
}

/// `path`（リポジトリからの相対パス）が、エージェントへの指示のファイルか。
pub fn is_agent_file(path: &str) -> bool {
    let parts = parts(path);
    under(&parts, AGENT_DIRS)
        || parts
            .last()
            .is_some_and(|name| AGENT_NAMES.contains(&name.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dependency_and_build_files_are_found_anywhere() {
        for path in [
            "Cargo.lock",
            "crates/core/Cargo.toml",
            "build.rs",
            "web/package-lock.json",
            "app/build.gradle.kts",
            "app/build.gradle",
            "buildSrc/src/main/kotlin/Deps.kt",
            "gradle/wrapper/gradle-wrapper.properties",
            "Gemfile",
            "my.gemspec",
            "requirements-dev.txt",
            "requirements/base.txt",
            "Dockerfile",
            "docker/Dockerfile.dev",
            "App.xcodeproj/project.pbxproj",
            "Packages/manifest.json",
            ".github/workflows/ci.yml",
            ".cargo/config.toml",
            ".GitHub",
            "sub/.githooks/pre-commit",
            ".gitmodules",
        ] {
            assert!(is_build_file(path), "{path}");
        }
        for path in [
            "src/main.rs",
            "README.md",
            "docs/build.md",
            "Cargo.toml.orig",
            "src/github.rs",
            "gemspec",
            "notes.txt",
            "src/manifest.json",
            "CLAUDE.md",
            "",
        ] {
            assert!(!is_build_file(path), "{path}");
        }
    }

    #[test]
    fn agent_instructions_are_found_anywhere() {
        for path in [
            "CLAUDE.md",
            "src/claude.md",
            "AGENTS.md",
            ".claude/settings.json",
            ".cursor/rules/a.mdc",
            ".github/copilot-instructions.md",
            ".mcp.json",
            ".Claude",
        ] {
            assert!(is_agent_file(path), "{path}");
        }
        for path in ["README.md", "docs/claude.txt", "src/agents.rs", ""] {
            assert!(!is_agent_file(path), "{path}");
        }
    }
}
