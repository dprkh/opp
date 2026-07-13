#!/bin/bash
set -euo pipefail

if [[ $# -ne 4 ]]; then
  echo "usage: $0 VERSION ARCHITECTURE TARGET OUTPUT_DIRECTORY" >&2
  exit 2
fi

version=$1
architecture=$2
target=$3
output_directory=$4

case "$architecture:$target" in
  arm64:aarch64-apple-darwin | amd64:x86_64-apple-darwin) ;;
  *)
    echo "unsupported architecture and target: $architecture $target" >&2
    exit 2
    ;;
esac

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

manifest_version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -1)
if [[ "$manifest_version" != "$version" ]]; then
  echo "version $version does not match Cargo.toml version $manifest_version" >&2
  exit 1
fi

cargo build --locked --release --no-default-features --bin opp --target "$target"
binary="target/$target/release/opp"
if [[ "$($binary --version)" != "opp $version" ]]; then
  echo "release binary reported an unexpected version" >&2
  exit 1
fi

case "$architecture" in
  arm64) file "$binary" | grep -q 'arm64' ;;
  amd64) file "$binary" | grep -q 'x86_64' ;;
esac

minimum_os=$(otool -l "$binary" | awk '
  $1 == "cmd" && $2 == "LC_BUILD_VERSION" { build = 1; next }
  build && $1 == "minos" { print $2; exit }
')
if [[ "$minimum_os" != "12.0" ]]; then
  echo "release binary has unexpected minimum macOS version: $minimum_os" >&2
  exit 1
fi

mkdir -p "$output_directory"
output_directory=$(cd "$output_directory" && pwd)
staging=$(mktemp -d)
trap 'rm -rf "$staging"' EXIT
cp "$binary" "$staging/opp"
chmod 755 "$staging/opp"

archive="$output_directory/opp_${version}_darwin_${architecture}.tar.gz"
COPYFILE_DISABLE=1 tar -C "$staging" -czf "$archive" opp
if [[ "$(tar -tzf "$archive")" != "opp" ]]; then
  echo "release archive must contain exactly one file named opp" >&2
  exit 1
fi

echo "$archive"
