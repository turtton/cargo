#!/usr/bin/env bash
# HTTPS with the built cargo, outside the Nix sandbox (so with the network and
# the runner's real environment), and TLS verification always on:
#
#   - as the runner has it (no CA setting at all): recorded, not judged. The
#     nixpkgs libcurl has no CA bundle compiled in (--with-ca-fallback), so
#     this is the case README tells users about;
#   - with the CA file of the Nix configuration in SSL_CERT_FILE: must succeed;
#   - with the CA bundle of nixpkgs' cacert given explicitly
#     (CARGO_HTTP_CAINFO): must succeed;
#   - with a CA bundle that does not contain crates.io's issuer: must FAIL
#     with a certificate error, which shows that verification is on.
# `cargo fetch` of a manifest with one dependency reads the crates.io index and
# downloads the crate, both over HTTPS. Unlike ci/smoke-installed.sh this is not
# run in `env -i`: it shows what a user's shell does.
#
#   nix/darwin-ci-https.sh result-cargo/bin/cargo
#
# Written for the bash 3.2 and BSD tools of a stock macOS.
set -eo pipefail

cargo="$(cd "$(dirname "${1:?usage: darwin-ci-https.sh CARGO}")" && pwd)/$(basename "$1")"
work="$(mktemp -d "${RUNNER_TEMP:-/tmp}/https.XXXXXX")"
trap 'rm -rf "$work"' EXIT

mkdir -p "$work/project/src"
cat > "$work/project/Cargo.toml" <<'TOML'
[package]
name = "https-probe"
version = "0.0.0"
edition = "2021"

[dependencies]
cfg-if = "1"
TOML
echo 'fn main() {}' > "$work/project/src/main.rs"

# probe LABEL EXPECT [VAR=value ...]: EXPECT is ok, fail or record.
probe() {
  local label="$1" expect="$2" log got
  shift 2
  log="$work/$label.log"
  rm -rf "${work:?}/home"
  mkdir -p "$work/home"
  echo "== $label"
  if (cd "$work/project" && rm -f Cargo.lock && env -u SSL_CERT_FILE -u NIX_SSL_CERT_FILE -u CARGO_HTTP_CAINFO -u CURL_CA_BUNDLE "$@" CARGO_HOME="$work/home" CARGO_NET_RETRY=1 CARGO_TERM_COLOR=never "$cargo" fetch) > "$log" 2>&1; then
    got=ok
  else
    got=fail
  fi
  cat "$log"
  echo "$label: $got (expected: $expect)"
  RESULTS="$RESULTS$label: $got (expected $expect)
"
  case "$expect" in
    record) ;;
    "$got") ;;
    *) echo "::error::$label: $got, expected $expect"; FAILED=1 ;;
  esac
  if [ "$expect" = fail ] && [ "$got" = fail ]; then
    grep -Eiq 'certificate|SSL' "$log" || { echo "::error::$label failed, but not with a certificate error"; FAILED=1; }
  fi
}

RESULTS=""
FAILED=""

nix_ca="$(nix config show ssl-cert-file)"
[ -n "$nix_ca" ] || nix_ca="${NIX_SSL_CERT_FILE:-/etc/ssl/cert.pem}"
echo "CA file of the Nix configuration: $nix_ca"
[ -s "$nix_ca" ] || { echo "::error::$nix_ca is missing or empty"; exit 1; }
cacert="$(nix build --no-link --print-out-paths --inputs-from . nixpkgs#cacert)/etc/ssl/certs/ca-bundle.crt"
echo "nixpkgs cacert bundle: $cacert"
[ -s "$cacert" ] || { echo "::error::$cacert is missing or empty"; exit 1; }

# A CA that cannot have issued crates.io's certificate.
openssl req -x509 -newkey rsa:2048 -nodes -keyout "$work/bogus.key" -out "$work/bogus-ca.pem" -days 1 -subj "/CN=turtton-cargo-bogus-ca" 2> /dev/null
[ -s "$work/bogus-ca.pem" ]

probe runner-environment record
probe nix-ca-file ok SSL_CERT_FILE="$nix_ca"
probe explicit-ca ok CARGO_HTTP_CAINFO="$cacert"
probe bogus-ca fail CARGO_HTTP_CAINFO="$work/bogus-ca.pem"
probe bogus-ca-over-valid-env fail SSL_CERT_FILE="$nix_ca" CARGO_HTTP_CAINFO="$work/bogus-ca.pem"

if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  {
    echo "### build-darwin: HTTPS (outside the Nix sandbox)"
    echo
    echo '```'
    printf '%s' "$RESULTS"
    echo '```'
  } >> "$GITHUB_STEP_SUMMARY"
fi
[ -z "$FAILED" ] || exit 1
