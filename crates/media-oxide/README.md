# media-oxide

Shared media format helpers: JPEG XL encoding, container inspection, and
optional decoding.

The default feature set has no codec dependencies. It drives libjxl's `cjxl`
from `PATH`:

```rust,ignore
use media_oxide::jxl::{encode_file, EncodeOptions};

encode_file(
    "photo.png".as_ref(),
    "photo.jxl".as_ref(),
    &EncodeOptions { distance: 1.0, effort: 7, progressive: true },
)?;
```

`strip_metadata` removes Exif, XMP, and JUMBF boxes from a container, and
`is_jxl`/`is_jxl_file` detect JPEG XL bytes and files.

## Decoding

The `decode` feature adds JPEG XL decoding through
[jxl-oxide](https://crates.io/crates/jxl-oxide), with an explicit pixel budget:

- `decode` renders the first frame as an `image::DynamicImage`.
- `validate` requires the whole codestream to decode and rejects animations.
- `dimensions` reads only the image header, from bytes or a reader.
- `cuts` finds the progressive-preview prefixes a browser can paint early.
- `verify_file` fully decodes a file to prove it renders.

## Foundry VTT assets

The `foundry` feature converts a game master's assets for
[Foundry VTT](https://foundryvtt.com) before upload. Conversion shells out to
`ffmpeg` and `ffprobe`; the crate still implements no codecs.

| Asset | Input | Output |
| --- | --- | --- |
| `token` | png, jpg, jpeg, webp, avif, bmp, tif, tiff | square WebP, aspect preserved, transparent padding |
| `map` | same | WebP scaled to whole `grid_px` cells |
| `tile`, `artwork` | same | WebP, quality 80, compression level 6 |
| `tile`, `artwork` | webp | copied unchanged, never re-encoded lossy |
| any image kind | svg | SVG, with root `width`/`height` added from `viewBox` |
| `token`, `map`, `tile`, `artwork` | webm, mp4, m4v, mov, mkv, gif | VP9 WebM, CRF 32, 4M cap, 30 fps, audio dropped |
| `audio` | wav, flac, aif, aiff | Ogg Vorbis (default) or Opus |
| `audio` | mp3, ogg, oga, opus, m4a, aac | copied unchanged, never lossy-to-lossy |

| Option | Default | Range |
| --- | --- | --- |
| `token_px` | 400 | 200-400 |
| `grid_px` | 100 | 1 or more |
| `audio` | `Vorbis` | `Vorbis`, `Opus` |
| `audio_kbps` | 128 | 64-192, clamped |

Files land in `tokens/`, `maps/`, `tiles/`, `artwork/`, and `audio/` under the
output root, named by `normalize_stem` (lowercase words joined by dashes), and
an existing output is never overwritten.

`ffmpeg` and `ffprobe` must be on `PATH`; the error names
<https://ffmpeg.org/download.html> when they are not. `allowed_upload` and
`sniff_matches` are pure, so a server can validate an upload without spawning a
process.

What cannot be automated, so the DM handles it:

- Token art faces south (down) in Foundry. The converter sizes and pads but does
  not rotate or redraw art, so convert already-south-facing tokens.
- Maps keep whatever grid, or no grid, is drawn in the image. Use assets with
  the grid drawn in, or let Foundry draw it.
- Grid snapping starts at the top-left corner. A map whose grid starts anywhere
  else keeps that offset.

## License

Licensed under either of `MIT` (`LICENSE-MIT`) or `Apache-2.0`
(`LICENSE-APACHE`), at your option. The rest of this workspace stays
`AGPL-3.0-only`.
