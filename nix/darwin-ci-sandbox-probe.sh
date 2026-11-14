#!/usr/bin/env bash
# Proves that the Nix sandbox of the runner is on, not only configured: a
# derivation reads a world-readable file outside the store and outside what the
# Darwin sandbox grants. With `--option sandbox true` it must not see the file;
# with `--option sandbox false` (the control) it must, which shows that the file
# is readable for the build user and that the denial is the sandbox's.
#
# Written for the bash 3.2 and BSD tools of a stock macOS.
set -eo pipefail

dir="$(mktemp -d "${RUNNER_TEMP:-/tmp}/sandbox-probe.XXXXXX")"
trap 'rm -rf "$dir" "$probe"' EXIT
# /Users/Shared is writable and readable for every user; the sandbox does not
# grant it.
probe="/Users/Shared/nix-sandbox-probe-$$-${GITHUB_RUN_ID:-local}"
echo "hello from outside the sandbox" > "$probe"
chmod 644 "$probe"
read -r line < "$probe"
echo "the runner user reads it: $line"

cat > "$dir/probe.nix" <<'NIX'
{ path, nonce, expect }:
derivation {
  name = "sandbox-probe-${nonce}";
  system = "aarch64-darwin";
  builder = "/bin/sh";
  args = [
    "-c"
    ''
      if read -r line < ${path} 2> /dev/null; then
        result=readable
      else
        result=hidden
      fi
      echo "the build sees the file: $result"
      [ "$result" = ${expect} ] || { echo "expected: ${expect}"; exit 1; }
      echo "$result" > $out
    ''
  ];
}
NIX

echo "== sandbox on: the file must be hidden"
nix build -L --no-link --impure --file "$dir/probe.nix" \
  --option sandbox true \
  --argstr path "$probe" --argstr nonce "on-$$" --argstr expect hidden 2>&1 | tee "$dir/on.log"
if grep -Eiq 'ignoring the (client|user)-specified setting|ignored' "$dir/on.log"; then
  echo "::error::Nix ignored the sandbox setting"
  exit 1
fi

echo "== control, sandbox off: the same file must be readable"
nix build -L --no-link --impure --file "$dir/probe.nix" \
  --option sandbox false \
  --argstr path "$probe" --argstr nonce "off-$$" --argstr expect readable 2>&1 | tee "$dir/off.log" || {
  echo "::warning::the control (sandbox off) failed; the daemon may not accept the setting from this user"
  cat "$dir/off.log"
  exit 1
}
echo "the sandbox is on: the build cannot read $probe"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  echo "### build-darwin: sandbox" >> "$GITHUB_STEP_SUMMARY"
  echo "sandbox = true; a derivation cannot read a world-readable file outside the store (control with sandbox off can)." >> "$GITHUB_STEP_SUMMARY"
fi
