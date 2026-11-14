# Changelog

## [Unreleased]

### Changed

- The `media-oxide` crate is now licensed `MIT OR Apache-2.0`, so permissively
  licensed tools can link it. The rest of the workspace stays `AGPL-3.0-only`.
- The workspace is renamed to `media-oxide` after its shared crate. The
  `rs-immicher-oxide-*` crate names, the CLI binary, the NixOS module, and the
  flake output attributes are unchanged, so existing hosts keep working.
- Images are encoded by libjxl's `cjxl` (Exif, XMP, ICC, and orientation kept)
  and only JPEG and PNG are converted; videos are encoded by the `ffmpeg` CLI
  with SVT-AV1 10-bit and gated by a per-video VMAF floor (`--video-min-vmaf`,
  default 95). The `avio` stack and the vendored `ff-pipeline` are gone.
- Originals are kept, and recorded as `Kept`, when the replacement is not at
  most 90% of their size, fails the VMAF floor, is already AV1, or cannot be
  decoded; partial successes are recorded too, so neither is redone each poll.
- Metadata moves through Immich's `PUT /assets/copy` (albums, shared links,
  stack, favorite, sidecar); uploads keep archive visibility, and description
  and rating are re-applied.

### Fixed

- SVG uploads are validated against a namespace-aware allowlist rather than a
  denylist, so an XHTML-namespaced `<iframe srcdoc>` (which Chromium executes),
  `foreignObject`, event-handler attributes, `javascript:`/`data:` references,
  external `url()` references, CSS escapes in attribute values, and any non-SVG
  element are refused. An internal `<!DOCTYPE ... [...]>` subset is rejected
  from the raw text before parsing, so `<!ATTLIST>` defaults a browser would
  apply and declared entities are never seen, and a document over 8 MiB is
  refused. Dimensions are added to the real root element, so a commented-out
  `<svg>` can no longer shadow it.
- Discovery followed `next_page` instead of Immich's `nextPage` and stopped at
  `total`, which is the page length, so only the first 500 assets were seen.
- Search filters were sent in snake_case and silently ignored.
- Partner assets, hidden live-photo motion parts, and live-photo stills are no
  longer converted.
- Originals and verification downloads stream to disk instead of RAM; the
  verification compares the downloaded byte count with the upload.
- The NixOS module passes its CRF, preset, and distance options to the CLI,
  defaults to the `/api` base URL, spools to `/var/tmp`, and runs at idle
  priority; the unused VAAPI, work-directory, encoder-enum, and concurrency
  options are removed.

### Added

- The `foundry` feature in `media-oxide`: the Foundry VTT conversion policy for
  tokens, maps, tiles, artwork, and audio. It plans and runs `ffmpeg`
  (`libwebp`, `libvpx-vp9`, `libvorbis`, `libopus`) conversions, normalizes file
  names, sizes SVGs, and offers process-free `allowed_upload` and
  `sniff_matches` checks for server-side validation.
- `media-oxide`, a shared crate for JPEG XL encoding through `cjxl`,
  container metadata stripping, and optional jxl-oxide decoding, validation,
  header dimensions, and progressive-preview cuts. `rs-immicher-oxide-codec-jxl`
  now encodes through it instead of keeping its own `cjxl` invocation.
- `TranscodedPayload` enum with `Reader` and `File` variants for memory-efficient
  file-backed transcoder output.
- `ImmichApiClient::upload_asset_file()` for streaming file uploads without
  buffering the entire payload into memory.
- Concurrent asset processing in the pipeline via `Pipeline::with_concurrency()`
  and `buffer_unordered`.
- Immich source discovery rewritten as a state-machine stream using
  `futures::stream::unfold` for proper pagination and backpressure.
- Crate metadata for crates.io: `description`, `documentation`, `readme`,
  `keywords`, `categories`, `include`, `repository`, `rust-version`.
- `docs.rs` metadata (`all-features`, `rustdoc-args`) on the backend-immich,
  codec-av1, and codec-jxl crates.
- README files for all crates (`core`, `cli`, `backend-immich`, `codec-av1`,
  `codec-jxl`).
- Immich source integration tests in `backend-immich/tests/source_tests.rs`.
- Forgejo CI workflows for all crates and publish pipelines.
- `simit.toml` project configuration.
- `.opencode/opencode.jsonc` configuration.
- `LICENSE` (AGPL-3.0-only) and `keys/maintainers.gpg`.
- `RecordingState` mock for testing concurrent pipeline state recording.

### Changed

- Flake packaging updated for the current project dependency layout.
- Crate sources restructured from flat top-level directories into
  `crates/` subdirectory.
- Pipeline internals use `Arc<dyn T>` instead of `Box<dyn T>` to support
  shared ownership in concurrent processing.
- AV1 transcoder (`codec-av1`) uses `std::io::copy` instead of
  `read_to_end` for streaming input; output uses `TranscodedPayload::File`.
- JXL transcoder (`codec-jxl`) output uses `TranscodedPayload::Reader`;
  `encode_pixels` extracted into a dedicated `impl` block.
- Immich sink matches on `TranscodedPayload` variants: reads `Reader` into
  memory or streams `File` via `upload_asset_file`.
- Workspace `Cargo.toml` uses `crates/*` glob for member discovery.
- Workspace-level metadata: `rust-version = "1.93"`, `repository`,
  `[workspace.metadata.crane]`.

### Fixed

- Test cleanup: `set_body_json(&body)` → `set_body_json(body)` and
  `set_body_json(&make_search_response(...))` → `set_body_json(make_search_response(...))`
  in `client_tests.rs`.
