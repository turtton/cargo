{
  description = "turtton/cargo: a Cargo fork that shares build output across worktrees, built with Nix";

  # The attic cache CI pushes to (see .github/workflows/nix.yml). Nix asks
  # before trusting a flake's nixConfig; without it everything is built here.
  nixConfig = {
    extra-substituters = [ "https://attic.taile2777.ts.net/home" ];
    extra-trusted-public-keys = [ "home:00byWMpTTw/3xTntv8EF6LQmlWlR9RU5Tl0GVG5Vwn8=" ];
  };

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  inputs.fenix = {
    url = "github:nix-community/fenix";
    inputs.nixpkgs.follows = "nixpkgs";
  };
  inputs.crane.url = "github:ipetkov/crane";

  outputs =
    {
      self,
      nixpkgs,
      fenix,
      crane,
    }:
    let
      # "aarch64-linux" could be added here, but only after the Linux install
      # check's allow list is made per system: it names x86_64 glibc sonames.
      # aarch64-darwin has its own Mach-O check (nix/check-macho.sh) and allow
      # list. x86_64-darwin is not built.
      systems = [
        "x86_64-linux"
        "aarch64-darwin"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f system nixpkgs.legacyPackages.${system});

      # The release shown by `cargo -V`. build.rs reads the same file when
      # cargo compiles, so Nix and plain `cargo build` cannot disagree.
      release = nixpkgs.lib.strings.removeSuffix "\n" (builtins.readFile ./TURTTON_VERSION);

      perSystem =
        system: pkgs:
        let
          lib = pkgs.lib;
          isDarwin = pkgs.stdenv.hostPlatform.isDarwin;

          # Rust 1.99.0, the toolchain the fork targets (rust-version 1.99).
          # The hash is that of the channel manifest, so the compiler cannot
          # change under the same lock. It is the manifest of evorch's
          # toolchain file, which names the same channel. The driver of every
          # build here is this upstream cargo, never the fork itself.
          rustToolchain =
            (fenix.packages.${system}.toolchainOf {
              channel = "1.99.0";
              sha256 = "sha256-zm3dyIY2T414ZRR3EhLOvptzG6gta4WZUcawzMUWtqI=";
            }).minimalToolchain;
          craneLib = (crane.mkLib pkgs).overrideToolchain (_p: rustToolchain);

          # The whole repository except what the build never reads, so that
          # editing a workflow, a test or this flake does not rebuild cargo.
          # `tests/` also holds hundreds of fixture manifests (some invalid on
          # purpose) that crane would parse for the dependency build. The deps
          # derivation only looks at manifests and Cargo.lock either way.
          src = lib.cleanSourceWith {
            src = lib.cleanSource ./.;
            filter =
              path: _type:
              let
                rel = lib.removePrefix "${toString ./.}/" (toString path);
                top = builtins.head (lib.splitString "/" rel);
              in
              !(builtins.elem top [
                ".github"
                "tests"
                "website"
                "flake.nix"
                "flake.lock"
                # The Mach-O check and its fixtures: used by the install check
                # and the checks, never by the build.
                "nix"
              ]);
          };

          commonArgs = {
            pname = "turtton-cargo";
            version = release;
            inherit src;
            strictDeps = true;
            # Same features as the former release build: vendored OpenSSL and
            # libgit2, static curl; zlib-rs and bundled SQLite come from the
            # dependency features. Keep every input here, not only in
            # buildPackage: build scripts of the -sys crates track the
            # environment, and the deps must be reusable by the package.
            cargoExtraArgs = "--locked -p cargo --features all-static --bin cargo";
            nativeBuildInputs = [
              pkgs.pkg-config
              pkgs.cmake
              pkgs.perl
            ];
            dontUseCmakeConfigure = true;
            # reproducible-builds.sh derives -frandom-seed from each
            # derivation's out path, which would rebuild the vendored C
            # libraries of the deps in the package; one seed keeps them fresh.
            NIX_OUTPATH_USED_AS_RANDOM_SEED = "turtton-cargo";
            doCheck = false;
          }
          // lib.optionalAttrs isDarwin {
            # Cargo links the system libcurl (all-static's
            # curl/force-system-lib-on-osx emits `-l curl` on macOS), and on
            # macOS that is nixpkgs' libcurl, as a dynamic library. curlMinimal
            # is enough (HTTPS and HTTP/2 over its OpenSSL) and keeps the
            # runtime closure small. libiconv comes from nixpkgs as well, so
            # that the one the binary links is a store output we list. OpenSSL,
            # libgit2 and SQLite stay vendored/bundled: no libssl or libcrypto
            # of the store is a dependency of the binary.
            buildInputs = darwinRuntimeLibs;
            # libz-sys would otherwise link the SDK's /usr/lib/libz.1.dylib. A
            # static zlib keeps zlib out of the binary's libraries, as
            # upstream's fetch smoke test (ci/fetch-smoke-test.sh) does.
            LIBZ_SYS_STATIC = "1";
            # What the stdenv exports already, stated: the check compares the
            # binary's minos with this value.
            MACOSX_DEPLOYMENT_TARGET = darwinMinVersion;
          };

          # The dependencies alone: this is what is cached in attic and
          # reused until a manifest or Cargo.lock changes. The default
          # `cargo check` pass would only add metadata-only artifacts.
          cargoDeps = craneLib.buildDepsOnly (
            commonArgs
            // {
              cargoCheckCommand = "true";
              zstdCompressionExtraArgs = "-9";
              # Keep the vendored sources in the cache closure.
              postInstall = ''
                mkdir -p "$out"
                printf '%s\n' "''${cargoVendorDir:?}" > "$out/cargo-vendor-dir"
              '';
            }
          );

          # A flake from a clean tree knows its commit; the build script reads
          # it from `git-commit-info`, as the rustc source tarball provides it.
          commitInfo = lib.optionalString (self ? rev) ''
            printf '%s\n%s\n%s\n' '${self.rev}' '${builtins.substring 0 9 self.rev}' \
              '${builtins.substring 0 4 self.lastModifiedDate}-${builtins.substring 4 2 self.lastModifiedDate}-${builtins.substring 6 2 self.lastModifiedDate}' \
              > git-commit-info
          '';

          # Libraries a static-ish cargo may still load: glibc and libgcc_s.
          # Linux/glibc only: ELF sonames of x86_64, and store paths that exist
          # on Linux alone (pkgs.glibc). A darwin build needs its own list.
          allowedNeeded = "libc.so.6 libm.so.6 libgcc_s.so.1 libdl.so.2 libpthread.so.0 librt.so.1 libutil.so.1 ld-linux-x86-64.so.2";
          allowedStorePaths = [
            pkgs.glibc
            pkgs.stdenv.cc.cc.lib
          ];

          # The install check, in the pieces the platforms share or not. The
          # Linux text is the one the package has always had.
          installCheckHead = ''
            runHook preInstallCheck
            cargo_bin="$out/bin/cargo"
            export HOME="$TMPDIR/home"
            export CARGO_HOME="$HOME/.cargo"
            mkdir -p "$HOME"

            echo "== version names turtton/cargo and the release in TURTTON_VERSION"
            "$cargo_bin" -V | tee version.txt
            grep -F "(turtton/cargo ${release}, https://github.com/turtton/cargo)" version.txt
            if grep -q huacnlee version.txt; then echo "version mentions huacnlee" >&2; exit 1; fi

            echo "== minimal offline build with the toolchain's rustc"
            # Outside the source tree, which is a workspace of its own.
            hello_dir="$(mktemp -d)"
            (
              cd "$hello_dir"
              "$cargo_bin" init --vcs none --name hello .
              "$cargo_bin" build --offline
              test "$(./target/debug/hello)" = "Hello, world!"
              "$cargo_bin" test --offline
            )

          '';

          linuxLinkCheck = ''
            echo "== runtime libraries: only glibc and libgcc_s"
            readelf -d "$cargo_bin"
            for lib in $(readelf -d "$cargo_bin" | sed -n 's/.*Shared library: \[\(.*\)\]/\1/p'); do
              case " ${allowedNeeded} " in
                *" $lib "*) ;;
                *) echo "unexpected shared library: $lib" >&2; exit 1 ;;
              esac
            done
            readelf -d "$cargo_bin" | sed -n 's/.*\(RUNPATH\|RPATH\).*\[\(.*\)\]/\2/p' | tr ':' '\n' > runpath.txt
            while read -r dir; do
              [ -z "$dir" ] && continue
              case "$dir" in
                ${lib.concatMapStringsSep "|" (p: "${p}/lib") allowedStorePaths}) ;;
                *) echo "unexpected library search path: $dir" >&2; exit 1 ;;
              esac
            done < runpath.txt
            grep -aoE '/nix/store/[a-z0-9]{32}-[^/"[:space:]:]+' "$cargo_bin" | sort -u > store-paths.txt || true
            cat store-paths.txt
            while read -r path; do
              # crane overwrites the hash of the vendored sources (and the
              # toolchain) in the binary with e's: not a reference.
              case "$path" in
                /nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-*) ;;
                $out|${lib.concatStringsSep "|" (map toString allowedStorePaths)}) ;;
                *) echo "unexpected store path in the binary: $path" >&2; exit 1 ;;
              esac
            done < store-paths.txt

          '';

          installCheckTail = ''
            echo "== no self-update (installed as \$CARGO_HOME/bin/cargo, every old switch set)"
            bash ci/smoke-installed.sh "$cargo_bin"
            runHook postInstallCheck
          '';

          # aarch64-darwin. The binary is a Mach-O: its libraries are the
          # system's (libSystem and the frameworks, which Apple ships and no
          # store path can provide) and the store outputs listed here.
          darwinMinVersion = pkgs.stdenv.hostPlatform.darwinMinVersion;
          darwinRuntimeLibs = [
            pkgs.curlMinimal
            pkgs.libiconv
          ];
          darwinAllowedStoreLibs = map lib.getLib darwinRuntimeLibs;
          # Their closure: what the binary and those libraries may name or load.
          darwinAllowedClosure = pkgs.closureInfo { rootPaths = darwinAllowedStoreLibs; };
          # What `otool -L` of the binary shows on macos-15 (the install check
          # prints it): libSystem, libobjc and three frameworks. Nothing else
          # of the system, and no Homebrew, build directory or SDK path.
          darwinSystemPrefixes = [
            "/usr/lib/libSystem.B.dylib"
            "/usr/lib/libobjc.A.dylib"
            "/System/Library/Frameworks/CoreFoundation.framework/Versions/"
            "/System/Library/Frameworks/Foundation.framework/Versions/"
            "/System/Library/Frameworks/Security.framework/Versions/"
          ];
          # The store libraries (libcurl's own dependencies) may use any
          # system framework.
          darwinTransitivePrefixes = [
            "/usr/lib/"
            "/System/Library/Frameworks/"
          ];
          machoEnv = ''
            export MACHO_ALLOWED_PREFIXES="${lib.concatStringsSep " " darwinSystemPrefixes}"
            export MACHO_TRANSITIVE_PREFIXES="${lib.concatStringsSep " " darwinTransitivePrefixes}"
            export MACHO_ALLOWED_CLOSURE="${darwinAllowedClosure}/store-paths"
            export MACHO_MINOS="${darwinMinVersion}"
          '';
          darwinLinkCheck = ''
            echo "== Mach-O: arm64, macOS ${darwinMinVersion}, system libraries and the allowed outputs only"
            ${machoEnv}
            export MACHO_SELF_OUT="$out"
            # libcurl is the one system library cargo links (all-static does not
            # make it static on macOS); the OpenSSL of the store never is a
            # direct dependency, it is vendored.
            export MACHO_REQUIRED_DEPS='/libcurl[.0-9]*dylib$'
            export MACHO_FORBIDDEN_DEPS='/lib(ssl|crypto)[.0-9]*dylib$'
            export MACHO_VERBOSE=1
            bash ${./nix/check-macho.sh} "$cargo_bin"

          '';

          cargo = craneLib.buildPackage (
            commonArgs
            // {
              cargoArtifacts = cargoDeps;
              preBuild = commitInfo;
              doInstallCheck = true;
              nativeInstallCheckInputs = [
                rustToolchain
                pkgs.python3
              ]
              ++ lib.optionals pkgs.stdenv.hostPlatform.isLinux [ pkgs.binutils ];
              installCheckPhase =
                installCheckHead + (if isDarwin then darwinLinkCheck else linuxLinkCheck) + installCheckTail;
              meta = {
                description = "turtton/cargo: Cargo with a shared build directory";
                homepage = "https://github.com/turtton/cargo";
                mainProgram = "cargo";
                platforms = systems;
              };
            }
            // lib.optionalAttrs isDarwin {
              # The install check starts listeners (ci/smoke-installed.sh) on
              # 127.0.0.1, which the Darwin sandbox denies by default. The
              # dependencies, built apart, do not get this.
              __darwinAllowLocalNetworking = true;
            }
          );

          # The client CI pushes to attic with. attic builds its HTTP client
          # with reqwest 0.13, whose Linux default TCP_USER_TIMEOUT is 30 s,
          # so the kernel drops an upload as soon as atticd leaves the body
          # unread for 30 s while it stores earlier chunks. Five minutes rides
          # out those stalls and still ends an upload whose peer is gone.
          # attic offers no setting for it, and --replace-fail stops the build
          # once upstream changes this builder. reqwest has the option on
          # Linux only, so the patch is applied, and the client exposed, on
          # Linux only: Darwin has no attic client here.
          atticClient = pkgs.attic-client.overrideAttrs (old: {
            postPatch = (old.postPatch or "") + ''
              substituteInPlace client/src/api/mod.rs --replace-fail \
                '.default_headers(headers)' \
                '.default_headers(headers).tcp_user_timeout(std::time::Duration::from_secs(300))'
            '';
          });

          # Closure of the package: itself and the closure of the allowed
          # outputs, nothing else (no SDK, toolchain or vendored sources).
          darwinClosureCheck =
            pkgs.runCommand "turtton-cargo-closure" { closure = pkgs.closureInfo { rootPaths = [ cargo ]; }; }
              ''
                sort "$closure/store-paths" > actual.txt
                { cat ${darwinAllowedClosure}/store-paths; echo ${cargo}; } | sort -u > expected.txt
                test -s actual.txt
                echo "== closure of the package"
                while read -r p; do du -sk "$p"; done < actual.txt
                echo "== expected: the package and the closure of ${lib.concatStringsSep " " (map toString darwinAllowedStoreLibs)}"
                diff -u expected.txt actual.txt
                touch $out
              '';

          # The fixtures of nix/check-macho.sh: it accepts what it must and
          # fails, for the right reason, on everything else.
          machoInspection =
            pkgs.runCommandCC "turtton-cargo-macho-inspection"
              {
                curlLib = lib.getLib pkgs.curlMinimal;
              }
              ''
                ${machoEnv}
                export MACHO_SELF_OUT="$out"
                check=${./nix/check-macho.sh}

                pass() {
                  local name="$1" file="$2"
                  shift 2
                  echo "== accepted: $name"
                  if ! env "$@" bash "$check" "$file" > "pass-$name.log" 2>&1; then
                    cat "pass-$name.log"
                    echo "fixture $name is rejected but must pass" >&2
                    exit 1
                  fi
                  tail -n 2 "pass-$name.log"
                }
                # reject NAME TAG-REGEX FILE [VAR=value...]: the check must fail,
                # and with that tag, not for another reason.
                reject() {
                  local name="$1" tag="$2" file="$3"
                  shift 3
                  echo "== rejected ($tag): $name"
                  if env "$@" bash "$check" "$file" > "reject-$name.log" 2>&1; then
                    cat "reject-$name.log"
                    echo "fixture $name passes but must be rejected" >&2
                    exit 1
                  fi
                  if ! grep -Eq "^macho-check: ($tag): " "reject-$name.log"; then
                    cat "reject-$name.log"
                    echo "fixture $name fails for another reason than $tag" >&2
                    exit 1
                  fi
                  grep '^macho-check: ' "reject-$name.log"
                }

                echo 'int main(void) { return 0; }' > good.c
                cc good.c -o good
                pass good good
                pass good-required good MACHO_REQUIRED_DEPS=libSystem

                # libcurl and everything it loads, as cargo links it.
                echo 'char *curl_version(void); int main(void) { return curl_version() == 0; }' > curl.c
                cc curl.c -L"$curlLib/lib" -lcurl -o uses-curl
                pass store-library uses-curl MACHO_REQUIRED_DEPS='/libcurl[.0-9]*dylib$'
                reject forbidden-dependency forbidden-dep uses-curl MACHO_FORBIDDEN_DEPS='/libcurl[.0-9]*dylib$'
                reject missing-dependency missing-required good MACHO_REQUIRED_DEPS='/libcurl[.0-9]*dylib$'

                # The package's own path and crane's placeholder are no store
                # reference to reject; any other path is.
                printf 'const char self[] = "%s";\nconst char vendor[] = "/nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-vendor-cargo-deps";\nint main(void) { return self[0] + vendor[0] == 0; }\n' "$out" > self.c
                cc self.c -o self-reference
                pass self-and-placeholder self-reference
                echo 'const char evil[] = "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-evil-1.0"; int main(void) { return evil[0] == 0; }' > evil.c
                cc evil.c -o evil-reference
                reject unknown-store-path store-ref evil-reference
                echo 'const char sdk[] = "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-apple-sdk-14.4"; int main(void) { return sdk[0] == 0; }' > sdk.c
                cc sdk.c -o sdk-reference
                reject sdk-path store-ref sdk-reference

                # Runtime search paths.
                cc good.c -Wl,-rpath,"$PWD/lib" -o rpath-build-dir
                reject rpath-build-dir rpath rpath-build-dir
                cc good.c -Wl,-rpath,/opt/homebrew/lib -o rpath-homebrew
                reject rpath-homebrew rpath rpath-homebrew
                cc good.c -Wl,-rpath,@loader_path/../lib -o rpath-relative
                reject rpath-relative rpath rpath-relative

                # Libraries outside the allowed places, found by their install name.
                echo 'int foo(void) { return 1; }' > foo.c
                echo 'int foo(void); int main(void) { return foo(); }' > usefoo.c
                cc -dynamiclib foo.c -install_name /opt/homebrew/lib/libfoo.dylib -o libfoo-homebrew.dylib
                cc usefoo.c libfoo-homebrew.dylib -o uses-homebrew
                reject homebrew-library dep-disallowed uses-homebrew
                cc -dynamiclib foo.c -install_name "$PWD/libfoo-build.dylib" -o libfoo-build.dylib
                cc usefoo.c libfoo-build.dylib -o uses-build-dir
                reject build-dir-library dep-disallowed uses-build-dir
                cc -dynamiclib foo.c -install_name @rpath/libfoo-rpath.dylib -o libfoo-rpath.dylib
                cc usefoo.c libfoo-rpath.dylib -Wl,-rpath,"$PWD" -o uses-rpath
                reject unresolved-rpath rpath uses-rpath
                cc usefoo.c libfoo-rpath.dylib -o uses-rpath-none
                reject rpath-without-rpath dep-disallowed uses-rpath-none
                # A store library the allowed outputs do not need.
                ! grep -q -- '-bzip2-' "$MACHO_ALLOWED_CLOSURE"
                echo 'const char *BZ2_bzlibVersion(void); int main(void) { return BZ2_bzlibVersion() == 0; }' > bz2.c
                cc bz2.c -L${lib.getLib pkgs.bzip2}/lib -lbz2 -o uses-bzip2
                reject store-library-outside dep-disallowed uses-bzip2

                # Deployment target.
                cc -mmacosx-version-min=11.0 good.c -o old-minos
                reject deployment-target minos old-minos

                # Things that are not a Mach-O, or no analysis at all.
                printf '#!/bin/sh\nexit 0\n' > script
                reject script 'not-macho|arch' script
                : > empty
                reject empty-file 'not-macho|arch' empty
                reject missing-file not-macho does-not-exist
                : > empty-closure
                reject empty-closure tool good MACHO_ALLOWED_CLOSURE="$PWD/empty-closure"

                touch $out
              '';
        in
        {
          packages = {
            inherit cargo;
            default = cargo;
            cargo-deps = cargoDeps;
          }
          // lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
            attic-client = atticClient;
          };

          checks = {
            # The package builds, and its install check passes (version,
            # offline build, runtime libraries, no self-update).
            inherit cargo;
            # The finished store path, run read-only the way a user has it:
            # it must not try to update itself.
            smoke =
              pkgs.runCommand "turtton-cargo-smoke"
                (
                  { nativeBuildInputs = [ pkgs.python3 ]; }
                  // lib.optionalAttrs isDarwin {
                    # The listeners of ci/smoke-installed.sh, as in the install check.
                    __darwinAllowLocalNetworking = true;
                  }
                )
                ''
                  export HOME="$TMPDIR/home"
                  mkdir -p "$HOME"
                  bash ${./ci/smoke-installed.sh} ${cargo}/bin/cargo
                  touch $out
                '';
          }
          // lib.optionalAttrs isDarwin {
            macho-inspection = machoInspection;
            closure = darwinClosureCheck;
          };
        };

      outputs = forAllSystems perSystem;
    in
    {
      packages = nixpkgs.lib.mapAttrs (_: o: o.packages) outputs;
      checks = nixpkgs.lib.mapAttrs (_: o: o.checks) outputs;
    };
}
