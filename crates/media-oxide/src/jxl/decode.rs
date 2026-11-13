//! JPEG XL decoding, header-only dimensions, and the progressive preview
//! cuts.
//!
//! `cjxl -p` writes a separate low-frequency frame first (or one progressive
//! keyframe); serving that prefix paints the whole canvas before the rest
//! arrives. Two cuts are stored: the end of the first pass (wall tiles) and
//! the end of the preview frame or its next pass (grid cards). The cut rule is
//! deliberately strict: an unrecognised layout serves the whole file instead
//! of a prefix that might not decode.

use std::{
    io::{Cursor, Read},
    path::Path,
};

use image::{DynamicImage, GenericImageView as _};
use jxl_frame::data::{Toc, TocGroupKind};
use jxl_oxide::{AllocTracker, InitializeResult, JxlImage};

use super::{Error, Result};

/// jxl-oxide reads the image header (including ICC) before `try_init`.
const MAX_HEADER_BYTES: usize = 8 * 1024 * 1024;
/// Decoder allocations allowed per decoded pixel: 16 bytes cover four f32
/// working channels plus extra channels.
const DECODER_BYTES_PER_PIXEL: usize = 16;
/// Ceiling for the full decode of [`verify_file`].
const VERIFY_PIXEL_CEILING: u64 = 536_870_912;

/// The two stored prefix cuts of one progressive JPEG XL file. Both are
/// `None` for a layout the rule does not recognise; `tile` is never smaller
/// than `preview` when both exist.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cuts {
    pub preview: Option<u64>,
    pub tile: Option<u64>,
}

/// Decode the first frame of a JPEG XL codestream or container.
pub fn decode(bytes: &[u8], max_pixels: u64) -> Result<DynamicImage> {
    let (width, height) = dimensions(bytes)?;
    if u64::from(width) * u64::from(height) > max_pixels {
        return Err(pixel_budget(max_pixels, width, height));
    }
    let image = JxlImage::builder()
        .read(Cursor::new(bytes))
        .map_err(decode_error)?;
    let render = image.render_frame(0).map_err(decode_error)?;
    let mut stream = render.stream();
    let (width, height, channels) = (stream.width(), stream.height(), stream.channels());
    let len = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(channels as usize))
        .ok_or_else(|| Error::Invalid("JPEG XL dimensions overflow".to_string()))?;
    let mut pixels = vec![0u8; len];
    stream.write_to_buffer(&mut pixels);
    let inconsistent = || Error::Invalid("JPEG XL pixel buffer is inconsistent".to_string());
    Ok(match channels {
        1 => DynamicImage::ImageLuma8(
            image::ImageBuffer::from_raw(width, height, pixels).ok_or_else(inconsistent)?,
        ),
        2 => DynamicImage::ImageLumaA8(
            image::ImageBuffer::from_raw(width, height, pixels).ok_or_else(inconsistent)?,
        ),
        3 => DynamicImage::ImageRgb8(
            image::ImageBuffer::from_raw(width, height, pixels).ok_or_else(inconsistent)?,
        ),
        4 => DynamicImage::ImageRgba8(
            image::ImageBuffer::from_raw(width, height, pixels).ok_or_else(inconsistent)?,
        ),
        other => {
            return Err(Error::Invalid(format!(
                "JPEG XL with {other} colour channels is not supported"
            )));
        }
    })
}

