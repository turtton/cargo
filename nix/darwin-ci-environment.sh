#!/usr/bin/env bash
# First step of the `build-darwin` job (.github/workflows/nix.yml): records the
# runner and the Nix configuration, and fails when the job is not running on
# what it assumes: an arm64 runner, the Nix sandbox on, cache.nixos.org as the
# only substituter and the flake's own nixConfig (the attic cache) not accepted.
#
# Written for the bash 3.2 and BSD tools of a stock macOS.
set -eo pipefail

summary() {
  if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    cat >> "$GITHUB_STEP_SUMMARY"
  else
    cat > /dev/null
  fi
}

echo "== machine"
uname -a
sw_vers
arch="$(uname -m)"
echo "uname -m: $arch"
[ "$arch" = arm64 ] || { echo "::error::the runner is $arch, not arm64"; exit 1; }
system="$(nix eval --impure --raw --expr builtins.currentSystem)"
echo "nix system: $system"
[ "$system" = aarch64-darwin ] || { echo "::error::the Nix system is $system, not aarch64-darwin"; exit 1; }
nix --version

echo "== resources"
echo "cpus: $(sysctl -n hw.ncpu)"
echo "memory: $(( $(sysctl -n hw.memsize) / 1048576 )) MiB"
vm_stat | head -8
df -h / /nix 2>&1 || true
top -l 1 -n 0 2>/dev/null | sed -n '1,12p' || true

echo "== nix configuration"
for key in sandbox substituters trusted-substituters trusted-public-keys accept-flake-config trusted-users ssl-cert-file max-jobs cores; do
  printf '%s = %s\n' "$key" "$(nix config show "$key")"
done

sandbox="$(nix config show sandbox)"
[ "$sandbox" = true ] || { echo "::error::sandbox is '$sandbox', not true"; exit 1; }
substituters="$(nix config show substituters | sed 's#/$##')"
[ "$substituters" = https://cache.nixos.org ] || { echo "::error::substituters are '$substituters', not only cache.nixos.org"; exit 1; }
[ "$(nix config show accept-flake-config)" = false ] || { echo "::error::accept-flake-config is on"; exit 1; }
keys="$(nix config show trusted-public-keys)"
[ "$keys" = cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY= ] || { echo "::error::trusted-public-keys are '$keys'"; exit 1; }

{
  echo "### build-darwin: runner"
  echo
  echo "| | |"
  echo "|---|---|"
  echo "| uname -m | $arch |"
  echo "| Nix system | $system |"
  echo "| OS | $(sw_vers -productName) $(sw_vers -productVersion) ($(sw_vers -buildVersion)) |"
  echo "| Nix | $(nix --version) |"
  echo "| CPUs | $(sysctl -n hw.ncpu) |"
  echo "| memory | $(( $(sysctl -n hw.memsize) / 1048576 )) MiB |"
  echo "| free disk (/) | $(df -h / | awk 'NR==2 {print $4}') |"
  echo "| sandbox | $sandbox |"
  echo "| substituters | $substituters |"
} | summary
