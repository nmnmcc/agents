{
  description = "Run project agents with existing harness CLIs";

  # devenv.yaml declares the inputs and devenv.lock pins them. The flake
  # resolves that lock the same way the devenv CLI does, so it needs neither
  # inputs nor a flake.lock of its own.
  outputs =
    { self }:
    let
      lock = builtins.fromJSON (builtins.readFile ./devenv.lock);
      devenvRepo = builtins.fetchTree (removeAttrs lock.nodes.devenv.locked [ "dir" ]);

      agentsFor =
        system:
        let
          inherit
            (import "${devenvRepo}/devenv-nix-backend/bootstrap/resolve-lock.nix" {
              src = ./.;
              lockFilePath = ./devenv.lock;
              inherit system;
            })
            inputs
            ;
          pkgs = inputs.nixpkgs.legacyPackages.${system};
          # Mirrors devenv.lib.mkEval from the devenv flake.
          project = pkgs.lib.evalModules {
            class = "devenv";
            specialArgs = inputs // {
              inherit inputs;
            };
            modules = [
              "${inputs.devenv}/top-level.nix"
              (
                { config, ... }:
                {
                  _module.args.pkgs = pkgs.appendOverlays config.overlays;
                  devenv.flakesIntegration = true;
                  devenv.warnOnNewVersion = false;
                }
              )
              ./devenv.nix
            ];
          };
        in
        project.config.outputs.agents;
    in
    {
      packages =
        builtins.mapAttrs
          (system: _: rec {
            agents = agentsFor system;
            default = agents;
          })
          {
            aarch64-darwin = null;
            aarch64-linux = null;
            x86_64-linux = null;
          };
    };
}
