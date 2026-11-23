{
  config,
  lib,
  pkgs,
  ...
}:

{
  packages =
    with pkgs;
    [
      dovecot
      git
      python3
    ]
    ++ lib.optionals stdenv.hostPlatform.isLinux [
      openssl
      pkg-config
    ];

  languages.rust = {
    enable = true;
    toolchainFile = ./rust-toolchain.toml;
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
          package = config.languages.rust.toolchainPackage;
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

  # Format explicitly; entering the shell (including for checks) must not edit files.
  tasks."devenv:treefmt:run".before = lib.mkForce [ ];

  scripts.test-e2e = {
    description = "Build agents and run the loopback IMAP/SMTP end-to-end test";
    exec = ''
      cd ${lib.escapeShellArg config.devenv.root}
      exec ${pkgs.python3}/bin/python3 crates/agents/tests/mail_e2e.py "$@"
    '';
  };

  enterTest = ''
    cargo test --locked --workspace && test-e2e
  '';
}
