# media-oxide

<!-- simit:badges:start -->

![CI](https://img.shields.io/badge/CI-managed-2088ff) [![docs](https://img.shields.io/badge/docs-enabled-6f42c1)](https://docs.rs/media-oxide) [![crates.io](https://img.shields.io/badge/crates.io-publishable-f46623)](https://crates.io/crates/media-oxide)

<!-- simit:badges:end -->

Immich media transcoding pipeline for replacing original assets with modern
encodes: JPEG XL for images and AV1/Opus MP4 for videos.

The workspace hosts `media-oxide`, a crate of shared media format helpers, plus
the Immich converter that uses it. The converter's crate, binary, NixOS module,
and flake output names are unchanged for existing deployments.

The workspace is split into small crates so the media helpers, Immich API
backend, pipeline orchestration, and codec implementations can evolve
independently. The CLI crate wires those pieces together for one-shot
conversion, polling daemon mode, and a small webhook listener.

## Status

This project is early-stage software. The default CLI mode is deliberately
conservative: `run` and `watch` default to dry-run discovery, so they log the
assets that would be handled without transcoding, uploading, or deleting
anything.

When dry-run is disabled, the sink lifecycle is:

1. upload the transcoded asset to Immich,
2. copy selected metadata from the original,
3. verify that the new asset is readable,
4. delete the original asset through the Immich API.

Use a test Immich library or a fresh backup before processing irreplaceable
media.

## Workspace

| Crate | Purpose |
| --- | --- |
| `media-oxide` | Shared JPEG XL encoding, container, and decoding helpers |
| `rs-immicher-oxide` | CLI runtime in `crates/cli` |
| `rs-immicher-oxide-core` | Pipeline traits, state, outcomes, and shared media types |
| `rs-immicher-oxide-backend-immich` | Immich asset discovery, download, upload, metadata copy, and deletion |
| `rs-immicher-oxide-codec-jxl` | JPEG XL image transcoder via libjxl's `cjxl` |
| `rs-immicher-oxide-codec-av1` | SVT-AV1 video transcoder via the `ffmpeg` CLI |

The repository also exports a NixOS module at
`nixosModules.immich-convert-originals`.

## Requirements

- Rust 1.93 or newer.
- Immich server URL and API key.
- Native build dependencies from the flake: OpenSSL, Clang, pkg-config, and mold.
- `cjxl` (libjxl) and an `ffmpeg`/`ffprobe` with libsvtav1 and libvmaf on
  `PATH`; the flake package wraps both in.

The easiest development environment is the Nix shell:

```sh
nix develop
```

## Build And Test

```sh
nix flake check
nix build
```

Inside the dev shell:

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- --deny warnings
cargo fmt --check
```

## CLI Usage

The CLI reads `IMMICH_URL` and `IMMICH_API_KEY`, or the same values can be
passed as flags.

```sh
export IMMICH_URL="http://localhost:2283/api"
export IMMICH_API_KEY="..."
```

Discover assets without changing Immich. This is the default write mode:

```sh
rs-immicher-oxide run
rs-immicher-oxide run --write-mode dry-run
```

Process only images or only videos:

```sh
rs-immicher-oxide run --image-only
rs-immicher-oxide run --video-only
```

Run a bounded production canary without touching originals:

```sh
rs-immicher-oxide run \
  --write-mode upload-only \
  --asset-id IMAGE_ASSET_ID \
  --asset-id VIDEO_ASSET_ID \
  --state-path ./state.jsonl \
  --manifest-path ./manifest.jsonl
```

`upload-only` uploads replacements, copies supported metadata, verifies the new
asset through the Immich API, and leaves originals untouched. `trash-original`
is the only mode that moves originals to Immich trash after verification. The
CLI does not expose force-delete.

Tune codecs:

```sh
rs-immicher-oxide run --image-distance 1.0 --video-crf 20 --video-min-vmaf 95 --concurrency 2
```

Poll for newly discoverable assets:

```sh
rs-immicher-oxide watch --interval 5min
```

Run the webhook listener:

```sh
rs-immicher-oxide serve --bind 0.0.0.0:8088
```

Webhook mode is dry-run only until single-asset webhook processing is
implemented.

`RUST_LOG` controls tracing output:

```sh
RUST_LOG=debug rs-immicher-oxide run
```

## Supported Media Paths

Only assets owned by the API key's user are converted. Live-photo stills and
their hidden motion parts are left untouched so the pairing survives.

Images: JPEG and PNG are encoded by libjxl's `cjxl` at butteraugli distance
1.0 (visually lossless). `cjxl` carries Exif, XMP, and ICC data and the
orientation, so dates, GPS, camera data, colour, and rotation survive. HEIC,
AVIF, WebP, GIF, TIFF, and RAW are left alone: they are already efficient,
cannot be read by `cjxl`, or carry gain maps, depth, or animation.

Videos: H.264, HEVC, and VP9 are encoded by `ffmpeg` with SVT-AV1 at 10-bit,
CRF 20 and the perceptual (VQ) tune. HDR colour tags, creation time, and the
Apple location/make/model keys are kept; the display rotation is applied to the
pixels. Audio becomes 192 kb/s Opus. Sources already in AV1 are kept.

Every replacement must be at most 90% of the original's size, and every video
encode must reach a mean VMAF of 95 (`--video-min-vmaf`) against its source.
An original that fails either check, or that the encoder cannot read, is kept
and recorded in the state file so it is not retried.

After upload, Immich's `PUT /assets/copy` moves albums, shared links, stacks,
favorite state, and the XMP sidecar (user edits) to the replacement; archive
visibility, description, and rating are set too. Faces are detected afresh.

## NixOS Module

The flake provides `nixosModules.immich-convert-originals`, which configures a
systemd service around `rs-immicher-oxide watch`.

Example:

```nix
{
  inputs.rs-immicher-oxide.url = "git+https://github.com/caniko/rs-immicher-oxide.git";

  outputs = { self, nixpkgs, rs-immicher-oxide, ... }: {
    nixosConfigurations.host = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        rs-immicher-oxide.nixosModules.immich-convert-originals
        {
          services.immich-convert-originals = {
            enable = true;
            package = rs-immicher-oxide.packages.x86_64-linux.default;
            immichUrl = "http://localhost:2283/api";
            apiKeyFile = "/run/secrets/immich-convert-api-key";
            writeMode = "dry-run";
            pollInterval = "5min";
            assetIds = [];
            statePath = "/var/lib/rs-immicher-oxide/state.jsonl";
            manifestPath = "/var/lib/rs-immicher-oxide/manifest.jsonl";
          };
        }
      ];
    };
  };
}
```

The secret file is consumed as a systemd `EnvironmentFile`, so it must define
the API key in environment-file syntax:

```sh
IMMICH_API_KEY=...
```

Before switching a production system from `dry-run` to `upload-only`, create
and validate a fresh Immich PostgreSQL dump plus a readonly btrfs snapshot of
the media location. If either artifact is missing, do not run production writes.

## Repository Maintenance

CI and publish workflows are generated by `simit` for GitHub Actions. The
repository uses Nix-backed CI on `ubuntu-24.04` for each workspace crate and
requires signed, exact-version tags for publication.

Useful checks:

```sh
simit init ci --platform github --check --diff
simit release trust check
```

## License

Licensed under `AGPL-3.0-only`. See `LICENSE`.
