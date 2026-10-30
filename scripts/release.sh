#!/usr/bin/env bash
# agentcells の版を上げる。`changes/*.md` を CHANGELOG.md の新しい版の節にまとめ、Cargo.toml と Cargo.lock の
# 版をそろえ、`changes/` のファイルを消す。足したことが1つでもあればマイナー、直しだけならパッチを上げる
# （1.0.0 まで）。コミットとタグは付けない（タグは、マージのあとに main を扱う人かエージェントが付ける）。
#
#   scripts/release.sh            版を上げる
#   scripts/release.sh --dry-run  何も書き換えず、上げる版と、書き足す節を出す
#   scripts/release.sh --check    Cargo.toml、Cargo.lock、CHANGELOG.md の最新の版がそろっているかを確かめる（CI）
#
# 手で踏むと、Cargo.lock のずれ、changes/ の消し忘れが起きやすいので、決まった処理にした。
set -euo pipefail
# 文字列をバイトで比べる（macOS の awk は、UTF-8 のロケールでは「### 足したこと」と「### 直したこと」を同じとみなす）
export LC_ALL=C

cd "$(dirname "$0")/.."

fail() {
  echo "release: $*" >&2
  exit 1
}

# [package] の version
package_version() {
  awk '
    /^\[/ { in_package = ($0 == "[package]") }
    in_package && /^version *= *"/ { split($0, v, "\""); print v[2]; exit }
  ' Cargo.toml
}

# Cargo.lock の agentcells の version
lock_version() {
  awk '
    $0 == "name = \"agentcells\"" { found = 1; next }
    found && /^version = "/ { split($0, v, "\""); print v[2]; exit }
    /^$/ { found = 0 }
  ' Cargo.lock
}

# CHANGELOG.md の最初の版の見出し（## [x.y.z] - 日付）の版
changelog_version() {
  awk '/^## \[[0-9]/ { v = $2; sub(/^\[/, "", v); sub(/\]$/, "", v); print v; exit }' CHANGELOG.md
}

check() {
  local package lock changelog
  package=$(package_version)
  lock=$(lock_version)
  changelog=$(changelog_version)
  [ -n "$package" ] || fail "Cargo.toml に [package] の version がありません"
  [ "$lock" = "$package" ] ||
    fail "Cargo.lock の agentcells の版（${lock:-なし}）が Cargo.toml（${package}）と違います。cargo build で合わせてください"
  [ "$changelog" = "$package" ] ||
    fail "CHANGELOG.md の最新の版（${changelog:-なし}）が Cargo.toml（${package}）と違います"
  echo "版はそろっています: ${package}"
}

mode=release
case "${1:-}" in
  "") ;;
  --dry-run) mode=dry-run ;;
  --check) mode=check ;;
  *) fail "使い方: scripts/release.sh [--dry-run | --check]" ;;
esac

if [ "$mode" = check ]; then
  check
  exit 0
fi

# 前の版がそろっていなければ、上げる前に止める（どこからずれたかが分からなくなるため）
check >/dev/null

