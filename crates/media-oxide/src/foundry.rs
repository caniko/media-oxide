//! Foundry VTT asset conversion.
//!
//! Foundry keeps assets in a handful of data-directory folders and renders them
//! at a size the game master picks, so conversion is not a plain re-encode:
//! tokens become square, transparent-padded WebP, maps snap to whole grid
//! cells, and scene audio is stored separately from the animation.
//!
//! This is the one copy of that policy, so the DM client that converts and the
//! server that validates uploads agree. Only [`convert`] spawns processes;
//! [`allowed_upload`] and [`sniff_matches`] are pure.

use std::{
    path::{Path, PathBuf},
    process::ExitStatus,
    str::FromStr,
};

/// WebP's maximum image side in pixels.
const WEBP_MAX_PX: u32 = 16383;

/// Logical asset kinds, each with its own directory and conversion policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssetKind {
    /// Square, transparent-padded actor and item art.
    Token,
    /// Scene background snapped to whole grid cells.
    Map,
    /// Repeating or modular scenery art.
    Tile,
    /// Handouts, portraits, and journal images.
    Artwork,
    /// Music, ambience, and sound effects.
    Audio,
}

impl AssetKind {
    /// Directory under the Foundry data root for this kind.
    pub fn dir(self) -> &'static str {
        match self {
            AssetKind::Token => "tokens",
            AssetKind::Map => "maps",
            AssetKind::Tile => "tiles",
            AssetKind::Artwork => "artwork",
            AssetKind::Audio => "audio",
        }
    }
}

impl std::fmt::Display for AssetKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            AssetKind::Token => "token",
            AssetKind::Map => "map",
            AssetKind::Tile => "tile",
            AssetKind::Artwork => "artwork",
            AssetKind::Audio => "audio",
        })
    }
}

impl FromStr for AssetKind {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "token" => Ok(AssetKind::Token),
            "map" => Ok(AssetKind::Map),
            "tile" => Ok(AssetKind::Tile),
            "artwork" => Ok(AssetKind::Artwork),
            "audio" => Ok(AssetKind::Audio),
            other => Err(Error::Invalid(format!("unknown asset kind `{other}`"))),
        }
    }
}

/// Codec for lossless audio masters; both land in an Ogg container.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioCodec {
    /// Vorbis, the safest choice for older Foundry installs.
    Vorbis,
    /// Opus, smaller at the same quality.
    Opus,
}

/// Conversion settings, with the defaults the DM client ships.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// Token side in pixels; 200-400.
    pub token_px: u32,
    /// Map grid cell in pixels.
    pub grid_px: u32,
    /// Codec for lossless audio masters.
    pub audio: AudioCodec,
    /// Audio bitrate in kbps; clamped to 64-192.
    pub audio_kbps: u32,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            token_px: 400,
            grid_px: 100,
            audio: AudioCodec::Vorbis,
            audio_kbps: 128,
        }
    }
}

/// Source dimensions, from `ffprobe` for the runner and from the caller in
/// tests, so [`plan`] stays pure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Probe {
    /// Source width in pixels.
    pub width: u32,
    /// Source height in pixels.
    pub height: u32,
}

/// What [`convert`] does with one source file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Copy the bytes unchanged.
    Copy,
    /// Add root dimensions to an SVG if it lacks them.
    Svg,
    /// ffmpeg arguments between `-i <input>` and `<output>`.
    Ffmpeg(Vec<String>),
}

/// The chosen output path and how to produce it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// Asset kind, which selects the directory.
    pub kind: AssetKind,
    /// Output file name: normalized stem plus target extension.
    pub file_name: String,
    /// Work needed to produce the output.
    pub action: Action,
}

