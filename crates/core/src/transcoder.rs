use async_trait::async_trait;

use crate::error::Result;
use crate::types::{MediaCodec, MediaKind, TranscodedAsset};

/// A transcoder that converts media from one codec to another.
///
/// Implementations wrap encoder libraries or subprocesses. The pipeline
/// calls `transcode()` with a readable byte stream and receives a
/// streaming `TranscodedAsset` in return.
#[async_trait]
pub trait Transcoder: Send + Sync {
    /// Human-readable label (e.g. "av1-svt-uhq", "jxl-lossless").
    fn label(&self) -> &'static str;

    /// The media kind this transcoder handles.
    fn kind(&self) -> MediaKind;

    /// Input codecs this transcoder can accept.
    fn input_codecs(&self) -> &'static [MediaCodec];

    /// The output codec produced.
    fn output_codec(&self) -> MediaCodec;

    /// Whether this transcoder can accept the given asset.
    fn can_handle(&self, asset: &crate::types::Asset) -> bool {
        asset.kind == self.kind() && self.input_codecs().contains(&asset.codec)
    }

    /// Transcode a source byte stream into the target format.
    ///
    /// The input must be a valid media stream decodable by this transcoder.
    /// The output stream should begin yielding encoded bytes as soon as
    /// they are available (frame-by-frame for video, atomically for images).
    async fn transcode(
        &self,
        input: Box<dyn Read + Send + Unpin + 'static>,
        asset: &crate::types::Asset,
    ) -> Result<TranscodedAsset>;
}

use std::io::Read;

/// Replacements larger than this share of the original are not worth a lossy
/// generation; the original is kept instead.
pub const MAX_OUTPUT_RATIO: f64 = 0.9;

/// Keep the original unless `output_bytes` is a worthwhile saving.
pub fn require_saving(input_bytes: u64, output_bytes: u64) -> crate::error::Result<()> {
    let ratio = output_bytes as f64 / input_bytes.max(1) as f64;
    if output_bytes == 0 || ratio > MAX_OUTPUT_RATIO {
        return Err(crate::error::PipelineError::Keep(format!(
            "output is {:.0}% of the original",
            ratio * 100.0
        )));
    }
    Ok(())
}

/// Run an external encoder tool, returning its stdout. A non-zero exit is a
/// transcoder error carrying the tool's stderr.
pub fn run_tool(cmd: &mut std::process::Command) -> crate::error::Result<Vec<u8>> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let out = cmd
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| {
            crate::error::PipelineError::Transcoder(format!("failed to run {program}: {e}").into())
        })?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(crate::error::PipelineError::Transcoder(
            format!("{program} exited with {}: {}", out.status, stderr.trim()).into(),
        ));
    }
    Ok(out.stdout)
}

#[cfg(test)]
mod tests {
    #[test]
    fn require_saving_keeps_originals_without_a_real_saving() {
        assert!(super::require_saving(100, 50).is_ok());
        assert!(super::require_saving(100, 95).is_err());
        assert!(super::require_saving(100, 0).is_err());
    }
}