shopt -s nullglob
files=(changes/*.md)
[ ${#files[@]} -gt 0 ] || fail "changes/ にまとめるファイルがありません"
# Issue番号の順（数の順）に並べる
IFS=$'\n' read -r -d '' -a files < <(printf '%s\n' "${files[@]}" | sort -V && printf '\0') || true

# 見出しごとに中身を集める。見出しは「### 足したこと」と「### 直したこと」だけ。見出しの前に中身があるもの、
# ほかの見出しがあるものは、まとめ方が分からないので止める
sections=$(awk '
  FNR == 1 { section = "" }
  # 行の後ろの空白と CR（Windowsの改行）は見ない
  { sub(/[ \t\r]+$/, "") }
  /^### / {
    if ($0 == "### 足したこと") section = "added"
    else if ($0 == "### 直したこと") section = "fixed"
    else { printf "%s: 見出し「%s」は使えません（### 足したこと か ### 直したこと）\n", FILENAME, $0 > "/dev/stderr"; bad = 1; section = "skip" }
    next
  }
  section == "skip" { next }
  section == "" {
    if ($0 !~ /^[[:space:]]*$/) { printf "%s: 見出しの前に中身があります\n", FILENAME > "/dev/stderr"; bad = 1 }
    next
  }
  { lines[section] = lines[section] FILENAME "\t" $0 "\n" }
  END {
    if (bad) exit 1
    # 各ファイルの中身の、前後の空行を除いてつなぐ
    for (s = 1; s <= 2; s++) {
      name = (s == 1) ? "added" : "fixed"
      n = split(lines[name], all, "\n")
      out = ""; current = ""; chunk = ""
      for (i = 1; i < n; i++) {
        tab = index(all[i], "\t")
        file = substr(all[i], 1, tab - 1); text = substr(all[i], tab + 1)
        if (file != current) { out = out trim(chunk); chunk = ""; current = file }
        chunk = chunk text "\n"
      }
      out = out trim(chunk)
      if (out != "") printf "%s\n%s\036", name, out
    }
  }
  function trim(s) {
    sub(/^(\n|[ \t]*\n)+/, "", s)
    sub(/(\n[ \t]*)+$/, "", s)
    return (s == "") ? "" : s "\n"
  }
' "${files[@]}") || fail "changes/ のファイルを直してください"

added=$(printf '%s' "$sections" | awk 'BEGIN { RS = "\036" } $1 == "added" { sub(/^added\n/, ""); printf "%s", $0 }')
fixed=$(printf '%s' "$sections" | awk 'BEGIN { RS = "\036" } $1 == "fixed" { sub(/^fixed\n/, ""); printf "%s", $0 }')
[ -n "$added$fixed" ] || fail "changes/ のファイルに中身がありません"

current=$(package_version)
IFS=. read -r major minor patch <<<"$current"
if [ -n "$added" ]; then
  next="$major.$((minor + 1)).0"
else
  next="$major.$minor.$((patch + 1))"
fi

section="## [$next] - $(date +%Y-%m-%d)"$'\n'
[ -z "$added" ] || section+=$'\n'"### 足したこと"$'\n\n'"$added"$'\n'
[ -z "$fixed" ] || section+=$'\n'"### 直したこと"$'\n\n'"$fixed"$'\n'

if [ "$mode" = dry-run ]; then
  echo "${current} → ${next}（まとめるファイル: ${files[*]}）"
  echo
  printf '%s' "$section"
  exit 0
fi

# 途中で書けずに、一部だけ上がった状態で止まらないよう、先に書けるかを見て、新しい中身をすべて作ってから書く
for file in CHANGELOG.md Cargo.toml Cargo.lock; do
  [ -w "$file" ] || fail "$file に書けません"
done
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT

# CHANGELOG.md の、最初の版の見出しの前に入れる
SECTION="$section" awk '
  !done && /^## \[/ { printf "%s\n", ENVIRON["SECTION"]; done = 1 }
  { print }
  END { if (!done) printf "\n%s", ENVIRON["SECTION"] }
' CHANGELOG.md >"$work/CHANGELOG.md"

NEXT="$next" awk '
  /^\[/ { in_package = ($0 == "[package]") }
  in_package && !done && /^version *= *"/ { print "version = \"" ENVIRON["NEXT"] "\""; done = 1; next }
  { print }
' Cargo.toml >"$work/Cargo.toml"

NEXT="$next" awk '
  $0 == "name = \"agentcells\"" { found = 1; print; next }
  found && /^version = "/ { print "version = \"" ENVIRON["NEXT"] "\""; found = 0; next }
  /^$/ { found = 0 }
  { print }
' Cargo.lock >"$work/Cargo.lock"

# 元のファイルに書き戻す（ファイルの権限を変えないように、置き換えずに書く）
cat "$work/CHANGELOG.md" >CHANGELOG.md
cat "$work/Cargo.toml" >Cargo.toml
cat "$work/Cargo.lock" >Cargo.lock
# そろったことを確かめてから、まとめたファイルを消す
check >/dev/null
rm -f -- "${files[@]}"
echo "${current} → ${next} に上げました（まとめたファイル: ${files[*]}）"
echo "CHANGELOG.md の節を読み、なぜやったかが足りなければ書き足してから、コミットしてください。タグはマージのあとに付けます"
