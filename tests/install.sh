#!/bin/bash
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
temporary=$(mktemp -d "${TMPDIR:-/tmp}/opp-install-test.XXXXXX")
trap 'rm -rf "$temporary"' EXIT

mock_bin="$temporary/bin"
release="$temporary/release"
payload="$temporary/payload"
mkdir -p "$mock_bin" "$release" "$payload"

cat > "$payload/opp" <<'EOF'
#!/bin/sh
case "${1:-}" in
  --version) printf '%s\n' "${OPP_TEST_FAKE_VERSION:-opp 0.1.0}" ;;
  stop) printf 'stop\n' >> "$OPP_TEST_STOP_LOG" ;;
  *) exit 2 ;;
esac
EOF
chmod 755 "$payload/opp"

for target in aarch64-apple-darwin x86_64-apple-darwin; do
  COPYFILE_DISABLE=1 tar -C "$payload" -czf "$release/opp-${target}.tar.gz" opp
done
(
  cd "$release"
  shasum -a 256 opp-*.tar.gz > SHA256SUMS
)

cat > "$mock_bin/uname" <<'EOF'
#!/bin/sh
case "${1:-}" in
  -s) printf '%s\n' "${OPP_TEST_UNAME_S:-Darwin}" ;;
  -m) printf '%s\n' "${OPP_TEST_UNAME_M:-arm64}" ;;
  *) exit 2 ;;
esac
EOF

cat > "$mock_bin/curl" <<'EOF'
#!/bin/sh
output=
url=
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o)
      shift
      output=${1:-}
      ;;
    http://*|https://*) url=$1 ;;
  esac
  shift
done
[ -n "$output" ] && [ -n "$url" ] || exit 2
printf '%s\n' "$url" >> "$OPP_TEST_CURL_LOG"
name=${url##*/}
cp "$OPP_TEST_RELEASE_DIR/$name" "$output"
if [ "${OPP_TEST_CORRUPT_DOWNLOAD:-}" = "$name" ]; then
  printf 'corrupt\n' >> "$output"
fi
EOF
chmod 755 "$mock_bin/uname" "$mock_bin/curl"

run_installer() {
  local home=$1
  local architecture=$2
  local stop_log=$3
  local curl_log=$4
  shift 4

  env \
    HOME="$home" \
    PATH="$mock_bin:/usr/bin:/bin" \
    OPP_TEST_UNAME_M="$architecture" \
    OPP_TEST_STOP_LOG="$stop_log" \
    OPP_TEST_CURL_LOG="$curl_log" \
    OPP_TEST_RELEASE_DIR="$release" \
    "$@" \
    sh "$root/scripts/install.sh"
}

sh -n "$root/scripts/install.sh"

for architecture in arm64 x86_64; do
  home="$temporary/home-$architecture"
  stop_log="$temporary/stop-$architecture.log"
  curl_log="$temporary/curl-$architecture.log"
  mkdir -p "$home"

  output=$(run_installer "$home" "$architecture" "$stop_log" "$curl_log" 2>&1)
  test -x "$home/.local/bin/opp"
  test "$("$home/.local/bin/opp" --version)" = "opp 0.1.0"
  test "$(wc -l < "$stop_log")" -eq 1
  case "$architecture" in
    arm64) target=aarch64-apple-darwin ;;
    x86_64) target=x86_64-apple-darwin ;;
  esac
  grep -Fq "/opp-${target}.tar.gz" "$curl_log"
  grep -Fq "add $home/.local/bin to PATH" <<<"$output"

  printf 'old executable\n' > "$home/.local/bin/opp"
  run_installer "$home" "$architecture" "$stop_log" "$curl_log" >/dev/null 2>&1
  test "$("$home/.local/bin/opp" --version)" = "opp 0.1.0"
  test "$(wc -l < "$stop_log")" -eq 2
  test -z "$(find "$home/.local/bin" -name '.opp.install.*' -print -quit)"
done

failure_home="$temporary/home-checksum-failure"
mkdir -p "$failure_home/.local/bin"
printf 'unchanged\n' > "$failure_home/.local/bin/opp"
if run_installer \
  "$failure_home" \
  arm64 \
  "$temporary/stop-checksum.log" \
  "$temporary/curl-checksum.log" \
  OPP_TEST_CORRUPT_DOWNLOAD=opp-aarch64-apple-darwin.tar.gz \
  >/dev/null 2>&1; then
  echo "installer accepted a corrupt archive" >&2
  exit 1
fi
test "$(cat "$failure_home/.local/bin/opp")" = "unchanged"
test ! -e "$temporary/stop-checksum.log"

bad_layout_release="$temporary/release-bad-layout"
bad_layout_payload="$temporary/payload-bad-layout"
cp -R "$release" "$bad_layout_release"
cp -R "$payload" "$bad_layout_payload"
printf 'unexpected\n' > "$bad_layout_payload/extra"
COPYFILE_DISABLE=1 tar \
  -C "$bad_layout_payload" \
  -czf "$bad_layout_release/opp-aarch64-apple-darwin.tar.gz" \
  opp \
  extra
(
  cd "$bad_layout_release"
  shasum -a 256 opp-*.tar.gz > SHA256SUMS
)

bad_layout_home="$temporary/home-bad-layout"
mkdir -p "$bad_layout_home/.local/bin"
printf 'unchanged\n' > "$bad_layout_home/.local/bin/opp"
if run_installer \
  "$bad_layout_home" \
  arm64 \
  "$temporary/stop-bad-layout.log" \
  "$temporary/curl-bad-layout.log" \
  OPP_TEST_RELEASE_DIR="$bad_layout_release" \
  >/dev/null 2>&1; then
  echo "installer accepted an archive with extra files" >&2
  exit 1
fi
test "$(cat "$bad_layout_home/.local/bin/opp")" = "unchanged"
test ! -e "$temporary/stop-bad-layout.log"

bad_version_home="$temporary/home-bad-version"
mkdir -p "$bad_version_home/.local/bin"
printf 'unchanged\n' > "$bad_version_home/.local/bin/opp"
if run_installer \
  "$bad_version_home" \
  arm64 \
  "$temporary/stop-bad-version.log" \
  "$temporary/curl-bad-version.log" \
  OPP_TEST_FAKE_VERSION="unexpected 0.1.0" \
  >/dev/null 2>&1; then
  echo "installer accepted an unexpected version string" >&2
  exit 1
fi
test "$(cat "$bad_version_home/.local/bin/opp")" = "unchanged"
test ! -e "$temporary/stop-bad-version.log"

unsupported_home="$temporary/home-unsupported"
mkdir -p "$unsupported_home"
if run_installer \
  "$unsupported_home" \
  arm64 \
  "$temporary/stop-unsupported.log" \
  "$temporary/curl-unsupported.log" \
  OPP_TEST_UNAME_S=Linux \
  >/dev/null 2>&1; then
  echo "installer accepted an unsupported operating system" >&2
  exit 1
fi
test ! -e "$unsupported_home/.local/bin/opp"
