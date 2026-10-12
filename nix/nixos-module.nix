# System-wide install for NixOS (also works as a nix-darwin module).
# Per-user policy and the `tidy watch` service live in the home-manager
# module (nix/hm-module.nix).
self:
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.programs.tidy;
in
{
  options.programs.tidy = {
    enable = lib.mkEnableOption "Tidy, a fast disk analyser, cleaner and organiser";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "tidy.packages.\${pkgs.stdenv.hostPlatform.system}.default";
      description = "The Tidy package to install.";
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];
  };
}
