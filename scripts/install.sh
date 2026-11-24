#!/bin/sh
# Installs nekorisu. No sudo needed.
#
#   curl -fsSL https://github.com/NEKO-CwC/nekorisu/releases/latest/download/install.sh | sh
#
# What it does:
#   1. downloads nekorisu-<platform>.tar.gz and SHA256SUMS, checks the SHA-256
#   2. puts the binary in ~/.nekorisu/versions/<version>/nekorisu
#   3. points ~/.local/bin/nekorisu at it (later versions switch this link)
#   4. registers core to start at login (launchd / systemd --user)
#
# The first install trusts HTTPS to GitHub, as most installers do; every later
# update is checked against the signing keys built into the binary.
#
# Environment:
#   NEKORISU_DOWNLOAD_BASE  where to download from (default: GitHub's latest release)
#   NEKORISU_HOME           default ~/.nekorisu
#   NEKORISU_BIN_DIR        default ~/.local/bin
#   NEKORISU_NO_AUTOSTART=1 skip step 4

set -eu

BASE="${NEKORISU_DOWNLOAD_BASE:-https://github.com/NEKO-CwC/nekorisu/releases/latest/download}"
HOME_DIR="${NEKORISU_HOME:-$HOME/.nekorisu}"
BIN_DIR="${NEKORISU_BIN_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "$*"; }
die() { printf '错误：%s\n' "$*" >&2; exit 1; }

case "$(uname -s)" in
  Darwin) os=apple-darwin ;;
  Linux) os=unknown-linux-musl ;;
  *) die "不支持这个系统：$(uname -s)（只支持 macOS 和 Linux）" ;;
esac
case "$(uname -m)" in
  arm64 | aarch64) arch=aarch64 ;;
  x86_64 | amd64) arch=x86_64 ;;
  *) die "不支持这个架构：$(uname -m)" ;;
esac
target="$arch-$os"
asset="nekorisu-$target.tar.gz"

if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL --retry 3 -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -q -O "$2" "$1"; }
else
  die "需要 curl 或 wget"
fi
if command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
  sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
  die "需要 sha256sum 或 shasum"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

say "下载 $asset ……"
fetch "$BASE/$asset" "$tmp/$asset" || die "下载不了 $BASE/$asset"
fetch "$BASE/SHA256SUMS" "$tmp/SHA256SUMS" || die "下载不了 $BASE/SHA256SUMS"
want="$(grep " $asset\$" "$tmp/SHA256SUMS" | cut -d' ' -f1)"
[ -n "$want" ] || die "SHA256SUMS 里没有 $asset"
got="$(sha256 "$tmp/$asset")"
[ "$want" = "$got" ] || die "SHA-256 对不上，下载的文件可能损坏或被篡改"

mkdir -p "$tmp/unpack"
tar -xzf "$tmp/$asset" -C "$tmp/unpack"
[ -f "$tmp/unpack/nekorisu" ] || die "压缩包里没有 nekorisu"
chmod 755 "$tmp/unpack/nekorisu"
version="$("$tmp/unpack/nekorisu" --version | awk '{print $2}')"
[ -n "$version" ] || die "运行不了下载的 nekorisu（平台不对？）"

link="$BIN_DIR/nekorisu"
if [ -e "$link" ] && [ ! -L "$link" ]; then
  die "$link 已经存在，而且不是安装脚本建的链接；先移走它再装"
fi

dest="$HOME_DIR/versions/$version"
mkdir -p "$dest" "$BIN_DIR"
chmod 700 "$HOME_DIR"
mv -f "$tmp/unpack/nekorisu" "$dest/nekorisu"
rm -f "$link.nekorisu-new"
ln -s "$dest/nekorisu" "$link.nekorisu-new"
mv -f "$link.nekorisu-new" "$link"
say "已安装 nekorisu $version：$link → $dest/nekorisu"

# A core from an earlier install keeps running the old version until it restarts.
"$link" core stop >/dev/null 2>&1 || true

if [ "${NEKORISU_NO_AUTOSTART:-0}" != 1 ]; then
  "$link" core enable || say "开机自启没注册成功，可以稍后运行：nekorisu core enable"
fi

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) say "提示：$BIN_DIR 不在 PATH 里，把它加进 shell 的配置文件（比如 ~/.zshrc）：export PATH=\"$BIN_DIR:\$PATH\"" ;;
esac
say "下一步：nekorisu scan 看看本机有哪些会话"