/// Upload validation: the whole codestream must decode, not just its header.
/// Frame-less or truncated files never enter the store, animated files are
/// refused outright (their keyframes would render unbounded), and decoder
/// allocations are capped by `max_pixels`.
pub fn validate(bytes: &[u8], max_pixels: u64) -> Result<(u32, u32)> {
    let (width, height) = dimensions(bytes)?;
    if u64::from(width) * u64::from(height) > max_pixels {
        return Err(pixel_budget(max_pixels, width, height));
    }
    let alloc_bytes = usize::try_from(max_pixels)
        .unwrap_or(usize::MAX)
        .saturating_mul(DECODER_BYTES_PER_PIXEL);
    let image = JxlImage::builder()
        .alloc_tracker(AllocTracker::with_limit(alloc_bytes))
        .read(Cursor::new(bytes))
        .map_err(decode_error)?;
    if image.image_header().metadata.animation.is_some() {
        return Err(Error::Invalid(
            "animated JPEG XL uploads are not supported".to_string(),
        ));
    }
    if !image.is_loading_done() || image.num_loaded_keyframes() == 0 {
        return Err(Error::Invalid(
            "JPEG XL file ended before its frame data was complete".to_string(),
        ));
    }
    for keyframe in 0..image.num_loaded_keyframes() {
        image.render_frame(keyframe).map_err(decode_error)?;
    }
    Ok((width, height))
}

/// Pixel dimensions only: the JXL header. Every container layout is accepted,
/// including fragmented `jxlp`; only preview-cut discovery needs a single
/// contiguous `jxlc`.
pub fn dimensions(bytes: &[u8]) -> Result<(u32, u32)> {
    dimensions_from_reader(Cursor::new(bytes))
}

/// Pixel dimensions from a reader, without decoding frame data.
pub fn dimensions_from_reader(mut reader: impl Read) -> Result<(u32, u32)> {
    let mut buffer = vec![0u8; 16 * 1024];
    let mut valid = 0usize;
    let mut uninit = JxlImage::builder().build_uninit();
    loop {
        if valid == buffer.len() {
            if buffer.len() >= MAX_HEADER_BYTES {
                return Err(Error::Invalid(
                    "JPEG XL header exceeds the 8 MiB budget".to_string(),
                ));
            }
            buffer.resize(buffer.len() * 2, 0);
        }
        let read = reader.read(&mut buffer[valid..])?;
        if read == 0 {
            return Err(Error::Invalid(
                "JPEG XL file ended before its image header".to_string(),
            ));
        }
        valid += read;
        let consumed = uninit.feed_bytes(&buffer[..valid]).map_err(decode_error)?;
        buffer.copy_within(consumed..valid, 0);
        valid -= consumed;
        match uninit.try_init().map_err(decode_error)? {
            InitializeResult::NeedMoreData(next) => uninit = next,
            InitializeResult::Initialized(image) => {
                let header = image.image_header();
                return Ok((header.size.width, header.size.height));
            }
        }
    }
}

/// Full decode used to prove an encoded file renders, without the interactive
/// pixel budget: 12k-px scans exceed it.
pub fn verify_file(path: &Path) -> Result<(u32, u32)> {
    // ponytail: full decode; tile the verification if scans grow past the
    // 512 M-pixel ceiling or the conversion workers' memory budget.
    let bytes = std::fs::read(path)?;
    let decoded = decode(&bytes, VERIFY_PIXEL_CEILING)?;
    Ok(decoded.dimensions())
}