/// Errors from planning and converting Foundry assets.
#[derive(Debug)]
pub enum Error {
    /// Reading, writing, or creating a file failed.
    Io(std::io::Error),
    /// An external tool could not start.
    Spawn {
        /// Program name, `ffmpeg` or `ffprobe`.
        program: &'static str,
        /// Why it did not start.
        source: std::io::Error,
    },
    /// An external tool ran and rejected the input.
    Tool {
        /// Program name, `ffmpeg` or `ffprobe`.
        program: &'static str,
        /// Exit status.
        status: ExitStatus,
        /// Captured standard error.
        stderr: String,
    },
    /// The input cannot be converted under this policy.
    Invalid(String),
    /// The output path is taken; conversion never overwrites.
    Exists(PathBuf),
}

impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(error) => write!(formatter, "io error: {error}"),
            Error::Spawn { program, source } => write!(
                formatter,
                "failed to run {program}: {source}; install ffmpeg (with ffprobe) from https://ffmpeg.org/download.html"
            ),
            Error::Tool {
                program,
                status,
                stderr,
            } => write!(formatter, "{program} exited with {status}: {stderr}"),
            Error::Invalid(message) => write!(formatter, "{message}"),
            Error::Exists(path) => write!(formatter, "{} already exists", path.display()),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(error) => Some(error),
            Error::Spawn { source, .. } => Some(source),
            Error::Tool { .. } | Error::Invalid(_) | Error::Exists(_) => None,
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

/// Lowercase the name, turn every run of other characters into one `-`, and
/// trim the dashes. An empty result becomes `asset`, so wildcards such as
/// `Guard Red!` and `Guard Blue!` stay distinguishable as `guard-red` and
/// `guard-blue`.
pub fn normalize_stem(name: &str) -> String {
    let mut stem = String::with_capacity(name.len());
    let mut pending_dash = false;
    for character in name.chars() {
        let character = character.to_ascii_lowercase();
        if character.is_ascii_lowercase() || character.is_ascii_digit() {
            if pending_dash && !stem.is_empty() {
                stem.push('-');
            }
            pending_dash = false;
            stem.push(character);
        } else {
            pending_dash = true;
        }
    }
    if stem.is_empty() {
        "asset".to_string()
    } else {
        stem
    }
}

/// Whether [`convert`] has to run `ffprobe` before [`plan`] for this input.
///
/// Only maps need the source dimensions; tokens fit themselves with an ffmpeg
/// scale filter.
pub fn needs_probe(input: &Path, kind: Option<AssetKind>) -> bool {
    family_of(input).is_some_and(|family| {
        family == Family::Still && kind.unwrap_or_else(|| family.default_kind()) == AssetKind::Map
    })
}

/// Choose the output name, directory kind, and ffmpeg arguments for one input.
///
/// `probe` must be present when [`needs_probe`] says so.
pub fn plan(
    input: &Path,
    kind: Option<AssetKind>,
    opts: &Options,
    probe: Option<Probe>,
) -> Result<Plan> {
    let extension = extension_of(input)?;
    let family = Family::of(&extension)
        .ok_or_else(|| Error::Invalid(format!("unsupported extension `{extension}`")))?;
    let kind = kind.unwrap_or_else(|| family.default_kind());
    if kind == AssetKind::Audio && family != Family::Audio {
        return Err(Error::Invalid(format!(
            "`{extension}` is not an audio format"
        )));
    }
    if family == Family::Audio && kind != AssetKind::Audio {
        return Err(Error::Invalid(format!(
            "audio input cannot be stored as a {kind}"
        )));
    }
    let stem = normalize_stem(
        input
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or_default(),
    );
    let (file_name, action) = match family {
        Family::Svg => (format!("{stem}.svg"), Action::Svg),
        Family::Still => {
            if extension == "webp" && matches!(kind, AssetKind::Artwork | AssetKind::Tile) {
                (format!("{stem}.webp"), Action::Copy)
            } else {
                let arguments = match kind {
                    AssetKind::Token => token_arguments(opts)?,
                    AssetKind::Map => map_arguments(opts, probe)?,
                    _ => webp_arguments(),
                };
                (format!("{stem}.webp"), Action::Ffmpeg(arguments))
            }
        }
        Family::Video => (format!("{stem}.webm"), Action::Ffmpeg(video_arguments())),
        Family::Audio if lossless(extension.as_str()) => (
            format!("{stem}.ogg"),
            Action::Ffmpeg(audio_arguments(opts)),
        ),
        // `.oga` is Ogg audio; Foundry and the upload check know it as `.ogg`.
        Family::Audio if extension == "oga" => (format!("{stem}.ogg"), Action::Copy),
        Family::Audio => (format!("{stem}.{extension}"), Action::Copy),
    };
    Ok(Plan {
        kind,
        file_name,
        action,
    })
}

