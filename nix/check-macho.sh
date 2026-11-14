#!/usr/bin/env bash
# Inspects the Mach-O executable of a Darwin build. Used by the install check
# of the Nix package (flake.nix) and, on fixtures, by `checks.<darwin>.macho-inspection`,
# which proves that this script fails when it must.
#
#   nix/check-macho.sh path/to/binary
#
# It checks that the binary
#   - is a thin arm64 Mach-O,
#   - targets exactly macOS $MACHO_MINOS (LC_BUILD_VERSION minos),
#   - links dynamically only to the system libraries named in full in
#     $MACHO_ALLOWED_SYSTEM_LIBS and
#     to the store outputs whose closure is listed in $MACHO_ALLOWED_CLOSURE,
#     and so do all those store libraries, transitively,
#   - has LC_RPATH entries only in those places, and every @rpath/ dependency
#     resolves inside them,
#   - names no other store path (the crane placeholder eeee... and $MACHO_SELF_OUT
#     are told apart from the allowed outputs), and none of the toolchain or SDK.
# Anything it cannot parse, and an empty result, is a failure. Failures are
# reported as `macho-check: <tag>: <message>` on stderr and exit status 1.
#
#   MACHO_ALLOWED_SYSTEM_LIBS space-separated install names of the system libraries
#                             the binary may load, each in full
#   MACHO_TRANSITIVE_SYSTEM_LIBS same for the libraries of the store outputs
#                             (default: MACHO_ALLOWED_SYSTEM_LIBS)
#
# A path is never normalized by the check, so none that contains a `.` or `..`
# component, or an empty one (`//`), is accepted anywhere: it could name a place
# outside the one a string comparison saw.
#   MACHO_ALLOWED_CLOSURE     file listing the allowed store paths (closure)
#   MACHO_SELF_OUT            the store path the binary is installed to
#   MACHO_MINOS               the deployment target, e.g. 14.0
#   MACHO_REQUIRED_DEPS       optional, space-separated EREs; each must match
#                             a direct dependency
#   MACHO_FORBIDDEN_DEPS      optional, space-separated EREs; none may match
#   MACHO_DENIED_NAMES        ERE of store path names that are never allowed
#   MACHO_VERBOSE             when set, print the whole `otool -l`
set -euo pipefail

fail() {
  echo "macho-check: $1: $2" >&2
  exit 1
}

bin="${1:?usage: check-macho.sh BINARY}"
: "${MACHO_ALLOWED_SYSTEM_LIBS:?}"
: "${MACHO_ALLOWED_CLOSURE:?}"
: "${MACHO_SELF_OUT:?}"
: "${MACHO_MINOS:?}"
transitive_libs="${MACHO_TRANSITIVE_SYSTEM_LIBS:-$MACHO_ALLOWED_SYSTEM_LIBS}"
required="${MACHO_REQUIRED_DEPS:-}"
forbidden="${MACHO_FORBIDDEN_DEPS:-}"
denied="${MACHO_DENIED_NAMES:-apple-sdk|clang|llvm|cctools|cc-wrapper|bintools|stdenv|xcbuild|bootstrap|rustc|rust-1|crane}"

command -v otool > /dev/null || fail tool "otool is not available"
[ -f "$bin" ] || fail not-macho "$bin is not a file"
[ -s "$MACHO_ALLOWED_CLOSURE" ] || fail tool "the allowed closure list $MACHO_ALLOWED_CLOSURE is empty"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

in_closure() {
  grep -qxF -- "$1" "$MACHO_ALLOWED_CLOSURE"
}

store_root_of() {
  printf '%s\n' "$1" | sed -E 's#^(/nix/store/[^/]+).*#\1#'
}

# Succeeds when $1 is exactly one of the words of $2.
is_listed() {
  local path="$1" libs="$2" l
  for l in $libs; do
    [ "$path" = "$l" ] && return 0
  done
  return 1
}

# Fails for a path with a `.` or `..` component or an empty one.
is_plain_path() {
  case "$1" in
    *//* | */./* | */. | */../* | */..) return 1 ;;
  esac
  return 0
}