/// The two file prefixes a progressive `cjxl -p` file paints at: `preview` is
/// the end of the first full-canvas pass (the wall's small tile cut), `tile`
/// the sharper grid cut. Both are `None` for a layout that is not the
/// recognised `cjxl -p` shape.
pub fn cuts(bytes: &[u8]) -> Cuts {
    let none = Cuts::default();
    if bytes.len() < usize::try_from(super::MIN_PREVIEW_FILE_BYTES).unwrap_or(usize::MAX) {
        return none;
    }
    let Some((codestream_start, codestream)) = codestream_slice(bytes) else {
        return none;
    };
    let Ok(image) = JxlImage::builder().read(Cursor::new(codestream)) else {
        return none;
    };
    if image.num_loaded_keyframes() != 1 {
        return none;
    }
    let image_header = image.image_header();
    let (full_width, full_height) = (image_header.size.width, image_header.size.height);

    // A single progressive keyframe has no separate LF frame. Its first
    // renderable full-canvas pass ends after LfGlobal + every LfGroup +
    // HfGlobal: Chromium renders the upsampled LF image there, while the LF
    // groups alone are still blank. A single-section TOC has no split.
    if image.num_loaded_frames() == 1 {
        let Some(frame) = image.frame(0) else {
            return none;
        };
        let header = frame.header();
        if !header.is_keyframe()
            || !header.frame_type.is_normal_frame()
            || !header.frame_type.is_progressive_frame()
            || header.width != full_width
            || header.height != full_height
            || frame.toc().is_single_entry()
        {
            return none;
        }
        let Some(preview_end) = toc_cut(frame.toc(), 0) else {
            return none;
        };
        let Some(cut) = image
            .frame_offset(0)
            .and_then(|offset| offset.checked_add(preview_end))
        else {
            return none;
        };
        if cut > codestream.len() {
            return none;
        }
        let Some(file_preview) = codestream_start
            .checked_add(cut)
            .filter(|cut| *cut < bytes.len())
        else {
            return none;
        };
        // The next pass (pass 1) is the sharper cut. A file with a single
        // pass has no second cut: the small one already is the whole canvas.
        let tile_end = toc_cut(frame.toc(), 1);
        let tile = tile_end
            .filter(|end| *end > preview_end)
            .and_then(|end| {
                image
                    .frame_offset(0)
                    .and_then(|offset| offset.checked_add(end))
            })
            .filter(|cut| *cut <= codestream.len())
            .and_then(|cut| codestream_start.checked_add(cut))
            .filter(|cut| *cut < bytes.len())
            .map(|cut| cut as u64)
            .unwrap_or(file_preview as u64);
        return Cuts {
            preview: Some(file_preview as u64),
            tile: Some(tile),
        };
    }

    if image.num_loaded_frames() != 2 {
        return none;
    }

    let Some(preview) = image.frame(0) else {
        return none;
    };
    let header = preview.header();
    if header.is_keyframe()
        || header.frame_type.is_normal_frame()
        || header.flags.use_lf_frame()
        || header.width != full_width
        || header.height != full_height
    {
        return none;
    }
    let Some(preview_offset) = image.frame_offset(0) else {
        return none;
    };
    let Some(preview_end) = toc_cut(preview.toc(), 0) else {
        return none;
    };
    let Some(cut) = preview_offset.checked_add(preview_end) else {
        return none;
    };
    if cut > codestream.len() {
        return none;
    }

    let Some(keyframe) = image.frame(1) else {
        return none;
    };
    let keyframe_header = keyframe.header();
    let Some(keyframe_offset) = image.frame_offset(1) else {
        return none;
    };
    if !keyframe_header.is_keyframe()
        || !keyframe_header.frame_type.is_normal_frame()
        || !keyframe_header.flags.use_lf_frame()
        || keyframe_header.width != full_width
        || keyframe_header.height != full_height
        || keyframe_offset < cut
    {
        return none;
    }

    let Some(file_preview) = codestream_start
        .checked_add(cut)
        .filter(|cut| *cut < bytes.len())
    else {
        return none;
    };
    // The whole preview frame is the sharper cut: every LF-frame pass at
    // full canvas, where the first pass alone is the blurry wall tile.
    let frame_end = preview
        .toc()
        .iter_bitstream_order()
        .map(|group| group.offset + group.size as usize)
        .max()
        .and_then(|end| preview_offset.checked_add(end))
        .filter(|end| *end <= keyframe_offset);
    let tile = frame_end
        .and_then(|end| codestream_start.checked_add(end))
        .filter(|cut| *cut < bytes.len())
        .map(|cut| cut as u64)
        .unwrap_or(file_preview as u64);
    Cuts {
        preview: Some(file_preview as u64),
        tile: Some(tile),
    }
}

