{ config, lib, pkgs, ... }:
let
  cfg = config.services.immich-convert-originals;
  inherit (lib) mkIf mkOption mkEnableOption types literalExpression;
  stateDir = "/var/lib/rs-immicher-oxide";
  optionalArg = name: value: lib.optionals (value != null) [ name value ];
  repeatedArgs = name: values: lib.concatMap (value: [ name value ]) values;
  execArgs =
    [
      "${cfg.package}/bin/rs-immicher-oxide"
      "watch"
      "--immich-url"
      cfg.immichUrl
      "--api-key-file"
      cfg.apiKeyFile
      "--interval"
      cfg.pollInterval
      "--write-mode"
      cfg.writeMode
      "--video-crf"
      (toString cfg.videoCrf)
      "--video-preset"
      (toString cfg.videoPreset)
      "--video-min-vmaf"
      (toString cfg.videoMinVmaf)
      "--image-distance"
      (toString cfg.imageDistance)
      "--state-path"
      cfg.statePath
      "--manifest-path"
      cfg.manifestPath
    ]
    ++ optionalArg "--limit" (if cfg.limit == null then null else toString cfg.limit)
    ++ optionalArg "--created-after" cfg.createdAfter
    ++ optionalArg "--taken-after" cfg.takenAfter
    ++ repeatedArgs "--asset-id" cfg.assetIds;
in
{
  options.services.immich-convert-originals = {
    enable = mkEnableOption "Immich original transcoder (AV1/JXL)";

    package = mkOption {
      type = types.package;
      default = pkgs.rs-immicher-oxide or null;
      defaultText = literalExpression "pkgs.rs-immicher-oxide";
      description = "The rs-immicher-oxide package to use.";
    };

    immichUrl = mkOption {
      type = types.str;
      default = "http://localhost:2283/api";
      example = "http://localhost:2283/api";
      description = "Immich API base URL, including the `/api` prefix.";
    };

    apiKeyFile = mkOption {
      type = types.nullOr types.path;
      default = null;
      description = ''
        Path to a file containing a raw Immich API key. The key's user is the
        only owner whose assets are converted; it needs the asset read,
        upload, update, copy, and delete permissions.
        Use agenix: `config.age.secrets.immich-api-key.path`
      '';
    };

    videoCrf = mkOption {
      type = types.int;
      default = 20;
      description = "AV1 CRF value (0-63, lower = better quality).";
    };

    videoPreset = mkOption {
      type = types.int;
      default = 6;
      description = "SVT-AV1 preset (0-13, lower = slower/smaller).";
    };

    videoMinVmaf = mkOption {
      type = types.float;
      default = 95.0;
      description = ''
        Minimum mean VMAF an AV1 replacement must score against its source;
        below it the original is kept.
      '';
    };

    imageDistance = mkOption {
      type = types.float;
      default = 1.0;
      description = "JPEG XL butteraugli distance (0 = lossless, 1.0 = visually lossless).";
    };

    pollInterval = mkOption {
      type = types.str;
      default = "5min";
      example = "10min";
      description = "Poll interval for new asset discovery (watch mode).";
    };

    writeMode = mkOption {
      type = types.enum [ "dry-run" "upload-only" "trash-original" ];
      default = "dry-run";
      description = ''
        Runtime write behavior. Keep the default dry-run for production
        discovery. Use upload-only for the initial canary so originals remain
        untouched. trash-original moves originals to Immich trash after
        verification; force-delete is not exposed by this module.
      '';
    };

    assetIds = mkOption {
      type = types.listOf types.str;
      default = [];
      description = "Explicit Immich asset IDs to process.";
    };

    limit = mkOption {
      type = types.nullOr types.ints.positive;
      default = null;
      description = "Maximum matching assets to process per media pipeline.";
    };

    createdAfter = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "2026-07-01T00:00:00.000Z";
      description = "Only process assets created after this ISO 8601 timestamp.";
    };

    takenAfter = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "2026-07-01T00:00:00.000Z";
      description = "Only process assets taken after this ISO 8601 timestamp.";
    };

    statePath = mkOption {
      type = types.str;
      default = "${stateDir}/state.jsonl";
      description = "Append-only JSONL state file for replaced and kept originals.";
    };

    manifestPath = mkOption {
      type = types.str;
      default = "${stateDir}/manifest.jsonl";
      description = "Append-only JSONL manifest for all run outcomes.";
    };
  };

  config = mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.apiKeyFile != null;
        message = "services.immich-convert-originals.apiKeyFile must be set";
      }
    ];

    environment.systemPackages = [ cfg.package ];

    users.groups.immich-converter = {};
    users.users.immich-converter = {
      isSystemUser = true;
      group = "immich-converter";
    };

    systemd.services.immich-convert-originals = {
      description = "Immich original transcoder — transpile to AV1/JXL";
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];

      # Originals and encodes are spooled to disk, never to a RAM-backed /tmp.
      environment.TMPDIR = "/var/tmp";

      serviceConfig = {
        Type = "simple";
        ExecStart = lib.escapeShellArgs execArgs;
        User = "immich-converter";
        Group = "immich-converter";
        StateDirectory = "rs-immicher-oxide";
        AmbientCapabilities = "";
        CapabilityBoundingSet = "";
        PrivateTmp = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        NoNewPrivileges = true;
        # Backlog encoding must never compete with interactive work.
        Nice = 19;
        CPUSchedulingPolicy = "idle";
        IOSchedulingClass = "idle";
        Restart = "on-failure";
        RestartSec = "10s";
      };
    };
  };
}