/// Convert `input` into `out_root/<kind dir>/<normalized name>`.
///
/// The output directory is created, an existing output is an [`Error::Exists`],
/// and ffmpeg writes to a hidden temp file next to the output before the rename,
/// so a failed run never leaves a partial asset at the final path.
pub fn convert(
    input: &Path,
    kind: Option<AssetKind>,
    opts: &Options,
    out_root: &Path,
) -> Result<PathBuf> {
    let probe = if needs_probe(input, kind) {
        Some(probe_dimensions(input)?)
    } else {
        None
    };
    let planned = plan(input, kind, opts, probe)?;
    let directory = out_root.join(planned.kind.dir());
    std::fs::create_dir_all(&directory)?;
    let output = directory.join(&planned.file_name);
    if output.exists() {
        return Err(Error::Exists(output));
    }
    // Every action writes an exclusively created temp file; only a complete
    // one is published, and publishing never replaces an existing asset.
    let staged = Staged::create(&output)?;
    match &planned.action {
        Action::Copy => {
            std::fs::copy(input, &staged.0)?;
        }
        Action::Svg => {
            let text = std::fs::read_to_string(input)?;
            if !svg_is_safe(&text) {
                return Err(Error::Invalid(format!(
                    "{} is not a safe SVG",
                    input.display()
                )));
            }
            std::fs::write(&staged.0, svg_with_size(&text)?)?;
        }
        Action::Ffmpeg(arguments) => {
            run_ffmpeg(input, &alpha_decoder(input)?, arguments, &staged.0)?
        }
    }
    staged.publish(&output)?;
    Ok(output)
}

/// Whether `file_name` is a normalized name with an extension `kind` accepts.
///
/// Names must match `[a-z0-9]+(-[a-z0-9]+)*\.<ext>`; this rejects traversal,
/// uppercase, and mismatched extensions without touching the bytes.
pub fn allowed_upload(kind: AssetKind, file_name: &str) -> bool {
    let Some((stem, extension)) = file_name.rsplit_once('.') else {
        return false;
    };
    normalized_stem(stem) && upload_extension(kind, extension)
}

/// Whether `head` starts with the magic bytes the extension implies.
pub fn sniff_matches(file_name: &str, head: &[u8]) -> bool {
    let Some((_, extension)) = file_name.rsplit_once('.') else {
        return false;
    };
    match extension {
        "webp" => magic_at(head, 0, b"RIFF") && magic_at(head, 8, b"WEBP"),
        "png" => magic_at(head, 0, b"\x89PNG\r\n\x1a\n"),
        "jpg" | "jpeg" => magic_at(head, 0, b"\xff\xd8\xff"),
        "avif" => magic_at(head, 4, b"ftyp") && avif_brand(head),
        "mp4" | "m4v" | "m4a" => magic_at(head, 4, b"ftyp") && !avif_brand(head),
        // EBML header whose DocType is `webm`, not plain Matroska.
        "webm" => {
            magic_at(head, 0, b"\x1a\x45\xdf\xa3")
                && head[..head.len().min(64)]
                    .windows(4)
                    .any(|window| window == b"webm")
        }
        "ogg" | "opus" => magic_at(head, 0, b"OggS"),
        "flac" => magic_at(head, 0, b"fLaC"),
        "mp3" => {
            magic_at(head, 0, b"ID3")
                || (head.len() >= 2 && head[0] == 0xff && (head[1] & 0xe0) == 0xe0)
        }
        // ADTS frame sync (layer bits zero) or an ADIF header.
        "aac" => {
            magic_at(head, 0, b"ADIF")
                || (head.len() >= 2 && head[0] == 0xff && (head[1] & 0xf6) == 0xf0)
        }
        "svg" => svg_root_first(head),
        _ => false,
    }
}

