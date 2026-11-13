//! JPEG XL encoding and container helpers.
//!
//! The default feature set drives libjxl's `cjxl` from `PATH` and parses the
//! ISO BMFF container. The `decode` feature adds jxl-oxide decoding,
//! validation, header-only dimensions, and the progressive preview cuts.

use std::{path::Path, process::ExitStatus};

/// Files below this size are cheap to serve whole; no preview part.
pub const MIN_PREVIEW_FILE_BYTES: u64 = 64 * 1024;

const CONTAINER_SIGNATURE: [u8; 12] = [
    0x00, 0x00, 0x00, 0x0c, b'J', b'X', b'L', b' ', 0x0d, 0x0a, 0x87, 0x0a,
];

/// Errors from JPEG XL encoding, decoding, and container inspection.
#[derive(Debug)]
pub enum Error {
    /// Reading or writing a file failed.
    Io(std::io::Error),
    /// The encoder could not start.
    Spawn {
        program: &'static str,
        source: std::io::Error,
    },
    /// The encoder ran and rejected the input.
    Encode {
        program: &'static str,
        status: ExitStatus,
        stderr: String,
    },
    /// The bytes are not a JPEG XL codestream or container this crate accepts.
    Invalid(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(error) => write!(formatter, "io error: {error}"),
            Error::Spawn { program, source } => {
                write!(formatter, "failed to run {program}: {source}")
            }
            Error::Encode {
                program,
                status,
                stderr,
            } => write!(formatter, "{program} exited with {status}: {stderr}"),
            Error::Invalid(message) => write!(formatter, "{message}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(error) => Some(error),
            Error::Spawn { source, .. } => Some(source),
            Error::Encode { .. } | Error::Invalid(_) => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Error::Io(error)
    }
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, Error>;

/// How [`encode_file`] drives `cjxl`.
#[derive(Clone, Copy, Debug)]
pub struct EncodeOptions {
    /// Butteraugli distance (0 = lossless, 1.0 = visually lossless).
    pub distance: f32,
    /// Encoder effort (1-10, higher = slower/better).
    pub effort: u8,
    /// Write a progressive file with a low-frequency first frame.
    pub progressive: bool,
}

/// Encode `input` to a fresh JPEG XL file at `output` with `cjxl` from `PATH`.
///
/// `--lossless_jpeg=1` is passed only at distance 0, where JPEG input is
/// recompressed losslessly; at every other distance JPEG input is re-encoded.
pub fn encode_file(input: &Path, output: &Path, options: &EncodeOptions) -> Result<()> {
    let mut command = std::process::Command::new("cjxl");
    command
        .arg(input)
        .arg(output)
        .arg(format!("--distance={}", options.distance))
        .arg(format!("--effort={}", options.effort))
        .arg(format!(
            "--lossless_jpeg={}",
            u8::from(options.distance == 0.0)
        ))
        .arg("--quiet")
        .stdin(std::process::Stdio::null());
    if options.progressive {
        command.arg("-p");
    }
    let result = command.output().map_err(|source| Error::Spawn {
        program: "cjxl",
        source,
    })?;
    if !result.status.success() {
        return Err(Error::Encode {
            program: "cjxl",
            status: result.status,
            stderr: String::from_utf8_lossy(&result.stderr).trim().to_string(),
        });
    }
    Ok(())
}

/// Strip JPEG XL container metadata boxes (`Exif`, `xml `, `jumb`, and their
/// Brotli-compressed `brob` wrappers) from an upload; bare codestreams carry
/// none. `None` means no box was removed.
pub fn strip_metadata(bytes: &[u8]) -> Result<Option<Vec<u8>>> {
    if bytes.starts_with(&[0xff, 0x0a]) {
        return Ok(None);
    }
    if !bytes.starts_with(&CONTAINER_SIGNATURE) {
        return Err(Error::Invalid(
            "upload is not a JPEG XL codestream or container".to_string(),
        ));
    }
    let bad_box = || Error::Invalid("bad JPEG XL box".to_string());
    let mut position = CONTAINER_SIGNATURE.len();
    let mut stripped = CONTAINER_SIGNATURE.to_vec();
    stripped.reserve(bytes.len());
    let mut removed = false;
    while position + 8 <= bytes.len() {
        let (header_len, box_len) = box_header(bytes, position).ok_or_else(bad_box)?;
        let box_end = position.checked_add(box_len).ok_or_else(bad_box)?;
        if box_end > bytes.len() {
            return Err(bad_box());
        }
        let kind = &bytes[position + 4..position + 8];
        // `brob` holds a box type then its Brotli stream, so the four bytes
        // after the header name the metadata it would otherwise hide.
        let wrapped = box_len
            .checked_sub(header_len)
            .is_some_and(|payload| payload >= 4)
            .then(|| &bytes[position + header_len..position + header_len + 4]);
        let is_metadata = matches!(kind, b"Exif" | b"xml " | b"jumb")
            || (kind == b"brob"
                && wrapped.is_some_and(|kind| matches!(kind, b"Exif" | b"xml " | b"jumb")));
        if is_metadata {
            removed = true;
        } else {
            stripped.extend_from_slice(&bytes[position..box_end]);
        }
        position = box_end;
    }
    if position != bytes.len() {
        return Err(Error::Invalid("trailing JPEG XL box bytes".to_string()));
    }
    Ok(removed.then_some(stripped))
}

/// Whether the bytes start with a JPEG XL bare codestream or container.
pub fn is_jxl(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xff, 0x0a]) || bytes.starts_with(&CONTAINER_SIGNATURE)
}