# Mach-O thin arm64 header of $1.
check_arch() {
  local f="$1" hdr n
  hdr="$(otool -hv "$f" 2>&1)" || fail not-macho "otool -hv failed on $f: $hdr"
  case "$hdr" in
    *"Fat headers"*) fail arch "$f is a fat binary" ;;
  esac
  n="$(printf '%s\n' "$hdr" | grep -cE 'MH_MAGIC_64[[:space:]]+ARM64([[:space:]]|$)' || true)"
  [ "$n" = 1 ] || fail arch "$f is not a single arm64 Mach-O: $hdr"
}

# LC_RPATH entries of the file whose `otool -l` output is $1, one per line.
rpaths_in() {
  printf '%s\n' "$1" | awk '
    /^[[:space:]]*cmd LC_RPATH/ { want = 1; next }
    want && $1 == "path" {
      sub(/^[[:space:]]*path /, "")
      sub(/ \(offset [0-9]+\)$/, "")
      print
      want = 0
    }'
}

# Dependencies of $1 (its own install name excluded), one per line.
deps_of() {
  local f="$1" out lc
  out="$(otool -L "$f" 2>&1)" || fail not-macho "otool -L failed on $f: $out"
  lc="$(otool -l "$f" 2>&1)" || fail not-macho "otool -l failed on $f: $lc"
  # `grep -q` in a pipeline could kill the writer of a long `otool -l` output.
  case "$lc" in
    *"cmd LC_ID_DYLIB"*) printf '%s\n' "$out" | tail -n +3 ;;
    *) printf '%s\n' "$out" | tail -n +2 ;;
  esac | sed -E 's/^[[:space:]]+//; s/ \(compatibility version.*$//' | grep -v '^$' || true
}

