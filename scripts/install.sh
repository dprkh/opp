#!/bin/sh
set -eu

release_url="https://github.com/dprkh/opp/releases/latest/download"

fail() {
  printf 'opp installer: %s\n' "$*" >&2
  exit 1
}

download() {
  curl --retry 3 -L --proto '=https' --tlsv1.2 -sSf -o "$2" "$1"
}

[ "$(uname -s)" = "Darwin" ] || fail "only macOS is supported"

case "$(uname -m)" in
  arm64) target="aarch64-apple-darwin" ;;
  x86_64) target="x86_64-apple-darwin" ;;
  *) fail "unsupported macOS architecture: $(uname -m)" ;;
esac

[ -n "${HOME:-}" ] || fail "HOME is not set"
case "$HOME" in
  /*) ;;
  *) fail "HOME must be an absolute path" ;;
esac

archive="opp-${target}.tar.gz"
temporary_root=${TMPDIR:-/tmp}
work_directory=$(mktemp -d "${temporary_root%/}/opp-install.XXXXXX") ||
  fail "could not create a temporary directory"
staged_install=

cleanup() {
  [ -z "$staged_install" ] || rm -f "$staged_install"
  rm -rf "$work_directory"
}

trap cleanup EXIT
trap 'exit 1' HUP INT TERM
umask 077

download "$release_url/$archive" "$work_directory/$archive" ||
  fail "could not download $archive"
download "$release_url/SHA256SUMS" "$work_directory/SHA256SUMS" ||
  fail "could not download SHA256SUMS"

awk -v archive="$archive" '
  $2 == archive { print; matches += 1 }
  END { if (matches != 1) exit 1 }
' "$work_directory/SHA256SUMS" > "$work_directory/ARCHIVE.SHA256" ||
  fail "SHA256SUMS does not contain exactly one checksum for $archive"

(
  cd "$work_directory"
  shasum -a 256 -c ARCHIVE.SHA256 >/dev/null
) || fail "checksum verification failed for $archive"

[ "$(tar -tzf "$work_directory/$archive")" = "opp" ] ||
  fail "$archive must contain exactly one file named opp"
tar -xzf "$work_directory/$archive" -C "$work_directory"
[ -x "$work_directory/opp" ] || fail "the release executable is not runnable"

reported_version=$("$work_directory/opp" --version) ||
  fail "the release executable did not report its version"
case "$reported_version" in
  "opp "*) ;;
  *) fail "the release executable reported an unexpected version" ;;
esac

"$work_directory/opp" stop ||
  fail "could not stop the existing broker; the installed executable was not changed"

install_directory="$HOME/.local/bin"
destination="$install_directory/opp"
[ ! -d "$destination" ] || fail "$destination is a directory"
mkdir -p "$install_directory" || fail "could not create $install_directory"

staged_install="$install_directory/.opp.install.$$"
install -m 0755 "$work_directory/opp" "$staged_install" ||
  fail "could not stage the executable in $install_directory"
mv -f "$staged_install" "$destination" || fail "could not replace $destination"
staged_install=

printf 'Installed %s at %s\n' "$reported_version" "$destination"
case ":${PATH:-}:" in
  *":$install_directory:"*) ;;
  *) printf 'opp installer: add %s to PATH\n' "$install_directory" >&2 ;;
esac
