#!/usr/bin/env sh
set -eu
repo='zeebo0-UI/Fetchman'
release=$(curl -fsSL -A Fetchman-installer "https://api.github.com/repos/$repo/releases/latest")
os=$(uname -s); arch=$(uname -m)
case "$os:$arch" in Linux:x86_64) target='x86_64-unknown-linux-gnu';; Darwin:x86_64) target='x86_64-apple-darwin';; Darwin:arm64) target='aarch64-apple-darwin';; *) echo 'No Fetchman build for this platform.' >&2; exit 1;; esac
asset=$(printf '%s' "$release" | sed -n 's/.*"name"[[:space:]]*:[[:space:]]*"\([^"]*'"$target"'[^" ]*\.tar\.gz\)".*/\1/p' | head -n1)
[ -n "$asset" ] || { echo 'No Fetchman release asset found.' >&2; exit 1; }
tag=$(printf '%s' "$release" | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n1)
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
url="https://github.com/$repo/releases/download/$tag/$asset"
curl -fsSL "$url" -o "$tmp/a.tar.gz"; curl -fsSL "$url.sha256" -o "$tmp/a.sha256"
(cd "$tmp" && { command -v sha256sum >/dev/null && sha256sum -c a.sha256 || shasum -a 256 -c a.sha256; })
bin="${XDG_BIN_HOME:-$HOME/.local/bin}"; mkdir -p "$bin"; tar -xzf "$tmp/a.tar.gz" -C "$tmp"; install -m 755 "$tmp/fetchman" "$bin/fetchman"
printf 'Fetchman installed to %s\n' "$bin/fetchman"
case ":${PATH:-}:" in *:"$bin":*) ;; *) echo "Add $bin to PATH, then run: fetchman --help";; esac