# Checks the rpaths of $1 ($2 is its `otool -l`): only directories of the
# allowed store outputs.
check_rpaths() {
  local f="$1" lc="$2" r
  rpaths_in "$lc" | while IFS= read -r r; do
    is_plain_path "$r" || fail rpath "$f has an LC_RPATH with a . or .. component: $r"
    case "$r" in
      /nix/store/*)
        in_closure "$(store_root_of "$r")" || fail rpath "$f has an LC_RPATH outside the allowed outputs: $r"
        ;;
      *) fail rpath "$f has an LC_RPATH that is not in an allowed output: $r" ;;
    esac
  done
}

# Checks dependency $2 of file $1 ($3 is the file's `otool -l`) against the
# system libraries $4. A dependency in the store prints its path on stdout so that the
# caller can inspect it in turn.
check_dep() {
  local f="$1" dep="$2" lc="$3" syslibs="$4" r rest cand found=""
  is_plain_path "$dep" || fail dep-disallowed "$f: dependency $dep has a . or .. component"
  case "$dep" in
    @rpath/*)
      rest="${dep#@rpath/}"
      for r in $(rpaths_in "$lc"); do
        cand="$r/$rest"
        case "$r" in
          /nix/store/*)
            if [ -e "$cand" ] && in_closure "$(store_root_of "$cand")"; then
              found="$cand"
              break
            fi
            ;;
        esac
      done
      [ -n "$found" ] || fail dep-disallowed "$f: @rpath dependency $dep does not resolve inside the allowed outputs"
      printf '%s\n' "$found"
      ;;
    @*) fail dep-disallowed "$f: dependency $dep is relative to the loader or the executable" ;;
    /nix/store/*)
      in_closure "$(store_root_of "$dep")" || fail dep-disallowed "$f: dependency $dep is a store path outside the allowed outputs"
      printf '%s\n' "$dep"
      ;;
    /*)
      is_listed "$dep" "$syslibs" || fail dep-disallowed "$f: dependency $dep is not an allowed system library"
      ;;
    *) fail dep-disallowed "$f: dependency $dep is not an absolute path" ;;
  esac
}

echo "== otool -L $bin"
otool -L "$bin"
lc="$(otool -l "$bin" 2>&1)" || fail not-macho "otool -l failed on $bin: $lc"
echo "== load commands of $bin that matter (LC_RPATH, LC_BUILD_VERSION, LC_VERSION_MIN_MACOSX)"
printf '%s\n' "$lc" | grep -E -A5 'cmd (LC_RPATH|LC_BUILD_VERSION|LC_VERSION_MIN_MACOSX)' || true
if [ -n "${MACHO_VERBOSE:-}" ]; then
  echo "== otool -l $bin"
  printf '%s\n' "$lc"
fi

echo "== architecture"
check_arch "$bin"

echo "== deployment target $MACHO_MINOS"
minos="$(printf '%s\n' "$lc" | awk '
  /cmd LC_BUILD_VERSION/ { b = 1; platform = ""; next }
  b && $1 == "platform" { platform = $2 }
  b && $1 == "minos" { print platform, $2; b = 0 }
  /cmd LC_VERSION_MIN_MACOSX/ { m = 1; next }
  m && $1 == "version" { print "MACOS", $2; m = 0 }')"
case "$minos" in
  "MACOS $MACHO_MINOS" | "1 $MACHO_MINOS") echo "minos: $minos" ;;
  *) fail minos "expected one macOS build version $MACHO_MINOS, got: ${minos:-nothing}" ;;
esac

echo "== runtime libraries"
deps="$(deps_of "$bin")"
[ -n "$deps" ] || fail deps-empty "$bin has no dependency (libSystem is always one): the analysis found nothing"
printf '%s\n' "$deps" | sed 's/^/dep: /'
for re in $required; do
  printf '%s\n' "$deps" | grep -Eq -- "$re" || fail missing-required "no direct dependency matches $re"
done
for re in $forbidden; do
  if printf '%s\n' "$deps" | grep -Eq -- "$re"; then
    fail forbidden-dep "a direct dependency matches $re"
  fi
done
check_rpaths "$bin" "$lc"
: > "$tmp/work"
: > "$tmp/seen"
queue() {
  grep -qxF -- "$1" "$tmp/seen" || {
    printf '%s\n' "$1" >> "$tmp/seen"
    printf '%s\n' "$1" >> "$tmp/work"
  }
}
while IFS= read -r dep; do
  resolved="$(check_dep "$bin" "$dep" "$lc" "$MACHO_ALLOWED_SYSTEM_LIBS")"
  if [ -n "$resolved" ]; then
    queue "$resolved"
  fi
done <<EOF
$deps
EOF

echo "== libraries of the allowed outputs, transitively"
while :; do
  f="$(head -n 1 "$tmp/work")"
  [ -n "$f" ] || break
  tail -n +2 "$tmp/work" > "$tmp/work.next"
  mv "$tmp/work.next" "$tmp/work"
  [ -e "$f" ] || fail dep-disallowed "$f does not exist, it cannot be inspected"
  echo "-- $f"
  check_arch "$f"
  flc="$(otool -l "$f" 2>&1)" || fail not-macho "otool -l failed on $f: $flc"
  fdeps="$(deps_of "$f")"
  [ -n "$fdeps" ] || fail deps-empty "$f has no dependency: the analysis found nothing"
  printf '%s\n' "$fdeps" | sed 's/^/   dep: /'
  check_rpaths "$f" "$flc"
  while IFS= read -r dep; do
    resolved="$(check_dep "$f" "$dep" "$flc" "$transitive_libs")"
    if [ -n "$resolved" ]; then
      queue "$resolved"
    fi
  done <<EOF
$fdeps
EOF
done
echo "inspected $(wc -l < "$tmp/seen" | tr -d ' ') libraries of the allowed outputs"

echo "== store paths named in the binary"
rc=0
refs="$(grep -aoE '/nix/store/[a-z0-9]{32}-[^/"[:space:]:]+' "$bin")" || rc=$?
[ "$rc" -le 1 ] || fail tool "grep failed on $bin (status $rc)"
refs="$(printf '%s\n' "$refs" | sort -u | grep -v '^$' || true)"
printf '%s\n' "$refs"
while IFS= read -r path; do
  [ -n "$path" ] || continue
  case "$path" in
    "$MACHO_SELF_OUT") continue ;;
    # crane overwrites the hash of the vendored sources (and the toolchain) in
    # the binary with e's: not a reference.
    /nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-*) continue ;;
  esac
  name="${path#/nix/store/}"
  name="${name#*-}"
  if printf '%s\n' "$name" | grep -Eq -- "$denied"; then
    fail store-ref "the binary names the toolchain or SDK path $path"
  fi
  in_closure "$path" || fail store-ref "the binary names the store path $path, which is not an allowed output"
done <<EOF
$refs
EOF

echo "macho-check: OK"
