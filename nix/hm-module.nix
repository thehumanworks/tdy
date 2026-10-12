# home-manager module: installs Tidy, writes your policy (validated at build
# time) and optionally runs `tdy watch` as a user service (systemd on Linux,
# launchd on macOS).
self:
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.programs.tdy;
  toml = pkgs.formats.toml { };
  inherit (pkgs.stdenv.hostPlatform) isLinux isDarwin;
  exe = lib.getExe cfg.package;

  # Generated policy, checked with `tdy policy check` so a typo fails the
  # home-manager build instead of the next `tdy plan`.
  policyFile =
    pkgs.runCommand "tdy-policy.toml"
      {
        nativeBuildInputs = [ cfg.package ];
        policy = toml.generate "tdy-policy-unchecked.toml" cfg.settings;
      }
      ''
        export HOME="$TMPDIR" TDY_HOME="$TMPDIR/data" TDY_CONFIG_DIR="$TMPDIR/config" TDY_NO_FNOX=1 NO_COLOR=1
        tdy policy check --policy "$policy" > /dev/null
        cp "$policy" "$out"
      '';

  watchArgs = [
    "watch"
    "--settle"
    (toString cfg.watch.settle)
  ]
  ++ lib.optional cfg.watch.notify "--notify"
  ++ cfg.watch.extraArgs
  ++ lib.optionals (cfg.watch.paths != [ ]) ([ "--" ] ++ cfg.watch.paths);

  # Services start with a bare environment. Give them the user's profile
  # (fnox for API keys, notify-send) and the system tools Tidy probes with
  # (lsof lives in /usr/sbin on macOS).
  servicePath = lib.concatStringsSep ":" (
    lib.optional (cfg.watch.notify && isLinux) "${pkgs.libnotify}/bin"
    ++ [
      "${config.home.profileDirectory}/bin"
      "/run/current-system/sw/bin"
      "/usr/local/bin"
      "/usr/bin"
      "/bin"
      "/usr/sbin"
      "/sbin"
    ]
  );
in
{
  options.programs.tdy = {
    enable = lib.mkEnableOption "Tidy, a fast disk analyser, cleaner and organiser";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "tdy.packages.\${pkgs.stdenv.hostPlatform.system}.default";
      description = "The Tidy package to install.";
    };

    settings = lib.mkOption {
      type = toml.type;
      default = { };
      description = ''
        Policy written to {file}`$XDG_CONFIG_HOME/tdy/policy.toml` and
        validated with `tdy policy check` at build time. Leave empty to
        manage the file yourself (for example with `tdy init`). See the
        README for the schema.
      '';
      example = lib.literalExpression ''
        {
          mode = "manual";
          presets = [ "screenshots" "archive-leftovers" ];
          judge.default = "luna";
          judge.backends.luna = {
            kind = "openai-decisions";
            model = "gpt-6-luna";
            api_key_env = "OPENAI_API_KEY";
          };
          rule = [
            {
              id = "protect.finance";
              action = "keep";
              scope = [ "~/Downloads" "~/Desktop" ];
              when.any = [ "Tax returns, invoices, contracts, payslips or bank statements." ];
            }
          ];
        }
      '';
    };

    policyTests = lib.mkOption {
      type = lib.types.listOf toml.type;
      default = [ ];
      description = ''
        Cases for `tdy policy test`, written to
        {file}`$XDG_CONFIG_HOME/tdy/policy.tests.toml`.
      '';
      example = lib.literalExpression ''
        [
          {
            name = "tax return is protected";
            item = { name = "2024-tax-return.pdf"; folder = "~/Downloads"; size = "2MB"; age = "400d"; };
            expect = "protect.finance";
          }
        ]
      '';
    };

    watch = {
      enable = lib.mkEnableOption ''
        `tdy watch` as a user service: new files in the policy's rule scopes
        are judged once they settle and added to a watch plan (applied right
        away only in autonomous mode)
      '';

      paths = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
        example = [
          "~/Downloads"
          "~/Desktop"
        ];
        description = "Folders to watch. Empty: every rule scope in the policy.";
      };

      settle = lib.mkOption {
        type = lib.types.number;
        default = 2;
        description = "Seconds a file must stay unchanged before it is judged.";
      };

      notify = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = "Desktop notification for each proposal.";
      };

      environmentFile = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "/run/user/1000/secrets/tdy.env";
        description = ''
          File with `NAME=value` lines (for example `OPENAI_API_KEY=…`) loaded
          into the service. Give a path as a string so the secret never
          enters the Nix store. Without it, keys come from `fnox` on the
          service's PATH.
        '';
      };

      extraArgs = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
        example = [
          "--policy"
          "/path/to/policy.toml"
        ];
        description = "Extra arguments for `tdy watch`.";
      };
    };
  };

  config = lib.mkIf cfg.enable (
    lib.mkMerge [
      { home.packages = [ cfg.package ]; }

      (lib.mkIf (cfg.settings != { }) {
        xdg.configFile."tdy/policy.toml".source = policyFile;
      })

      (lib.mkIf (cfg.policyTests != [ ]) {
        xdg.configFile."tdy/policy.tests.toml".source = toml.generate "tdy-policy.tests.toml" {
          case = cfg.policyTests;
        };
      })

      (lib.mkIf (cfg.watch.enable && isLinux) {
        systemd.user.services.tdy-watch = {
          Unit = {
            Description = "Tidy: plan actions for new files in watched folders";
            # A missing or rule-less policy fails fast; don't retry forever.
            StartLimitIntervalSec = 600;
            StartLimitBurst = 3;
          };
          Service = {
            ExecStart = lib.escapeShellArgs ([ exe ] ++ watchArgs);
            Environment = [ "PATH=${servicePath}" ];
            Restart = "on-failure";
            RestartSec = 30;
            Nice = 10;
            IOSchedulingClass = "idle";
            CPUSchedulingPolicy = "batch";
          }
          // lib.optionalAttrs (cfg.watch.environmentFile != null) {
            EnvironmentFile = cfg.watch.environmentFile;
          };
          Install.WantedBy = [ "default.target" ];
        };
      })

      (lib.mkIf (cfg.watch.enable && isDarwin) {
        launchd.agents.tdy-watch = {
          enable = true;
          config = {
            ProgramArguments =
              if cfg.watch.environmentFile == null then
                [ exe ] ++ watchArgs
              else
                [
                  "/bin/sh"
                  "-c"
                  "set -a; . ${lib.escapeShellArg cfg.watch.environmentFile}; set +a; exec ${
                    lib.escapeShellArgs ([ exe ] ++ watchArgs)
                  }"
                ];
            EnvironmentVariables.PATH = servicePath;
            RunAtLoad = true;
            KeepAlive = {
              Crashed = true;
              SuccessfulExit = false;
            };
            ThrottleInterval = 30;
            ProcessType = "Background";
            LowPriorityIO = true;
            Nice = 10;
            StandardOutPath = "${config.home.homeDirectory}/Library/Logs/tdy-watch.log";
            StandardErrorPath = "${config.home.homeDirectory}/Library/Logs/tdy-watch.log";
          };
        };
      })
    ]
  );
}
