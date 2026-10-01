#!/bin/sh
set -eu

repository="binbandit/oh-fx"
install_dir="${OH_FX_INSTALL_DIR:-$HOME/.local/bin}"

fail() {
  echo "oh-fx: $1" >&2
  exit 1
}

case "$(uname -s)" in
  Linux) os=linux ;;
  Darwin) os=macos ;;
  *) fail "unsupported operating system $(uname -s)" ;;
esac

case "$(uname -m)" in
  x86_64 | amd64) arch=x86_64 ;;
  arm64 | aarch64) arch=aarch64 ;;
  *) fail "unsupported architecture $(uname -m)" ;;
esac

if [ "$os" = macos ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = 1 ]; then
  arch=aarch64
fi

if [ -n "${1:-}" ]; then
  base="https://github.com/$repository/releases/download/$1"
else
  base="https://github.com/$repository/releases/latest/download"
fi

download() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$1" -o "$2"
  else
    wget -qO "$2" "$1"
  fi
}

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d ' ' -f 1
  else
    shasum -a 256 "$1" | cut -d ' ' -f 1
  fi
}

archive="oh-fx-$os-$arch.tar.gz"
workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT

download "$base/$archive" "$workdir/$archive" || fail "failed to download $archive"
download "$base/$archive.sha256" "$workdir/$archive.sha256" || fail "failed to download $archive.sha256"
[ "$(cut -d ' ' -f 1 <"$workdir/$archive.sha256")" = "$(sha256 "$workdir/$archive")" ] ||
  fail "downloaded archive failed integrity check"

tar -xzf "$workdir/$archive" -C "$workdir" oh-fx
mkdir -p "$install_dir"
install -m 755 "$workdir/oh-fx" "$install_dir/.oh-fx-install"
mv -f "$install_dir/.oh-fx-install" "$install_dir/oh-fx"
ln -sf oh-fx "$install_dir/ofx"

echo "installed oh-fx $("$install_dir/oh-fx" --version) to $install_dir/oh-fx"
case ":$PATH:" in
  *":$install_dir:"*) ;;
  *) echo "add $install_dir to your PATH: export PATH=\"$install_dir:\$PATH\"" ;;
esac
