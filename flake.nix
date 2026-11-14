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
      # "aarch64-linux" could be added here, but only after the install check's
      # allow list is made per system: it names x86_64 glibc sonames. A darwin
      # system is more work than adding it here: the install check below is
      # glibc- and ELF-specific (readelf, sonames, glibc and libgcc_s store
      # paths) and would need a Mach-O variant (otool) with its own allow list.
      systems = [ "x86_64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f system nixpkgs.legacyPackages.${system});

      # The release shown by `cargo -V`. build.rs reads the same file when
      # cargo compiles, so Nix and plain `cargo build` cannot disagree.
      release = nixpkgs.lib.strings.removeSuffix "\n" (builtins.readFile ./TURTTON_VERSION);

      perSystem =
        system: pkgs:
        let
          lib = pkgs.lib;

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

          cargo = craneLib.buildPackage (
            commonArgs
            // {
              cargoArtifacts = cargoDeps;
              preBuild = commitInfo;
              doInstallCheck = true;
              nativeInstallCheckInputs = [
                rustToolchain
                pkgs.python3
                pkgs.binutils
              ];
              installCheckPhase = ''
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

                echo "== no self-update (installed as \$CARGO_HOME/bin/cargo, every old switch set)"
                bash ci/smoke-installed.sh "$cargo_bin"
                runHook postInstallCheck
              '';
              meta = {
                description = "turtton/cargo: Cargo with a shared build directory";
                homepage = "https://github.com/turtton/cargo";
                mainProgram = "cargo";
                platforms = systems;
              };
            }
          );

          # The client CI pushes to attic with. attic builds its HTTP client
          # with reqwest 0.13, whose Linux default TCP_USER_TIMEOUT is 30 s,
          # so the kernel drops an upload as soon as atticd leaves the body
          # unread for 30 s while it stores earlier chunks. Five minutes rides
          # out those stalls and still ends an upload whose peer is gone.
          # attic offers no setting for it, and --replace-fail stops the build
          # once upstream changes this builder. reqwest has the option on
          # Linux only, so this client is exposed on Linux only.
          atticClient = pkgs.attic-client.overrideAttrs (old: {
            postPatch = (old.postPatch or "") + ''
              substituteInPlace client/src/api/mod.rs --replace-fail \
                '.default_headers(headers)' \
                '.default_headers(headers).tcp_user_timeout(std::time::Duration::from_secs(300))'
            '';
          });
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
            smoke = pkgs.runCommand "turtton-cargo-smoke" { nativeBuildInputs = [ pkgs.python3 ]; } ''
              export HOME="$TMPDIR/home"
              mkdir -p "$HOME"
              bash ${./ci/smoke-installed.sh} ${cargo}/bin/cargo
              touch $out
            '';
          };
        };

      outputs = forAllSystems perSystem;
    in
    {
      packages = nixpkgs.lib.mapAttrs (_: o: o.packages) outputs;
      checks = nixpkgs.lib.mapAttrs (_: o: o.checks) outputs;
    };
}
