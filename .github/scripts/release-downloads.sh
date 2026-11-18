#!/usr/bin/env bash
# Writes the 「下载地址」 section of a release's notes from the files actually
# attached to it, replacing the section a previous run wrote. linux-deb.yml
# and linux-release.yml both call this after uploading, one after the other
# (they share a concurrency group), so the second run sees every file.
#
# Usage: release-downloads.sh <tag>   (needs GH_TOKEN and GITHUB_REPOSITORY)
set -euo pipefail

tag=$1
repo=${GITHUB_REPOSITORY:?}
base="https://github.com/${repo}/releases/download/${tag}"
begin='<!-- downloads:begin -->'
end='<!-- downloads:end -->'

assets=$(gh release view "$tag" -R "$repo" --json assets --jq '.assets[].name')
pick() { grep -E -e "$1" <<<"$assets" | head -n1 || true; }
deb=$(pick '_amd64\.deb$')
deb_arm=$(pick '_arm64\.deb$')
archive=$(pick '-x86_64-linux\.tar\.zst$')
sum=$(pick '-x86_64-linux\.tar\.zst\.sha256$')
binary=$(pick '^OpenUUYC$')

lines=()
links=()
[ -n "$deb" ] && links+=("[64 位](${base}/${deb})")
[ -n "$deb_arm" ] && links+=("[ARM64](${base}/${deb_arm})")
if [ ${#links[@]} -gt 0 ]; then
  line="- DEB（Debian / Ubuntu 22.04 及以上）："
  for i in "${!links[@]}"; do
    [ "$i" -gt 0 ] && line+=" | "
    line+="${links[$i]}"
  done
  lines+=("$line")
fi
if [ -n "$archive" ]; then
  line="- 压缩包（Arch 等其他发行版，含虚拟屏辅助程序、桌面入口与 systemd 单元）：[64 位](${base}/${archive})"
  [ -n "$sum" ] && line+=" | [SHA256](${base}/${sum})"
  lines+=("$line")
fi
[ -n "$binary" ] && lines+=("- 单文件程序：[64 位](${base}/${binary})")
if [ ${#lines[@]} -eq 0 ]; then
  echo "no Linux packages attached to ${tag} yet"
  exit 0
fi

section=$(printf '%s\n' "$begin" '## 下载地址' '' '**Linux：**' '' "${lines[@]}" "$end")

notes=$(gh release view "$tag" -R "$repo" --json body --jq .body)
# Drop the section a previous run wrote, then append the current one.
notes=$(awk -v b="$begin" -v e="$end" '
  $0 == b { skip = 1; next }
  $0 == e { skip = 0; next }
  !skip' <<<"$notes")
notes=$(printf '%s\n' "$notes" | sed -e :a -e '/^\n*$/{$d;N;ba' -e '}')

file=$(mktemp)
printf '%s\n\n%s\n' "$notes" "$section" > "$file"
gh release edit "$tag" -R "$repo" --notes-file "$file"
rm -f "$file"
echo "wrote the download links for ${tag}:"
printf '%s\n' "${lines[@]}"
