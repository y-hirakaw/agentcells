#!/bin/sh
# agentcellsをソースからビルドして、$AGENTCELLS_HOME/bin（既定は ~/.agentcells/bin）に入れる。
# 何度実行してもよい。入れたあとの確認と設定は `agentcells setup` に任せる。
#
# 使い方: このリポジトリをcloneして、その中で ./install.sh を実行する。
set -eu

fail() {
  printf 'エラー: %s\n' "$1" >&2
  printf '直し方: %s\n' "$2" >&2
  exit 1
}

src_dir=$(cd "$(dirname "$0")" && pwd)
[ -f "$src_dir/Cargo.toml" ] ||
  fail "$src_dir にCargo.tomlがありません" \
    "agentcellsのリポジトリをcloneして、その中で ./install.sh を実行してください"

command -v cargo >/dev/null 2>&1 ||
  fail "Rust（cargo）が見つかりません" \
    "https://rustup.rs の手順でRustを入れてから、もう一度実行してください"

if [ -n "${AGENTCELLS_HOME:-}" ]; then
  home=$AGENTCELLS_HOME
else
  [ -n "${HOME:-}" ] ||
    fail "ホームディレクトリがわかりません" "環境変数 HOME か AGENTCELLS_HOME を設定してください"
  home=$HOME/.agentcells
fi
bin_dir=$home/bin

echo "ビルドしています（rust-toolchain.tomlのバージョンが入っていなければ、先にダウンロードします）"
(cd "$src_dir" && cargo build --release --locked) ||
  fail "ビルドに失敗しました" \
    "上のエラーを確かめてください。Rustが古い場合は \`rustup update\` で直ることがあります"

mkdir -p "$bin_dir" ||
  fail "$bin_dir を作れません" "親ディレクトリの権限を確かめてください"
# 実行中のバイナリを上書きしないよう、一時ファイルに置いてから入れ替える。
cp "$src_dir/target/release/agentcells" "$bin_dir/agentcells.tmp"
chmod 755 "$bin_dir/agentcells.tmp"
mv -f "$bin_dir/agentcells.tmp" "$bin_dir/agentcells"
echo "入れました: $bin_dir/agentcells"
echo

# setupは置き場所を作ったあとdoctorの結果（PATHの案内も含む）を出す。
# doctorで直す項目があっても、インストール自体は終わっている。
AGENTCELLS_HOME=$home "$bin_dir/agentcells" setup || true
