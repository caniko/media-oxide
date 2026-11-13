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