fn avif_brand(head: &[u8]) -> bool {
    magic_at(head, 8, b"avif") || magic_at(head, 8, b"avis")
}

/// Whether the first element after the prolog (BOM, XML declaration,
/// comments, whitespace) is `<svg`. Only the valid UTF-8 prefix of `head`
/// counts, so a head cut inside a character still sniffs.
fn svg_root_first(head: &[u8]) -> bool {
    let valid = match std::str::from_utf8(head) {
        Ok(text) => text,
        Err(error) => match std::str::from_utf8(&head[..error.valid_up_to()]) {
            Ok(text) => text,
            Err(_) => return false,
        },
    };
    let mut rest = valid.trim_start_matches('\u{feff}').trim_start();
    loop {
        if let Some(after) = rest.strip_prefix("<?xml") {
            match after.find("?>") {
                Some(end) => rest = after[end + 2..].trim_start(),
                None => return false,
            }
        } else if let Some(after) = rest.strip_prefix("<!--") {
            match after.find("-->") {
                Some(end) => rest = after[end + 3..].trim_start(),
                None => return false,
            }
        } else {
            return rest.starts_with("<svg")
                && rest[4..].starts_with(|character: char| {
                    character.is_ascii_whitespace() || character == '>' || character == '/'
                });
        }
    }
}

/// Whether an SVG is safe to hand to a browser.
///
/// ponytail: a denylist, not a sanitizer; swap in a real sanitizer if SVGs from
/// untrusted authors must render.
pub fn svg_is_safe(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    // Character references and DTD entities can spell any blocked word, and
    // browsers drop whitespace inside URL schemes, so refuse the former and
    // match schemes with whitespace removed.
    let compact: String = lower
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .collect();
    if ["&#", "<!entity", "foreignobject"]
        .iter()
        .any(|blocked| lower.contains(blocked))
        || compact.contains("javascript:")
    {
        return false;
    }
    // `<script` in any namespace (`<svg:script`).
    let script = lower.match_indices("script").any(|(index, _)| {
        matches!(
            lower.as_bytes().get(index.wrapping_sub(1)),
            Some(b'<' | b':')
        )
    });
    !script && !event_attribute(&lower)
}

/// Add `width`/`height` to an SVG root that only declares a `viewBox`.
///
/// This edits the root start tag as a string; nothing else in the document is
/// parsed or changed. A root that already has both dimensions is returned
/// unchanged, and a root with neither dimensions nor a usable `viewBox` is an
/// error.
pub fn svg_with_size(text: &str) -> Result<String> {
    let (tag, start) =
        root_tag(text).ok_or_else(|| Error::Invalid("SVG has no <svg> root tag".to_string()))?;
    let has_width = attribute(tag, "width").is_some();
    let has_height = attribute(tag, "height").is_some();
    if has_width && has_height {
        return Ok(text.to_string());
    }
    let view_box = attribute(tag, "viewBox")
        .flatten()
        .ok_or_else(|| Error::Invalid("SVG has no width/height or viewBox".to_string()))?;
    let numbers: Vec<f64> = view_box
        .split(|character: char| character.is_ascii_whitespace() || character == ',')
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse().ok())
        .collect();
    if numbers.len() != 4 {
        return Err(Error::Invalid(format!(
            "SVG viewBox `{view_box}` is not four numbers"
        )));
    }
    let (width, height) = (numbers[2], numbers[3]);
    let mut sized = String::with_capacity(text.len() + 32);
    sized.push_str(&text[..start + "<svg".len()]);
    if !has_width {
        sized.push_str(&format!(" width=\"{width}\""));
    }
    if !has_height {
        sized.push_str(&format!(" height=\"{height}\""));
    }
    sized.push_str(&text[start + "<svg".len()..]);
    Ok(sized)
}

