#!/usr/bin/env bash
# The checks of `verify-cache-darwin` (.github/workflows/nix.yml): what
# `publish-darwin` pushed to attic is fetched on another macOS runner into an
# empty store, checked, installed into the Nix daemon of the runner and run.
# The same script runs, against a local cache, in the pull-request rehearsal
# (nix/darwin-ci-rehearsal.sh), so every command here is tried before a merge.
#
#   nix/run-pinned.sh nix/darwin-ci-verify-cache.sh handover
#   nix/run-pinned.sh nix/darwin-ci-verify-cache.sh fetch     (needs ATTIC_TOKEN)
#   nix/run-pinned.sh nix/darwin-ci-verify-cache.sh daemon    (no credential)
#
# handover  checks the values the publishing job handed over and this machine.
# fetch     substitutes the closures of the two roots, by their absolute store
#           paths only (no flake, no derivation, no build), into an empty
#           `local?root=` store, from attic and cache.nixos.org alone, each
#           trusted by its own key alone; checks that the closure is complete,
#           intact and exactly the package and the allowed outputs; classifies
#           every path by the narinfo of cache.nixos.org (200: upstream, 404:
#           attic's, anything else: failure), requires both roots to be attic's
#           and attic to have every one of its paths; and shows that a made-up
#           path and a path that only cache.nixos.org has are not served. The
#           credential (a netrc file) is removed when it ends.
# daemon    runs without the credential. It checks that no path of attic's is in
#           the daemon yet, that `nix copy --no-recursive` of the package to
#           the daemon trusting only the upstream key fails for the missing
#           signature and for no other reason, and that nothing was registered
#           by it; then copies both closures with both keys, with
#           `require-sigs true` and no substituter (neither --no-check-sigs nor
#           --substitute-on-destination), compares NAR hashes and references of
#           both stores, and runs the package from the daemon's store with an
#           empty environment and a new HOME and CARGO_HOME, and checks its
#           version, release, commit and Mach-O linkage again.
#
# Settings (the publishing job's outputs and the cache's constants):
#   VC_SYSTEM VC_COMMIT VC_RELEASE VC_CARGO VC_DEPS VC_ALLOWED VC_MINOS VC_MACHO
#   ATTIC_URL ATTIC_HOST ATTIC_KEY UPSTREAM_URL UPSTREAM_KEY
#   VC_WORK   scratch directory (default $RUNNER_TEMP/verify-cache)
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=nix/pinned-env.sh
source "$here/pinned-env.sh"
# shellcheck source=nix/darwin-ci-lib.sh
source "$here/darwin-ci-lib.sh"

mode="${1:?usage: darwin-ci-verify-cache.sh handover|fetch|daemon}"

: "${VC_SYSTEM:?}" "${VC_COMMIT:?}" "${VC_RELEASE:?}" "${VC_CARGO:?}" "${VC_DEPS:?}"
: "${VC_ALLOWED:?}" "${VC_MINOS:?}" "${VC_MACHO:?}"
: "${ATTIC_URL:?}" "${ATTIC_HOST:?}" "${ATTIC_KEY:?}" "${UPSTREAM_URL:?}" "${UPSTREAM_KEY:?}"
work="${VC_WORK:-${RUNNER_TEMP:?RUNNER_TEMP}/verify-cache}"
store="$work/store"
store_uri="local?root=$store"
roots=("$VC_CARGO" "$VC_DEPS")
netrc="$work/netrc"

macho_get() {
  jq -r "$1" <<< "$VC_MACHO"
}

