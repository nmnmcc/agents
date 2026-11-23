{
  config,
  lib,
  pkgs,
  ...
}:

let
  crate = (builtins.fromTOML (builtins.readFile ./crates/agents/Cargo.toml)).package;
  isLinux = pkgs.stdenv.hostPlatform.isLinux;
  releasePkgs = if isLinux then pkgs.pkgsStatic else pkgs;
  releaseTarget = releasePkgs.stdenv.hostPlatform.rust.rustcTarget;
  # Cargo runs build scripts on the GNU build platform. Their ELF interpreter
  # must come from the Nix store, not the host's /lib.
  rustcWrapper = pkgs.writeShellScript "agents-rustc" ''
    rustc="$1"
    shift
    target_arg=
    for arg do
      if [ -n "$target_arg" ] && [ "$arg" = ${releaseTarget} ]; then
        exec "$rustc" "$@"
      fi
      case "$arg" in
        --target=${releaseTarget}) exec "$rustc" "$@" ;;
        --target) target_arg=1 ;;
        *) target_arg= ;;
      esac
    done
    # pkgsStatic sets NIX_CFLAGS_LINK=-static for the musl target, but the GNU
    # cc wrapper reads it too. Linking a proc-macro with -static -shared pulls
    # in the non-PIC crtbeginT.o, which rust-lld rejects on x86_64.
    unset NIX_CFLAGS_LINK
    exec "$rustc" "$@" \
      -C linker=${pkgs.stdenv.cc}/bin/cc \
      -C link-arg=-Wl,--dynamic-linker=${pkgs.stdenv.cc.bintools.dynamicLinker} \
      -C link-arg=-Wl,-rpath,${lib.getLib pkgs.glibc}/lib
  '';
  rustToolchain = config.languages.rust.toolchainPackage;
  rustPlatform = releasePkgs.makeRustPlatform {
    cargo = rustToolchain;
    rustc =
      if isLinux then
        rustToolchain.override (old: {
          targets = (old.targets or [ ]) ++ [ releaseTarget ];
        })
      else
        rustToolchain;
  };
  exe = ''"$out/bin/${crate.name}"'';
  readelf =
    let
      inherit (releasePkgs.stdenv.cc) bintools;
    in
    lib.getExe' bintools "${bintools.targetPrefix}readelf";
in
{
  packages = [
    pkgs.dovecot
    pkgs.git
    pkgs.knope
    pkgs.python3
    pkgs.pkg-config
    pkgs.aws-lc
  ]
  ++ lib.optional isLinux pkgs.openssl;

  languages.rust = {
    enable = true;
    toolchainFile = ./rust-toolchain.toml;
  };

  outputs.agents = rustPlatform.buildRustPackage {
    pname = crate.name;
    inherit (crate) version;
    nativeBuildInputs = [
      releasePkgs.buildPackages.pkg-config
    ]
    ++ lib.optional isLinux releasePkgs.buildPackages.cmake;
    buildInputs =
      if isLinux then
        [ releasePkgs.openssl ]
      else
        [ (releasePkgs.aws-lc.override { useSharedLibraries = false; }) ];
    depsBuildBuild = lib.optional isLinux pkgs.stdenv.cc;
    dontUseCmakeConfigure = isLinux;
    # Use the Cargo-locked AWS-LC source on Linux: nixpkgs' musl build
    # fails to build its Go symbol generator.
    postConfigure = lib.optionalString isLinux ''
      export AWS_LC_SYS_USE_SYSTEM=0
    '';
    env = {
      AWS_LC_SYS_STATIC = "1";
    }
    // (
      if isLinux then
        {
          OPENSSL_STATIC = "1";
          RUSTC_WRAPPER = rustcWrapper;
        }
      else
        # macOS requires the system libraries, but need not load unused dylibs
        # added by the toolchain.
        { NIX_LDFLAGS = "-dead_strip_dylibs"; }
    );
    src = lib.fileset.toSource {
      root = ./.;
      fileset = lib.fileset.unions [
        ./Cargo.toml
        ./Cargo.lock
        ./crates
        ./samples
        ./agents.schema.json
      ];
    };
    cargoLock.lockFile = ./Cargo.lock;
    postPatch = lib.optionalString isLinux ''
      patch -d "$cargoDepsCopy" -p1 < ${./nix/patches/hopf-mailbox-musl-uid-validity.patch}
    '';

    doInstallCheck = true;
    postInstallCheck =
      (
        if isLinux then
          ''
            readelf=${readelf}
            "$readelf" -lW ${exe} > program-headers
            "$readelf" -dW ${exe} > dynamic-section
            if grep -q INTERP program-headers || grep -q '(NEEDED)' dynamic-section; then
              echo "Expected a static Linux binary" >&2
              exit 1
            fi
          ''
        else
          ''
            otool -L ${exe} > dylibs
            awk '
              NR > 1 && $1 !~ /^\/(usr\/lib|System\/Library)\// {
                print "Unexpected non-system dylib:", $1
                failed = 1
              }
              END { exit failed }
            ' dylibs
          ''
      )
      + ''
        ${exe} --version
      '';

    meta = {
      inherit (crate) description;
      mainProgram = crate.name;
    };
  };

  treefmt = {
    enable = true;
    config = {
      programs = {
        nixfmt.enable = true;
        prettier.enable = true;
        ruff-format.enable = true;
        rustfmt = {
          enable = true;
          package = rustToolchain;
        };
      };
      settings.global.excludes = [
        ".devenv/**"
        ".direnv/**"
        "target/**"
        "devenv.local.nix"
        "devenv.local.yaml"
        # The CLI generates this file byte for byte.
        "agents.schema.json"
      ];
    };
  };

  tasks."devenv:treefmt:run".before = lib.mkForce [ ];

  scripts.test-e2e = {
    description = "Build agents and run the loopback IMAP/SMTP end-to-end test";
    exec = ''
      cd ${lib.escapeShellArg config.devenv.root}
      exec ${lib.getExe pkgs.python3} crates/agents/tests/mail_e2e.py "$@"
    '';
  };

  enterTest = ''
    cargo test --locked --workspace && test-e2e
  '';
}