/// Input families, which decide the target container.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
    /// Raster still image.
    Still,
    /// SVG, kept as SVG.
    Svg,
    /// Video or animation, re-encoded to WebM.
    Video,
    /// Audio, re-encoded to Ogg or copied.
    Audio,
}

impl Family {
    fn of(extension: &str) -> Option<Self> {
        match extension {
            "png" | "jpg" | "jpeg" | "webp" | "avif" | "bmp" | "tif" | "tiff" => {
                Some(Family::Still)
            }
            "svg" => Some(Family::Svg),
            "webm" | "mp4" | "m4v" | "mov" | "mkv" | "gif" => Some(Family::Video),
            "wav" | "flac" | "aif" | "aiff" | "mp3" | "ogg" | "oga" | "opus" | "m4a" | "aac" => {
                Some(Family::Audio)
            }
            _ => None,
        }
    }

    fn default_kind(self) -> AssetKind {
        match self {
            Family::Still | Family::Svg => AssetKind::Artwork,
            Family::Video => AssetKind::Map,
            Family::Audio => AssetKind::Audio,
        }
    }
}

fn family_of(input: &Path) -> Option<Family> {
    input
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .and_then(|extension| Family::of(&extension))
}

fn extension_of(input: &Path) -> Result<String> {
    input
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| Error::Invalid(format!("{} has no file extension", input.display())))
}

fn lossless(extension: &str) -> bool {
    matches!(extension, "wav" | "flac" | "aif" | "aiff")
}

fn arguments(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_string()).collect()
}

fn webp_arguments() -> Vec<String> {
    arguments(&[
        "-c:v",
        "libwebp",
        "-quality",
        "80",
        "-compression_level",
        "6",
        "-map_metadata",
        "-1",
    ])
}

fn token_arguments(opts: &Options) -> Result<Vec<String>> {
    let px = opts.token_px;
    if !(200..=400).contains(&px) {
        return Err(Error::Invalid(format!(
            "token_px must be between 200 and 400, got {px}"
        )));
    }
    let filter = format!(
        "format=rgba,scale={px}:{px}:force_original_aspect_ratio=decrease:flags=lanczos,pad={px}:{px}:(ow-iw)/2:(oh-ih)/2:color=black@0"
    );
    let mut parts = arguments(&["-vf", filter.as_str()]);
    parts.extend(webp_arguments());
    Ok(parts)
}

fn map_arguments(opts: &Options, probe: Option<Probe>) -> Result<Vec<String>> {
    let grid = opts.grid_px;
    if grid == 0 {
        return Err(Error::Invalid("grid_px must be at least 1".to_string()));
    }
    let probe = probe
        .ok_or_else(|| Error::Invalid("map conversion needs the image dimensions".to_string()))?;
    // ponytail: rounding to whole cells can change the aspect by up to half a
    // cell per side; choose a grid_px that divides the source when it matters.
    let width = cells(probe.width, grid).saturating_mul(grid);
    let height = cells(probe.height, grid).saturating_mul(grid);
    if width > WEBP_MAX_PX || height > WEBP_MAX_PX {
        return Err(Error::Invalid(format!(
            "map target {width}x{height} exceeds WebP's {WEBP_MAX_PX} px limit; use a smaller grid_px than {grid}"
        )));
    }
    let filter = format!("scale={width}:{height}:flags=lanczos");
    let mut parts = arguments(&["-vf", filter.as_str()]);
    parts.extend(webp_arguments());
    Ok(parts)
}