/// The furthest end of the sections a renderer needs through `max_pass`:
/// global and LF sections always, group passes up to that pass.
fn toc_cut(toc: &Toc, max_pass: u32) -> Option<usize> {
    toc.iter_bitstream_order()
        .filter(|group| match group.kind {
            TocGroupKind::GroupPass { pass_idx, .. } => pass_idx <= max_pass,
            _ => true,
        })
        .map(|group| group.offset + group.size as usize)
        .max()
}

/// Codestream bytes and their file offset: a bare codestream, or a container
/// with exactly one `jxlc` box. Fragmented containers stay whole.
fn codestream_slice(bytes: &[u8]) -> Option<(usize, &[u8])> {
    if bytes.starts_with(&[0xff, 0x0a]) {
        return Some((0, bytes));
    }
    if !bytes.starts_with(&super::CONTAINER_SIGNATURE) {
        return None;
    }
    let mut position = super::CONTAINER_SIGNATURE.len();
    let mut codestream = None;
    while position + 8 <= bytes.len() {
        let (header_len, box_len) = super::box_header(bytes, position)?;
        let box_end = position.checked_add(box_len)?;
        if box_end > bytes.len() {
            return None;
        }
        match &bytes[position + 4..position + 8] {
            b"jxlc" => {
                if codestream.is_some() {
                    return None;
                }
                codestream = Some((
                    position + header_len,
                    &bytes[position + header_len..box_end],
                ));
            }
            b"jxlp" => return None,
            _ => {}
        }
        position = box_end;
    }
    codestream
}

fn pixel_budget(max_pixels: u64, width: u32, height: u32) -> Error {
    Error::Invalid(format!(
        "decoded raster exceeds the {max_pixels}-pixel budget ({width}x{height})"
    ))
}