/// Whether the path or content type names JPEG XL.
pub fn is_jxl_file(path: &Path, content_type: Option<&str>) -> bool {
    if content_type.is_some_and(|value| {
        value.eq_ignore_ascii_case("image/jxl") || value.eq_ignore_ascii_case("image/jxl ")
    }) {
        return true;
    }
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("jxl"))
}

/// One JPEG XL container box header at `position`: `(header_len, box_len)`.
/// `None` when the bytes there are not a complete, in-range box header, so a
/// declared length of `u64::MAX` can never wrap the arithmetic.
fn box_header(bytes: &[u8], position: usize) -> Option<(usize, usize)> {
    if position.checked_add(8)? > bytes.len() {
        return None;
    }
    let size = u32::from_be_bytes(bytes[position..position + 4].try_into().ok()?) as u64;
    let (header_len, box_len) = match size {
        0 => (8usize, bytes.len() - position),
        1 => {
            if position.checked_add(16)? > bytes.len() {
                return None;
            }
            let len = u64::from_be_bytes(bytes[position + 8..position + 16].try_into().ok()?);
            (16usize, usize::try_from(len).ok()?)
        }
        size => (8usize, usize::try_from(size).ok()?),
    };
    (box_len >= header_len).then_some((header_len, box_len))
}

#[cfg(feature = "decode")]
mod decode;
#[cfg(feature = "decode")]
pub use decode::{cuts, decode, dimensions, dimensions_from_reader, validate, verify_file, Cuts};

#[cfg(test)]
mod tests {
    use super::*;

    fn container_box(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = u32::try_from(8 + payload.len())
            .expect("box size")
            .to_be_bytes()
            .to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(payload);
        out
    }

    fn jxl_container(boxes: &[Vec<u8>]) -> Vec<u8> {
        let mut out = CONTAINER_SIGNATURE.to_vec();
        for section in boxes {
            out.extend_from_slice(section);
        }
        out
    }

    #[test]
    fn compressed_metadata_boxes_are_stripped_with_their_wrapper() {
        let image = container_box(b"jxlc", &[0xff, 0x0a, 0x00, 0x01]);
        let exif = container_box(b"brob", b"Exifhidden-gps");
        let xmp = container_box(b"brob", b"xml hidden-xmp");
        let jumb = container_box(b"brob", b"jumbhidden-jumbf");
        let other = container_box(b"brob", b"jbrdkeep-me");
        let bytes = jxl_container(&[image.clone(), exif, xmp, jumb, other.clone()]);
        let stripped = strip_metadata(&bytes).expect("strip").expect("removed");
        assert_eq!(stripped, jxl_container(&[image, other]));
        assert!(!stripped.windows(4).any(|window| window == b"Exif"));
        assert!(!stripped.windows(4).any(|window| window == b"xml "));
        assert!(!stripped.windows(4).any(|window| window == b"jumb"));
        // A bare codestream carries no boxes, so there is nothing to remove.
        assert_eq!(
            strip_metadata(&[0xff, 0x0a, 0x00]).expect("codestream"),
            None
        );
        assert!(is_jxl(&[0xff, 0x0a, 0x00]));
        assert!(is_jxl(&CONTAINER_SIGNATURE));
        assert!(!is_jxl(b"\x89PNG"));
    }

    #[test]
    fn extended_box_length_cannot_overflow_the_range_check() {
        let mut bytes = CONTAINER_SIGNATURE.to_vec();
        bytes.extend_from_slice(&1u32.to_be_bytes());
        bytes.extend_from_slice(b"brob");
        bytes.extend_from_slice(&u64::MAX.to_be_bytes());
        let error = strip_metadata(&bytes).expect_err("overflowing box length is invalid");
        assert!(error.to_string().contains("JPEG XL box"), "{error}");
    }

    /// Needs `cjxl` on PATH; skipped without it.
    #[test]
    fn cjxl_failure_reports_status_and_stderr() {
        let dir = tempfile::tempdir().expect("temp");
        let input = dir.path().join("input.png");
        std::fs::write(&input, b"not an image").expect("write");
        let output = dir.path().join("output.jxl");
        let options = EncodeOptions {
            distance: 1.0,
            effort: 7,
            progressive: false,
        };
        match encode_file(&input, &output, &options) {
            Err(Error::Encode { status, stderr, .. }) => {
                assert!(!status.success());
                assert!(!stderr.is_empty());
            }
            Err(Error::Spawn { .. }) => eprintln!("cjxl unavailable; skipping"),
            other => panic!("expected an encode failure, got {other:?}"),
        }
    }
}
