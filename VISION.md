# media-oxide vision

`media-oxide` is a Rust workspace with two jobs: shared media format crates,
and an Immich media transcoding pipeline built on them. The first shared crate,
`media-oxide`, owns JPEG XL encoding, container inspection, and decoding. The
converter discovers original assets through the Immich API, re-encodes images
to JPEG XL and video to AV1/Opus MP4, uploads the result, copies metadata,
verifies that the new asset is readable, and only then deletes the original. It
runs against a self-hosted Immich library in one-shot or polling-daemon mode,
and exposes a webhook listener that currently performs dry-run discovery only.

The converter keeps its existing crate, binary, NixOS module, and flake output
names; only the workspace name changed, so deployments and fleet consumers do
not move.

## Problem

Immich keeps originals in whatever format they arrived in, so a photo and video
library keeps growing in large JPEG and HEVC files. Converting in place is
destructive: a pipeline that uploads and deletes carelessly can lose
irreplaceable media. The safety of the delete step is therefore the central
design constraint.

## Principles

- Destructive work is opt-in. `run` and `watch` default to dry-run discovery,
  which logs the assets that would be handled without transcoding, uploading,
  or deleting anything.
- The sink lifecycle is fixed and ordered: upload the transcoded asset, copy
  selected metadata from the original, verify the new asset is readable, delete
  the original.
- The workspace is split into small crates — shared media helpers, core, Immich
  backend, JXL codec, AV1 codec, CLI — so format handling, the API backend,
  pipeline orchestration, and codecs evolve independently.
- Media handling is memory-conscious: transcoder output is a reader or a
  file-backed payload, uploads stream from file, and the pipeline can process
  assets concurrently.
- Source discovery is a state-machine stream instead of a pre-collected list,
  which keeps pagination and backpressure explicit.

## Scope

- Immich asset discovery, download, upload, metadata copy, and deletion.
- JPEG XL image transcoding and SVT-AV1/Opus video transcoding.
- One-shot, watcher, and webhook CLI modes.
- A NixOS module, `nixosModules.immich-convert-originals`, that runs the watcher
  as a service with an external API-key file.

## Non-goals

- It is not a media server and does not replace Immich; asset state stays in the
  Immich API.
- It does not offer a restore path. The README directs operators to a test
  library or a fresh backup before processing irreplaceable media.
- It does not implement codecs itself; the codec crates drive existing JPEG XL
  and SVT-AV1 encoders.

## Direction

The changelog's Unreleased section records the current push: memory-efficient
file-backed payloads, concurrent pipeline processing, a rewritten discovery
stream, and crate metadata for publishing. Recent commits add rollout-safety
hardening. No roadmap is checked in.

## Place in the fleet

No canix runtime consumer was found. Canix lists the repository in the atlas
`simit-updater` registry and in the maintenance build-cache path inventory.