validate_inputs() {
  [ "$VC_SYSTEM" = aarch64-darwin ] || fail_ci "unexpected system: $VC_SYSTEM"
  [[ "$VC_COMMIT" =~ ^[0-9a-f]{40}$ ]] || fail_ci "unexpected commit: $VC_COMMIT"
  [[ "$VC_RELEASE" =~ ^[0-9A-Za-z.+-]+$ ]] || fail_ci "unexpected release: $VC_RELEASE"
  [[ "$VC_CARGO" =~ $store_path_re ]] || fail_ci "unexpected store path: $VC_CARGO"
  [[ "$VC_DEPS" =~ $store_path_re ]] || fail_ci "unexpected store path: $VC_DEPS"
  [[ "$VC_CARGO" == /nix/store/*-turtton-cargo-"$VC_RELEASE" ]] || fail_ci "$VC_CARGO is not turtton-cargo-$VC_RELEASE"
  [[ "$VC_DEPS" == /nix/store/*-turtton-cargo-deps-"$VC_RELEASE" ]] || fail_ci "$VC_DEPS is not turtton-cargo-deps-$VC_RELEASE"
  [ "$VC_CARGO" != "$VC_DEPS" ] || fail_ci "the two roots are one path"
  local p seen=""
  for p in $VC_ALLOWED; do
    [[ "$p" =~ $store_path_re ]] || fail_ci "unexpected allowed store path: $p"
    case "$p" in "$VC_CARGO" | "$VC_DEPS") fail_ci "a root is in the allowed paths: $p" ;; esac
    case " $seen " in *" $p "*) fail_ci "allowed twice: $p" ;; esac
    seen="$seen $p"
  done
  [[ "$VC_MINOS" =~ ^[0-9]+\.[0-9]+$ ]] || fail_ci "unexpected deployment target: $VC_MINOS"
  jq -e --arg minos "$VC_MINOS" '
    .minos == $minos
    and (.systemLibs | length > 0) and (.transitiveSystemLibs | length > 0)
    and (.requiredDeps | length > 0) and (.forbiddenDeps | length > 0)
  ' <<< "$VC_MACHO" > /dev/null || fail_ci "the Mach-O allow lists are not usable or disagree with the deployment target"
}

# The state of the closure that `fetch` leaves for `daemon`.
closure_file="$work/closure.txt"

case "$mode" in
  handover)
    validate_inputs
    echo "== this machine"
    [ "$(uname -s)" = Darwin ] || fail_ci "the runner is $(uname -s), not Darwin"
    [ "$(uname -m)" = arm64 ] || fail_ci "the runner is $(uname -m), not arm64"
    nix_system="$(nix eval --raw --impure --expr builtins.currentSystem)"
    [ "$nix_system" = "$VC_SYSTEM" ] || fail_ci "the Nix system is $nix_system, the package is for $VC_SYSTEM"
    echo "== the checkout is the commit that was built"
    [ "${GITHUB_SHA:-}" = "$VC_COMMIT" ] || fail_ci "this run is at ${GITHUB_SHA:-unknown}, the package was built from $VC_COMMIT"
    [ "$(cat TURTTON_VERSION)" = "$VC_RELEASE" ] || fail_ci "TURTTON_VERSION is not $VC_RELEASE"
    if [ "${GITHUB_REF_TYPE:-}" = tag ]; then
      [ "${GITHUB_REF_NAME:-}" = "v$VC_RELEASE" ] || fail_ci "the tag ${GITHUB_REF_NAME:-} is not v$VC_RELEASE"
    fi
    echo "system: $VC_SYSTEM"
    echo "commit: $VC_COMMIT"
    echo "release: $VC_RELEASE"
    echo "cargo: $VC_CARGO"
    echo "cargo-deps: $VC_DEPS"
    echo "allowed: $VC_ALLOWED"
    echo "deployment target: $VC_MINOS"
    ;;

  fetch)
    : "${ATTIC_TOKEN:?the attic credential}"
    validate_inputs
    mkdir -p "$work"
    [ ! -e "$store" ] || fail_ci "$store exists: the store of this job is not empty"
    trap 'rm -f "$netrc"' EXIT
    # The token is only in this file, read by this script's nix commands.
    (umask 077; printf 'machine %s password %s\n' "$ATTIC_HOST" "$ATTIC_TOKEN" > "$netrc")
    unset ATTIC_TOKEN
    mkdir -p "$store"
    both_opts=(
      --option netrc-file "$netrc"
      --option trusted-public-keys "$ATTIC_KEY $UPSTREAM_KEY"
      --option substituters "$ATTIC_URL $UPSTREAM_URL"
    )
    attic_opts=(--option netrc-file "$netrc" --option trusted-public-keys "$ATTIC_KEY" --option substituters "$ATTIC_URL")

    echo "== the verify store starts empty"
    assert_store_empty "$store_uri"

    # The log names the cache of every path. The store has no derivation, so a
    # path that neither cache serves cannot be built instead (and builds are
    # not allowed).
    echo "== substitute the closures from attic and cache.nixos.org only"
    nix build -L --no-link --store "$store_uri" --max-jobs 0 "${both_opts[@]}" "${roots[@]}" 2>&1 | tee "$work/substitute.log"
    no_ignored_setting "$work/substitute.log"

    echo "== every path of the closures is there and its content is intact"
    nix path-info --store "$store_uri" --recursive "${roots[@]}" | sort -u | tee "$closure_file"
    [ -s "$closure_file" ] || fail_ci "the verify store lists no path for the roots"
    wc -l < "$closure_file"
    while read -r p; do
      test -e "$store$p" || fail_ci "missing from the verify store: $p"
    done < "$closure_file"
    nix store verify --store "$store_uri" --no-trust --recursive "${roots[@]}"
    echo "== nothing but those closures is in the store"
    nix path-info --all --store "$store_uri" | sort -u > "$work/all.txt"
    [ -s "$work/all.txt" ] || fail_ci "the verify store lists no path"
    cmp "$closure_file" "$work/all.txt" || fail_ci "the store holds paths outside the closures"
    echo "== the closure of the package is the package and the allowed outputs"
    nix path-info --store "$store_uri" --recursive "$VC_CARGO" | sort -u > "$work/cargo-closure.txt"
    # The allowed paths are validated store paths, split on purpose.
    # shellcheck disable=SC2086
    { printf '%s\n' "$VC_CARGO"; printf '%s\n' $VC_ALLOWED; } | sort -u > "$work/expected-cargo-closure.txt"
    cmp "$work/expected-cargo-closure.txt" "$work/cargo-closure.txt" || fail_ci "the closure of the package is not the package and the allowed outputs"
    for p in $VC_ALLOWED; do
      grep -qxF "$p" "$closure_file" || fail_ci "allowed output missing from the closure: $p"
    done

    # Substituting from both caches cannot tell where a path came from. What
    # cache.nixos.org does not have only attic can have served.
    echo "== which paths of the closures cache.nixos.org has"
    classify_closure "$closure_file" "$work"
    echo "cache.nixos.org has $(wc -l < "$work/upstream.txt") paths"
    echo "only attic can have $(wc -l < "$work/fork.txt") paths:"
    cat "$work/fork.txt"
    for p in "${roots[@]}"; do
      grep -qxF "$p" "$work/fork.txt" || fail_ci "$p is not attic's: $UPSTREAM_URL answers for it"
    done
    echo "== attic has every one of them"
    mapfile -t fork < "$work/fork.txt"
    nix path-info --store "$ATTIC_URL" --option netrc-file "$netrc" "${fork[@]}" > /dev/null
    echo "== attic's paths carry a signature of attic, the others one of cache.nixos.org"
    nix store verify --store "$store_uri" --no-contents --option trusted-public-keys "$ATTIC_KEY" "${fork[@]}"
    mapfile -t upstream < "$work/upstream.txt"
    if [ "${#upstream[@]}" -gt 0 ]; then
      nix store verify --store "$store_uri" --no-contents --option trusted-public-keys "$UPSTREAM_KEY" "${upstream[@]}"
    fi

    # A check that cannot fail proves nothing. Each path below must fail to be
    # fetched, with the same commands as above (both caches, and attic alone):
    #   - a path that neither cache has;
    #   - a path that only cache.nixos.org has: attic alone must not serve it.
    #     This is what the attic client's skipping of upstream paths means, and
    #     why the check above needs cache.nixos.org at all.
    fail_to_copy() {
      local dest="$work/negative-store-$1" log="$work/negative-$1.log"
      mkdir -p "$dest"
      if nix copy --from "$ATTIC_URL" --to "local?root=$dest" "${attic_opts[@]}" "$2" > "$log" 2>&1; then
        cat "$log"
        fail_ci "attic served $2, which it must not have"
      fi
      cat "$log"
      no_ignored_setting "$log"
      expect_missing "$log" "$2"
      echo "as expected, attic does not serve $2"
    }
    fail_to_substitute() {
      local dest="$work/negative-store-$1" log="$work/negative-$1.log"
      mkdir -p "$dest"
      if nix build --no-link --store "local?root=$dest" --max-jobs 0 "${both_opts[@]}" "$2" > "$log" 2>&1; then
        cat "$log"
        fail_ci "a substituter served $2, which it must not have"
      fi
      cat "$log"
      no_ignored_setting "$log"
      expect_missing "$log" "$2"
      echo "as expected, neither cache serves $2"
    }
    fake=/nix/store/00000000000000000000000000000000-turtton-cargo-missing
    echo "== a store path with a made-up hash, from attic alone"
    fail_to_copy fake "$fake"
    echo "== the same path, from attic and cache.nixos.org"
    fail_to_substitute fake-both "$fake"
    echo "== a path of cache.nixos.org that attic does not have"
    [ "${#upstream[@]}" -gt 0 ] || fail_ci "no path of the closure comes from $UPSTREAM_URL"
    fail_to_copy upstream-only "${upstream[0]}"

    rm -f "$netrc"
    {
      echo "### verify-cache-darwin: fetch"
      echo
      echo "- commit \`$VC_COMMIT\`, release \`$VC_RELEASE\`, deployment target $VC_MINOS"
      echo "- closure: $(wc -l < "$closure_file") paths; $(wc -l < "$work/fork.txt") from attic, $(wc -l < "$work/upstream.txt") from cache.nixos.org"
      echo "- cargo \`$VC_CARGO\` and cargo-deps \`$VC_DEPS\` are attic's"
      echo "- a made-up path and the upstream-only path \`${upstream[0]}\` are not served by attic"
    } | summary
    ;;

  daemon)
    validate_inputs
    [ ! -e "$netrc" ] || fail_ci "the attic credential is still there: $netrc"
    [ -s "$closure_file" ] || fail_ci "no closure listed: the fetch step did not run"
    [ -s "$work/fork.txt" ] || fail_ci "no attic path listed: the fetch step did not run"
    mapfile -t fork < "$work/fork.txt"
    store_args=(--store "$store_uri")

    none_in_daemon() {
      local p
      for p in "${fork[@]}"; do
        if in_daemon "$p"; then
          fail_ci "the daemon has $p ($1)"
        fi
      done
    }

    echo "== no path of attic's is in the daemon yet"
    none_in_daemon "before any copy"

    echo "== copy the package with only the upstream key trusted: must fail for the missing signature"
    if nix copy --no-recursive --from "$store_uri" --to daemon \
      --option require-sigs true --option substituters "" \
      --option trusted-public-keys "$UPSTREAM_KEY" "$VC_CARGO" > "$work/negative-copy.log" 2>&1; then
      cat "$work/negative-copy.log"
      fail_ci "the copy succeeded without a signature of a trusted key"
    fi
    cat "$work/negative-copy.log"
    no_ignored_setting "$work/negative-copy.log"
    grep -Eiq 'lacks a (valid )?signature' "$work/negative-copy.log" \
      || fail_ci "the copy failed, but not for a missing signature"
    echo "== it registered nothing"
    none_in_daemon "after the failed copy"

    echo "== copy both closures trusting the keys of attic and cache.nixos.org"
    nix copy --from "$store_uri" --to daemon \
      --option require-sigs true --option substituters "" \
      --option trusted-public-keys "$ATTIC_KEY $UPSTREAM_KEY" "${roots[@]}" 2>&1 | tee "$work/copy.log"
    no_ignored_setting "$work/copy.log"
    while read -r p; do
      in_daemon "$p" || fail_ci "the daemon lacks $p after the copy"
    done < "$closure_file"

    echo "== NAR hashes and references are the same in both stores"
    describe_store "$work/describe-verify.txt" "$closure_file" "${store_args[@]}"
    describe_store "$work/describe-daemon.txt" "$closure_file"
    cat "$work/describe-daemon.txt"
    compare_descriptions "$work/describe-verify.txt" "$work/describe-daemon.txt"
    echo "== the daemon's copies are intact, and attic's carry attic's signature"
    nix store verify --no-trust --recursive "${roots[@]}"
    nix store verify --no-contents --option trusted-public-keys "$ATTIC_KEY" "${fork[@]}"

    echo "== run the package from the daemon's store with an empty environment"
    home="$(mktemp -d "$work/home.XXXXXX")"
    chmod 700 "$home"
    env -i HOME="$home" CARGO_HOME="$home/.cargo" "$VC_CARGO/bin/cargo" -V | tee "$work/version.txt"
    grep -F "turtton/cargo $VC_RELEASE," "$work/version.txt"
    ! grep -q huacnlee "$work/version.txt"
    commit_short="${VC_COMMIT:0:9}"
    grep -Eq "\\(${commit_short} [0-9]{4}-[0-9]{2}-[0-9]{2}\\)" "$work/version.txt" \
      || fail_ci "cargo -V does not show the commit $commit_short"

    echo "== the Mach-O check and the store references, once more, on the daemon's copy"
    # shellcheck disable=SC2086
    printf '%s\n' $VC_ALLOWED > "$work/allowed-paths.txt"
    (
      MACHO_ALLOWED_SYSTEM_LIBS="$(macho_get '.systemLibs | join(" ")')"
      MACHO_TRANSITIVE_SYSTEM_LIBS="$(macho_get '.transitiveSystemLibs | join(" ")')"
      MACHO_REQUIRED_DEPS="$(macho_get '.requiredDeps | join(" ")')"
      MACHO_FORBIDDEN_DEPS="$(macho_get '.forbiddenDeps | join(" ")')"
      MACHO_ALLOWED_CLOSURE="$work/allowed-paths.txt"
      MACHO_SELF_OUT="$VC_CARGO"
      MACHO_MINOS="$VC_MINOS"
      export MACHO_ALLOWED_SYSTEM_LIBS MACHO_TRANSITIVE_SYSTEM_LIBS MACHO_REQUIRED_DEPS MACHO_FORBIDDEN_DEPS
      export MACHO_ALLOWED_CLOSURE MACHO_SELF_OUT MACHO_MINOS
      bash "$here/check-macho.sh" "$VC_CARGO/bin/cargo"
    ) 2>&1 | tee "$work/macho.log"
    {
      echo "### verify-cache-darwin: daemon"
      echo
      echo "- copy trusting only the upstream key: failed for the missing signature, registered nothing"
      echo "- copy trusting both keys, \`require-sigs true\`, no substituter: ok; NAR hashes and references equal"
      echo "- \`env -i\` run of the daemon's cargo: \`$(head -n 1 "$work/version.txt")\`"
      echo "- Mach-O check on the daemon's copy: ok"
    } | summary
    ;;

  *)
    fail_ci "unknown mode: $mode"
    ;;
esac
