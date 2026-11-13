use async_trait::async_trait;
use std::io::{Cursor, Read};

use media_oxide::jxl::{encode_file, EncodeOptions};
use rs_immicher_oxide_core::error::{PipelineError, Result};
use rs_immicher_oxide_core::transcoder::{require_saving, Transcoder};
use rs_immicher_oxide_core::types::{
    Asset, MediaCodec, MediaKind, TranscodeStats, TranscodedAsset, TranscodedPayload,
};

/// JPEG XL transcoder driving libjxl's reference `cjxl` encoder.
///
/// `cjxl` carries Exif, XMP, and ICC data into the JXL container and maps the
/// Exif orientation to the codestream, so Immich keeps dates, GPS, camera
/// data, colour, and rotation. Only JPEG and PNG are accepted: HEIC, AVIF,
/// and WebP are already efficient and carry gain maps, depth, or animation
/// that a JXL conversion would drop.
pub struct JxlTranscoder {
    /// Butteraugli distance (0 = lossless, 1.0 = visually lossless).
    distance: f32,
    /// Encoder effort (1-10, higher = slower/better).
    effort: u8,
}

impl JxlTranscoder {
    /// Create a new JXL transcoder.
    ///
    /// - `distance = 0.0`: lossless (JPEG input uses reversible recompression)
    /// - `distance = 1.0`: visually lossless (default)
    pub fn new(distance: f32) -> Self {
        Self {
            distance,
            effort: 7,
        }
    }

    /// Set encoder effort (1-10). Default 7.
    pub fn with_effort(mut self, effort: u8) -> Self {
        self.effort = effort.clamp(1, 10);
        self
    }

    fn encode(&self, data: &[u8], codec: MediaCodec) -> Result<Vec<u8>> {
        let tmp = tempfile::TempDir::with_prefix("rs-immich-jxl-").map_err(PipelineError::Io)?;
        let ext = if codec == MediaCodec::Png {
            "png"
        } else {
            "jpg"
        };
        let input = tmp.path().join(format!("input.{ext}"));
        let output = tmp.path().join("output.jxl");
        std::fs::write(&input, data).map_err(PipelineError::Io)?;

        encode_file(
            &input,
            &output,
            &EncodeOptions {
                distance: self.distance,
                effort: self.effort,
                progressive: false,
            },
        )
        .map_err(|error| PipelineError::Transcoder(Box::new(error)))?;
        std::fs::read(&output).map_err(PipelineError::Io)
    }
}

#[async_trait]
impl Transcoder for JxlTranscoder {
    fn label(&self) -> &'static str {
        if self.distance == 0.0 {
            "jxl-lossless"
        } else {
            "jxl-visual-lossless"
        }
    }

    fn kind(&self) -> MediaKind {
        MediaKind::Image
    }

    fn input_codecs(&self) -> &'static [MediaCodec] {
        &[MediaCodec::Jpeg, MediaCodec::Png]
    }

    fn output_codec(&self) -> MediaCodec {
        MediaCodec::Jxl
    }

    async fn transcode(
        &self,
        input: Box<dyn Read + Send + Unpin + 'static>,
        asset: &Asset,
    ) -> Result<TranscodedAsset> {
        let start = std::time::Instant::now();

        let mut data = Vec::with_capacity(asset.size_bytes.unwrap_or(8192) as usize);
        let mut reader = input;
        reader.read_to_end(&mut data).map_err(PipelineError::Io)?;
        let input_bytes = data.len() as u64;

        // ponytail: blocks the runtime thread for the encode, like the video
        // path; move to spawn_blocking if image concurrency starves the client.
        let jxl_bytes = self.encode(&data, asset.codec)?;
        let output_bytes = jxl_bytes.len() as u64;
        require_saving(input_bytes, output_bytes)?;
        let elapsed = start.elapsed();

        Ok(TranscodedAsset {
            original_id: asset.id.clone(),
            codec: MediaCodec::Jxl,
            kind: MediaKind::Image,
            payload: TranscodedPayload::Reader(Box::new(Cursor::new(jxl_bytes))),
            byte_count: output_bytes,
            original_checksum: asset.checksum.clone(),
            stats: TranscodeStats {
                decode_seconds: 0.0,
                encode_seconds: elapsed.as_secs_f64(),
                total_seconds: elapsed.as_secs_f64(),
                input_bytes,
                output_bytes,
                compression_ratio: output_bytes as f64 / input_bytes.max(1) as f64,
                encoder: self.label().to_string(),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(codec: MediaCodec) -> Asset {
        Asset {
            id: "a1".into(),
            filename: "photo.png".into(),
            kind: MediaKind::Image,
            codec,
            mime_type: None,
            size_bytes: None,
            created_at: None,
            checksum: None,
            metadata: serde_json::json!({}),
        }
    }

    #[test]
    fn accepts_only_jpeg_and_png() {
        let t = JxlTranscoder::new(1.0);
        assert!(t.can_handle(&asset(MediaCodec::Jpeg)));
        assert!(t.can_handle(&asset(MediaCodec::Png)));
        assert!(!t.can_handle(&asset(MediaCodec::Heic)));
        assert!(!t.can_handle(&asset(MediaCodec::Unknown)));
    }

    /// Needs `cjxl` on PATH (the flake provides it for checks).
    #[tokio::test]
    async fn png_becomes_smaller_jxl() {
        use image::ImageEncoder;
        let (w, h) = (256u32, 256u32);
        let pixels: Vec<u8> = (0..w * h)
            .flat_map(|i| {
                let (x, y) = (i % w, i / w);
                [x as u8, y as u8, ((x * y) % 251) as u8]
            })
            .collect();
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&pixels, w, h, image::ExtendedColorType::Rgb8)
            .unwrap();

        let out = JxlTranscoder::new(1.0)
            .transcode(Box::new(Cursor::new(png.clone())), &asset(MediaCodec::Png))
            .await
            .expect("cjxl encode");
        assert!(out.byte_count < png.len() as u64);
        let TranscodedPayload::Reader(mut r) = out.payload else {
            panic!("expected in-memory payload");
        };
        let mut jxl = Vec::new();
        r.read_to_end(&mut jxl).unwrap();
        // ISO BMFF container signature, used because cjxl writes metadata boxes.
        assert!(
            jxl.starts_with(&[0, 0, 0, 0x0c, b'J', b'X', b'L', b' '])
                || jxl.starts_with(&[0xff, 0x0a])
        );
    }
}