/// Nearest whole number of `grid` cells, at least one.
fn cells(pixels: u32, grid: u32) -> u32 {
    ((pixels as f64 / grid as f64).round().max(1.0)) as u32
}

fn video_arguments() -> Vec<String> {
    arguments(&[
        "-c:v",
        "libvpx-vp9",
        "-crf",
        "32",
        "-b:v",
        "4M",
        "-maxrate",
        "4M",
        "-bufsize",
        "8M",
        "-row-mt",
        "1",
        "-fpsmax",
        "30",
        "-an",
        "-pix_fmt",
        "yuva420p",
        "-map_metadata",
        "-1",
    ])
}

fn audio_arguments(opts: &Options) -> Vec<String> {
    let codec = match opts.audio {
        AudioCodec::Vorbis => "libvorbis",
        AudioCodec::Opus => "libopus",
    };
    let bitrate = format!("{}k", opts.audio_kbps.clamp(64, 192));
    arguments(&[
        "-c:a",
        codec,
        "-b:a",
        bitrate.as_str(),
        "-vn",
        "-map_metadata",
        "-1",
    ])
}

fn upload_extension(kind: AssetKind, extension: &str) -> bool {
    match kind {
        AssetKind::Audio => matches!(
            extension,
            "ogg" | "opus" | "mp3" | "flac" | "webm" | "m4a" | "aac"
        ),
        _ => matches!(
            extension,
            "webp" | "svg" | "png" | "jpg" | "jpeg" | "avif" | "webm" | "mp4" | "m4v"
        ),
    }
}

fn normalized_stem(stem: &str) -> bool {
    stem.split('-').all(|part| {
        !part.is_empty()
            && part
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    })
}

fn magic_at(head: &[u8], offset: usize, magic: &[u8]) -> bool {
    head.get(offset..offset + magic.len()) == Some(magic)
}

fn event_attribute(lower: &str) -> bool {
    lower.match_indices("on").any(|(index, _)| {
        let boundary = index == 0
            || !matches!(lower.as_bytes()[index - 1], b'a'..=b'z' | b'0'..=b'9' | b'-');
        if !boundary {
            return false;
        }
        let rest = &lower[index + 2..];
        let letters = rest.bytes().take_while(u8::is_ascii_lowercase).count();
        letters > 0 && rest[letters..].trim_start().starts_with('=')
    })
}

/// The root `<svg ...` start tag without its closing `>`, and its offset.
fn root_tag(text: &str) -> Option<(&str, usize)> {
    let start = text.find("<svg")?;
    let mut quote = None;
    for (offset, character) in text[start..].char_indices() {
        match quote {
            Some(delimiter) => {
                if character == delimiter {
                    quote = None;
                }
            }
            None => match character {
                '"' | '\'' => quote = Some(character),
                '>' => return Some((&text[start..start + offset], start)),
                _ => {}
            },
        }
    }
    None
}

/// One attribute of a start tag: `Some(None)` when it has no value.
fn attribute<'a>(tag: &'a str, name: &str) -> Option<Option<&'a str>> {
    let bytes = tag.as_bytes();
    let mut index = "<svg".len();
    while index < bytes.len() {
        while index < bytes.len() && (bytes[index].is_ascii_whitespace() || bytes[index] == b'/') {
            index += 1;
        }
        let key_start = index;
        while index < bytes.len()
            && !bytes[index].is_ascii_whitespace()
            && bytes[index] != b'='
            && bytes[index] != b'/'
        {
            index += 1;
        }
        let key = &tag[key_start..index];
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        let value = if bytes.get(index) == Some(&b'=') {
            index += 1;
            while index < bytes.len() && bytes[index].is_ascii_whitespace() {
                index += 1;
            }
            match bytes.get(index) {
                Some(&quote) if quote == b'"' || quote == b'\'' => {
                    index += 1;
                    let value_start = index;
                    while index < bytes.len() && bytes[index] != quote {
                        index += 1;
                    }
                    let value = &tag[value_start..index];
                    index += 1;
                    Some(value)
                }
                _ => {
                    let value_start = index;
                    while index < bytes.len() && !bytes[index].is_ascii_whitespace() {
                        index += 1;
                    }
                    Some(&tag[value_start..index])
                }
            }
        } else {
            None
        };
        if key == name {
            return Some(value);
        }
        if key.is_empty() && value.is_none() {
            break;
        }
    }
    None
}

