#!/usr/bin/env bash
# Runs a release binary the way users have it: installed as
# `$CARGO_HOME/bin/cargo`, outside CI, and checks that it does NOT update
# itself.
#
#   ci/smoke-installed.sh path/to/cargo
#
# turtton/cargo is distributed through Nix only. The background self-update of
# the upstream fork (huacnlee/cargo) was removed, so even with every switch
# that used to start it, cargo must
#   - name the turtton release in `cargo -V`,
#   - return at once and print the version (also when the old internal child
#     variable is set, which used to run an update instead of the command),
#   - write no `.fork-update*` file under `$CARGO_HOME`,
#   - never connect to the proxy that stands in for GitHub, and
#   - carry no reference to the huacnlee release endpoints.
set -euo pipefail

bin="$1"
python="$(command -v python3 || command -v python)"

home="$(mktemp -d)"
mkdir -p "$home/bin"
cp "$bin" "$home/bin/cargo"
cargo="$home/bin/cargo"

run() {
    env -u CI -u GITHUB_ACTIONS CARGO_HOME="$home" \
        CARGO_FORK_AUTO_UPDATE=1 \
        https_proxy="$proxy" HTTPS_PROXY="$proxy" http_proxy="$proxy" HTTP_PROXY="$proxy" ALL_PROXY="$proxy" \
        "$@"
}

# A listener that records every connection made to it. If cargo tried to reach
# GitHub through it, a hit would be written to `$home/proxy-hits`.
"$python" - "$home" <<'EOF' &
import socket, sys
home = sys.argv[1]
s = socket.socket()
s.bind(("127.0.0.1", 0))
s.listen(16)
with open(home + "/proxy-port", "w") as f:
    f.write(str(s.getsockname()[1]))
conns = []
while True:
    c = s.accept()[0]
    conns.append(c)
    with open(home + "/proxy-hits", "a") as f:
        f.write("hit\n")
EOF
listener=$!
trap 'kill $listener 2>/dev/null || true; rm -rf "$home"' EXIT
for _ in $(seq 50); do [ -s "$home/proxy-port" ] && break; sleep 0.1; done
proxy="http://127.0.0.1:$(cat "$home/proxy-port")"

check_version() {
    local out="$1" elapsed="$2"
    echo "cargo -V: $out (${elapsed}s)"
    case "$out" in
        cargo\ *"(turtton/cargo 0."*"turtton"*", https://github.com/turtton/cargo)") ;;
        *) echo "cargo -V does not name the turtton release: $out" >&2; exit 1 ;;
    esac
    case "$out" in *huacnlee*) echo "cargo -V mentions huacnlee: $out" >&2; exit 1 ;; esac
    if [ "$elapsed" -gt 10 ]; then
        echo "cargo -V took ${elapsed}s" >&2
        exit 1
    fi
}

# Every switch that used to start the updater: the installed path, no CI,
# CARGO_FORK_AUTO_UPDATE=1 (above) and a proxy. Several runs, because the old
# updater was rate limited by a stamp file and fired only on the first.
for _ in 1 2 3; do
    start=$SECONDS
    out="$(run "$cargo" -V)"
    check_version "$out" $((SECONDS - start))
done

# The old detached child ran an update instead of the command and printed
# nothing. It is plain cargo now.
start=$SECONDS
out="$(run __CARGO_FORK_SELF_UPDATE="$home" "$cargo" -V)"
check_version "$out" $((SECONDS - start))

# Leave time for a detached process to start, as the old updater would.
sleep 3
for name in .fork-update .fork-update.log; do
    if [ -e "$home/$name" ]; then
        echo "cargo wrote $home/$name: it tried to update itself" >&2
        exit 1
    fi
done
if [ -s "$home/proxy-hits" ]; then
    echo "cargo connected to the proxy: it tried to reach a release server" >&2
    exit 1
fi
if grep -aq 'huacnlee/cargo' "$cargo"; then
    echo "the binary still refers to huacnlee/cargo" >&2
    exit 1
fi
echo "no self-update"

# A `rustup` that is really this cargo, as older install.ps1 left behind, must
# not make it ask itself for the toolchain forever.
cp "$bin" "$home/bin/rustup"
run PATH="$home/bin:$PATH" "$cargo" -V >"$home/out" &
pid=$!
for _ in $(seq 10); do kill -0 $pid 2>/dev/null || break; sleep 1; done
if kill -0 $pid 2>/dev/null; then
    echo "cargo -V keeps starting itself when rustup is cargo" >&2
    for _ in 1 2 3 4 5; do
        pkill -f "$home/bin/rustup" || true
    done >/dev/null 2>&1
    exit 1
fi
wait $pid
echo "cargo -V with cargo as rustup: $(cat "$home/out")"
