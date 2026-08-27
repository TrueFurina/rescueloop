#!/bin/sh
set -eu

repo="${RESCUELOOP_REPOSITORY:-ostapondo/rescueloop}"
version="${RESCUELOOP_VERSION:-latest}"
install_dir="${RESCUELOOP_INSTALL_DIR:-$HOME/.local/bin}"

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) asset="rescueloop-macos-aarch64.tar.gz" ;;
  Darwin-x86_64) asset="rescueloop-macos-x86_64.tar.gz" ;;
  *) echo "RescueLoop installer supports macOS arm64 and x86_64." >&2; exit 1 ;;
esac

if [ "$version" = latest ]; then
  base="https://github.com/$repo/releases/latest/download"
else
  base="https://github.com/$repo/releases/download/$version"
fi

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT INT TERM
curl --fail --location --proto '=https' --tlsv1.2 "$base/$asset" -o "$tmp_dir/$asset"
curl --fail --location --proto '=https' --tlsv1.2 "$base/SHA256SUMS" -o "$tmp_dir/SHA256SUMS"
expected="$(awk -v file="$asset" '$2 == file { print $1 }' "$tmp_dir/SHA256SUMS")"
[ -n "$expected" ] || { echo "Checksum for $asset is missing." >&2; exit 1; }
actual="$(shasum -a 256 "$tmp_dir/$asset" | awk '{print $1}')"
[ "$actual" = "$expected" ] || { echo "Checksum verification failed." >&2; exit 1; }

tar -xzf "$tmp_dir/$asset" -C "$tmp_dir"
mkdir -p "$install_dir"
install -m 755 "$tmp_dir/rescueloop" "$install_dir/rescueloop"

profile="$HOME/.zprofile"
marker="# RescueLoop PATH"
if ! grep -Fq "$marker" "$profile" 2>/dev/null; then
  printf '\n%s\nexport PATH="%s:$PATH"\n' "$marker" "$install_dir" >> "$profile"
fi

echo "Installed RescueLoop to $install_dir/rescueloop"
echo "Open a new terminal, then run: rescueloop setup"