fn probe_dimensions(input: &Path) -> Result<Probe> {
    let text = ffprobe(input, "stream=width,height")?;
    let mut fields = text.trim().split(',');
    let width = fields.next().and_then(|value| value.trim().parse().ok());
    let height = fields.next().and_then(|value| value.trim().parse().ok());
    match (width, height) {
        (Some(width), Some(height)) if width > 0 && height > 0 => Ok(Probe { width, height }),
        _ => Err(Error::Invalid(format!(
            "ffprobe reported no usable dimensions for {}",
            input.display()
        ))),
    }
}

/// One `-show_entries` query on the first video stream, as CSV text.
fn ffprobe(input: &Path, entries: &str) -> Result<String> {
    let output = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", entries])
        .args(["-of", "csv=p=0"])
        .arg(input)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|source| Error::Spawn {
            program: "ffprobe",
            source,
        })?;
    if !output.status.success() {
        return Err(Error::Tool {
            program: "ffprobe",
            status: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn run_ffmpeg(input: &Path, decoder: &[&str], arguments: &[String], output: &Path) -> Result<()> {
    // `-y`: the exclusively created temp file is ours to overwrite.
    let result = std::process::Command::new("ffmpeg")
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y"])
        .args(decoder)
        .arg("-i")
        .arg(input)
        .args(arguments)
        .arg(output)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|source| Error::Spawn {
            program: "ffmpeg",
            source,
        })?;
    if !result.status.success() {
        return Err(Error::Tool {
            program: "ffmpeg",
            status: result.status,
            stderr: String::from_utf8_lossy(&result.stderr).trim().to_string(),
        });
    }
    Ok(())
}

/// FFmpeg's native VP8/VP9 decoders drop the alpha side channel, so a
/// transparent WebM input must be decoded by libvpx to stay transparent.
fn alpha_decoder(input: &Path) -> Result<Vec<&'static str>> {
    let extension = extension_of(input)?;
    if !matches!(extension.as_str(), "webm" | "mkv") {
        return Ok(Vec::new());
    }
    let codec = ffprobe(input, "stream=codec_name")?;
    Ok(match codec.trim() {
        "vp9" => vec!["-c:v", "libvpx-vp9"],
        "vp8" => vec!["-c:v", "libvpx"],
        _ => Vec::new(),
    })
}

/// An exclusively created temp file next to the output, removed on drop.
struct Staged(PathBuf);

impl Staged {
    /// `.<stem>.<pid>-<n>.partial.<ext>`: ffmpeg picks the muxer by
    /// extension, and `create_new` makes the path this invocation's alone.
    fn create(output: &Path) -> Result<Self> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let name = output
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("asset");
        let (stem, extension) = name.rsplit_once('.').unwrap_or((name, "tmp"));
        loop {
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = output.with_file_name(format!(
                ".{stem}.{}-{n}.partial.{extension}",
                std::process::id()
            ));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Ok(Self(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// Publish without replacement: a hard link fails if `output` exists.
    fn publish(self, output: &Path) -> Result<()> {
        match std::fs::hard_link(&self.0, output) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(Error::Exists(output.to_path_buf()))
            }
            Err(error) => Err(error.into()),
        }
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