fn decode_error(error: Box<dyn std::error::Error + Send + Sync>) -> Error {
    Error::Invalid(format!("JPEG XL decode failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jxl::{encode_file, EncodeOptions};
    use image::ImageFormat;

    /// Opaque RGB: cjxl -p emits the separate LF preview frame for RGB, while
    /// alpha input takes the single-frame path.
    fn encoded_synthetic_png(width: u32, height: u32) -> Vec<u8> {
        let mut image = image::RgbImage::new(width, height);
        let mut state = 0x2545_f491_4f6c_dd1du64;
        for pixel in image.pixels_mut() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *pixel = image::Rgb([state as u8, (state >> 8) as u8, (state >> 16) as u8]);
        }
        let mut out = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(image)
            .write_to(&mut out, ImageFormat::Png)
            .expect("png");
        out.into_inner()
    }

    fn encoded_synthetic_rgba_png(width: u32, height: u32) -> Vec<u8> {
        let mut image = image::RgbaImage::new(width, height);
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        for pixel in image.pixels_mut() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *pixel = image::Rgba([
                state as u8,
                (state >> 8) as u8,
                (state >> 16) as u8,
                (state >> 24) as u8,
            ]);
        }
        let mut out = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(image)
            .write_to(&mut out, ImageFormat::Png)
            .expect("png");
        out.into_inner()
    }

    /// Two full-canvas frames, written as APNG so cjxl keeps the animation.
    fn encoded_synthetic_apng(width: u32, height: u32) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        {
            let mut encoder = png::Encoder::new(&mut out, width, height);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_animated(2, 0).expect("apng");
            let mut writer = encoder.write_header().expect("header");
            for seed in [1u8, 2u8] {
                writer.set_frame_delay(1, 10).expect("delay");
                let mut frame = image::RgbImage::new(width, height);
                for (index, pixel) in frame.pixels_mut().enumerate() {
                    let value = u8::try_from(index % 251).unwrap_or(0) ^ seed;
                    *pixel = image::Rgb([value, seed, 32]);
                }
                writer.write_image_data(frame.as_raw()).expect("frame");
            }
        }
        out.into_inner()
    }

    fn encode(input: &Path, output: &Path, progressive: bool) -> bool {
        match encode_file(
            input,
            output,
            &EncodeOptions {
                distance: 1.0,
                effort: 7,
                progressive,
            },
        ) {
            Ok(()) => true,
            Err(error) => {
                eprintln!("cjxl unavailable; skipping: {error}");
                false
            }
        }
    }

    fn jxlp_container(codestream: &[u8], chunk: usize) -> Vec<u8> {
        let mut out = vec![
            0x00, 0x00, 0x00, 0x0c, b'J', b'X', b'L', b' ', 0x0d, 0x0a, 0x87, 0x0a,
        ];
        out.extend_from_slice(&20u32.to_be_bytes());
        out.extend_from_slice(b"ftypjxl \0\0\0\0jxl ");
        let chunks = codestream.chunks(chunk).collect::<Vec<_>>();
        for (index, part) in chunks.iter().enumerate() {
            let index = index as u32
                | if index + 1 == chunks.len() {
                    1 << 31
                } else {
                    0
                };
            out.extend_from_slice(
                &u32::try_from(12 + part.len())
                    .expect("box size")
                    .to_be_bytes(),
            );
            out.extend_from_slice(b"jxlp");
            out.extend_from_slice(&index.to_be_bytes());
            out.extend_from_slice(part);
        }
        out
    }

    #[test]
    fn single_progressive_keyframe_cuts_after_its_first_pass() {
        let dir = tempfile::tempdir().expect("temp");
        let input = dir.path().join("alpha.png");
        let output = dir.path().join("alpha.jxl");
        std::fs::write(&input, encoded_synthetic_rgba_png(900, 700)).expect("write");
        if !encode(&input, &output, true) {
            return;
        }
        let bytes = std::fs::read(&output).expect("read");
        let (start, codestream) = codestream_slice(&bytes).expect("codestream");
        let image = JxlImage::builder()
            .read(Cursor::new(codestream))
            .expect("jxl image");
        assert_eq!(image.num_loaded_frames(), 1, "alpha uses one frame");
        let frame = image.frame(0).expect("frame");
        let expected = image.frame_offset(0).expect("offset")
            + frame
                .toc()
                .iter_bitstream_order()
                .filter(|group| {
                    matches!(
                        group.kind,
                        TocGroupKind::All
                            | TocGroupKind::LfGlobal
                            | TocGroupKind::LfGroup(_)
                            | TocGroupKind::HfGlobal
                            | TocGroupKind::GroupPass { pass_idx: 0, .. }
                    )
                })
                .map(|group| group.offset + group.size as usize)
                .max()
                .expect("pass sections");
        let cuts = cuts(&bytes);
        assert_eq!(
            cuts.preview,
            Some(u64::try_from(start + expected).expect("cut"))
        );
        assert!(cuts.preview.expect("cut") > 64 * 1024);
        // The next pass is the sharper grid cut, when the encoder wrote one.
        let tile = cuts.tile.expect("tile cut");
        assert!(tile >= cuts.preview.expect("cut"));
    }

    #[test]
    fn small_progressive_file_keeps_no_cut() {
        // The HABS sheets are ~150 px and encode below 64 KiB: no prefix part.
        let dir = tempfile::tempdir().expect("temp");
        let input = dir.path().join("small.png");
        let output = dir.path().join("small.jxl");
        std::fs::write(&input, encoded_synthetic_png(150, 116)).expect("write");
        if !encode(&input, &output, true) {
            return;
        }
        let bytes = std::fs::read(&output).expect("read");
        assert!((bytes.len() as u64) < super::super::MIN_PREVIEW_FILE_BYTES);
        assert_eq!(cuts(&bytes), Cuts::default(), "small files serve whole");
    }

    #[test]
    fn preview_cut_is_none_for_a_plain_codestream() {
        // A one-frame JXL has no separate LF preview frame.
        let dir = tempfile::tempdir().expect("temp");
        let input = dir.path().join("plain.png");
        let output = dir.path().join("plain.jxl");
        std::fs::write(&input, encoded_synthetic_png(128, 96)).expect("write");
        if !encode(&input, &output, false) {
            return;
        }
        let bytes = std::fs::read(output).expect("read");
        assert_eq!(cuts(&bytes), Cuts::default());
        assert_eq!(dimensions(&bytes).expect("dims"), (128, 96));
    }

    /// An animation's keyframes are unbounded work; uploads must be stills.
    #[test]
    fn animated_jxl_upload_is_rejected() {
        let dir = tempfile::tempdir().expect("temp");
        let input = dir.path().join("clip.png");
        let output = dir.path().join("clip.jxl");
        std::fs::write(&input, encoded_synthetic_apng(96, 64)).expect("write");
        if !encode(&input, &output, false) {
            return;
        }
        let bytes = std::fs::read(&output).expect("read");
        let header = JxlImage::builder()
            .read(Cursor::new(&bytes))
            .expect("jxl image");
        assert!(
            header.image_header().metadata.animation.is_some(),
            "cjxl dropped the animation; this fixture no longer tests the rejection"
        );
        let error = validate(&bytes, 16_777_216).expect_err("animated upload must be rejected");
        assert!(error.to_string().contains("animated"), "{error}");
    }

    #[test]
    fn encoded_preview_frame_is_cut_before_the_keyframe() {
        let dir = tempfile::tempdir().expect("temp");
        let input = dir.path().join("photo.png");
        let output = dir.path().join("photo.jxl");
        std::fs::write(&input, encoded_synthetic_png(900, 700)).expect("write");
        if !encode(&input, &output, true) {
            return;
        }
        let bytes = std::fs::read(&output).expect("read");
        let cuts = cuts(&bytes);
        let cut = cuts.preview.expect("cut");
        assert!(cut < bytes.len() as u64);
        // The cut stops after pass 0, not after every pass of the preview frame.
        let (_, codestream) = codestream_slice(&bytes).expect("codestream");
        let image = JxlImage::builder()
            .read(Cursor::new(codestream))
            .expect("jxl image");
        let preview = image.frame(0).expect("preview frame");
        let all_end = preview
            .toc()
            .iter_bitstream_order()
            .map(|group| group.offset + group.size as usize)
            .max()
            .expect("preview sections");
        let conservative = image.frame_offset(0).expect("offset") + all_end;
        assert!(
            (cut as usize) <= conservative,
            "the preview must never need every LF-frame pass"
        );
        // The whole preview frame is the second, sharper cut for the
        // two-frame layout. A single progressive keyframe cuts at its next
        // pass, so its tile stays shorter than the whole frame.
        let tile = usize::try_from(cuts.tile.expect("tile cut")).expect("tile cut");
        assert!((cut as usize) <= tile && tile <= conservative);
        if image.num_loaded_frames() == 2 {
            assert_eq!(tile, conservative);
        }
        let decoded = decode(&bytes, 16_777_216).expect("decode");
        assert_eq!(decoded.dimensions(), (900, 700));
        assert_eq!(validate(&bytes, 16_777_216).expect("validate"), (900, 700));
    }

    #[test]
    fn two_frame_layout_cuts_at_the_end_of_the_preview_frame() {
        // cjxl 0.12.0 `-d 1 -e 7 -p` of a 400x300 RGB image: a progressive
        // preview frame ending at byte 4900, where the keyframe starts. It is
        // committed because the locked libjxl 0.11.2 encodes the synthetic
        // fixtures as one progressive keyframe, which would leave this path
        // untested. Small enough that its preview frame is one section, so
        // both cuts end the frame; a cut past 4900 reads into the keyframe.
        let bytes = include_bytes!("../../tests/fixtures/two-frame-400x300.jxl");
        let cuts = cuts(bytes);
        assert_eq!(cuts.preview, Some(4900));
        assert_eq!(cuts.tile, Some(4900));
        assert_eq!(validate(bytes, 16_777_216).expect("validate"), (400, 300));
    }

    #[test]
    fn decode_and_validate_enforce_the_pixel_budget() {
        let dir = tempfile::tempdir().expect("temp");
        let input = dir.path().join("plain.png");
        let output = dir.path().join("plain.jxl");
        std::fs::write(&input, encoded_synthetic_png(128, 96)).expect("write");
        if !encode(&input, &output, false) {
            return;
        }
        let bytes = std::fs::read(&output).expect("read");
        for error in [
            decode(&bytes, 1).expect_err("decode budget"),
            validate(&bytes, 1).expect_err("validate budget"),
        ] {
            assert!(error.to_string().contains("-pixel budget"), "{error}");
        }
    }

    #[test]
    fn container_codestream_maps_the_cut_to_file_offsets() {
        let dir = tempfile::tempdir().expect("temp");
        let input = dir.path().join("photo.png");
        let output = dir.path().join("photo.jxl");
        std::fs::write(&input, encoded_synthetic_png(900, 700)).expect("write");
        let status = std::process::Command::new("cjxl")
            .arg(&input)
            .arg(&output)
            .args(["-d", "1", "-e", "7", "-p", "--container=1", "--quiet"])
            .status();
        match status {
            Ok(status) if status.success() => {}
            _ => {
                eprintln!("cjxl unavailable; skipping");
                return;
            }
        }
        let bytes = std::fs::read(&output).expect("read");
        assert!(bytes.starts_with(&[
            0x00, 0x00, 0x00, 0x0c, b'J', b'X', b'L', b' ', 0x0d, 0x0a, 0x87, 0x0a
        ]));
        let (start, codestream) = codestream_slice(&bytes).expect("jxlc");
        assert!(start > 0);
        let image = JxlImage::builder()
            .read(Cursor::new(codestream))
            .expect("jxl image");
        let preview_end = image
            .frame(0)
            .expect("preview frame")
            .toc()
            .iter_bitstream_order()
            .filter(|group| {
                matches!(
                    group.kind,
                    TocGroupKind::All
                        | TocGroupKind::LfGlobal
                        | TocGroupKind::LfGroup(_)
                        | TocGroupKind::HfGlobal
                        | TocGroupKind::GroupPass { pass_idx: 0, .. }
                )
            })
            .map(|group| group.offset + group.size as usize)
            .max()
            .expect("preview sections");
        let cut = cuts(&bytes).preview.expect("cut");
        let expected = start + image.frame_offset(0).expect("preview offset") + preview_end;
        assert_eq!(cut, expected as u64);
        assert!(decode(&bytes[..usize::try_from(cut).unwrap()], 16_777_216).is_err());
        assert_eq!(dimensions(&bytes).expect("dims"), (900, 700));
    }

    #[test]
    fn fragmented_jxlp_images_decode_but_have_no_preview_cut() {
        let dir = tempfile::tempdir().expect("temp");
        let input = dir.path().join("photo.png");
        let output = dir.path().join("photo.jxl");
        std::fs::write(&input, encoded_synthetic_png(320, 240)).expect("write");
        if !encode(&input, &output, true) {
            return;
        }
        let bytes = std::fs::read(&output).expect("read");
        let (_, codestream) = codestream_slice(&bytes).expect("codestream");
        let fragmented = jxlp_container(codestream, 4096);
        assert_eq!(dimensions(&fragmented).expect("dims"), (320, 240));
        assert_eq!(
            decode(&fragmented, 16_777_216)
                .expect("decode")
                .dimensions(),
            (320, 240)
        );
        assert_eq!(cuts(&fragmented), Cuts::default());
    }
}
