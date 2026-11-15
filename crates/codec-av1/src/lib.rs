use async_trait::async_trait;
use std::io::Read;
use std::path::Path;
use std::process::Command;

use rs_immicher_oxide_core::error::{PipelineError, Result};
use rs_immicher_oxide_core::transcoder::{require_saving, run_tool, Transcoder};
use rs_immicher_oxide_core::types::{
    Asset, MediaCodec, MediaKind, TranscodeStats, TranscodedAsset, TranscodedPayload,
};

/// SVT-AV1 transcoder driving the `ffmpeg` and `ffprobe` CLIs.
///
/// Video is encoded 10-bit with SVT-AV1's perceptual (VQ) tune so 8-bit
/// sources gain no banding and HDR (HLG/PQ) keeps its depth and colour tags.
/// The display rotation is applied to the pixels, container metadata
/// (creation time, Apple location/make/model keys) is copied, and audio is
/// re-encoded to transparent-rate Opus. Every encode is scored against its
/// source with VMAF. The first encode uses `crf`; while it scores below
/// `min_vmaf`, the encode is retried at a CRF lowered by [`CRF_STEP`], up to
/// [`CRF_ATTEMPTS`] encodes in total. If none pass, the original is kept.
/// So the cheapest replacement that clears the floor wins.
pub struct SvtAv1UhqTranscoder {
    /// First (highest) CRF tried (0-63, lower = better quality).
    crf: u32,
    /// SVT-AV1 preset (0-13, lower = slower/smaller).
    preset: u32,
    /// Minimum mean VMAF a replacement must reach.
    min_vmaf: f64,
}

impl SvtAv1UhqTranscoder {
    /// Create a new SVT-AV1 transcoder with preset 6 and a VMAF floor of 95.
    pub fn new(crf: u32) -> Self {
        Self {
            crf,
            preset: 6,
            min_vmaf: 95.0,
        }
    }

    /// Set the SVT-AV1 preset.
    pub fn with_preset(mut self, preset: u32) -> Self {
        self.preset = preset;
        self
    }

    /// Set the minimum mean VMAF a replacement must reach.
    pub fn with_min_vmaf(mut self, min_vmaf: f64) -> Self {
        self.min_vmaf = min_vmaf;
        self
    }

    fn encode(&self, crf: u32, input: &Path, output: &Path) -> Result<()> {
        run_tool(
            Command::new("ffmpeg")
                .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-i"])
                .arg(input)
                .args(["-map", "0:v:0", "-map", "0:a?", "-map_metadata", "0"])
                // Phones record variable frame rate; keep every frame and its
                // timestamp so the encode stays 1:1 with the source.
                .args(["-fps_mode", "passthrough"])
                .args(["-c:v", "libsvtav1", "-pix_fmt", "yuv420p10le"])
                .args(["-preset", &self.preset.to_string()])
                .args(["-crf", &crf.to_string()])
                .args(["-svtav1-params", "tune=0"])
                .args(["-c:a", "libopus", "-b:a", "192k"])
                .args(["-movflags", "+faststart+use_metadata_tags", "-f", "mp4"])
                .arg(output),
        )
        .map(drop)
    }
}

/// CRF decrease between encode attempts.
pub const CRF_STEP: u32 = 4;
/// Encodes tried per video before keeping the original.
pub const CRF_ATTEMPTS: u32 = 3;

/// CRFs tried in order, highest (smallest file) first.
fn crf_ladder(start: u32) -> Vec<u32> {
    let mut ladder: Vec<u32> = (0..CRF_ATTEMPTS)
        .map(|i| start.saturating_sub(i * CRF_STEP))
        .collect();
    ladder.dedup();
    ladder
}

/// Codec name of the first video stream, as reported by ffprobe.
fn probe_video_codec(input: &Path) -> Result<String> {
    let out = run_tool(
        Command::new("ffprobe")
            .args(["-v", "error", "-select_streams", "v:0"])
            .args(["-show_entries", "stream=codec_name"])
            .args(["-of", "default=noprint_wrappers=1:nokey=1"])
            .arg(input),
    )?;
    Ok(String::from_utf8_lossy(&out).trim().to_string())
}

/// Mean VMAF of `distorted` against `reference`, both decoded with their
/// display rotation applied and compared at 10-bit.
///
/// Frames are paired by index, not timestamp: a variable-frame-rate source and
/// its re-muxed encode round timestamps differently, and timestamp pairing then
/// scores many frames against a neighbour (a 60 fps phone clip read VMAF ~54
/// instead of ~95). The encode keeps frames 1:1, and a frame-count mismatch can
/// only lower the score, so the gate still fails closed.
fn mean_vmaf(distorted: &Path, reference: &Path, log: &Path) -> Result<f64> {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let graph = format!(
        "[0:v]format=yuv420p10le,settb=1/1000,setpts=N[d];\
         [1:v]format=yuv420p10le,settb=1/1000,setpts=N[r];\
         [d][r]libvmaf=n_threads={threads}:log_fmt=json:log_path={}",
        log.display()
    );
    run_tool(
        Command::new("ffmpeg")
            .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-i"])
            .arg(distorted)
            .arg("-i")
            .arg(reference)
            .args(["-lavfi", &graph, "-f", "null", "-"]),
    )?;
    let json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(log).map_err(PipelineError::Io)?)
            .map_err(|e| PipelineError::Transcoder(Box::new(e)))?;
    json["pooled_metrics"]["vmaf"]["mean"]
        .as_f64()
        .ok_or_else(|| PipelineError::Transcoder("VMAF log has no pooled mean".into()))
}

#[async_trait]
impl Transcoder for SvtAv1UhqTranscoder {
    fn label(&self) -> &'static str {
        "av1-svt-uhq"
    }

    fn kind(&self) -> MediaKind {
        MediaKind::Video
    }

    /// Immich reports containers, not codecs, so every MP4/MOV maps to H264;
    /// the real codec is probed before encoding.
    fn input_codecs(&self) -> &'static [MediaCodec] {
        &[MediaCodec::H264, MediaCodec::H265, MediaCodec::Vp9]
    }

    fn output_codec(&self) -> MediaCodec {
        MediaCodec::Av1
    }

    async fn transcode(
        &self,
        mut input: Box<dyn Read + Send + Unpin + 'static>,
        asset: &Asset,
    ) -> Result<TranscodedAsset> {
        let start = std::time::Instant::now();

        let tmp = tempfile::TempDir::with_prefix("rs-immich-av1-").map_err(PipelineError::Io)?;
        let input_path = tmp.path().join("input");
        let output_path = tmp.path().join("output.mp4");

        let mut input_file = std::fs::File::create(&input_path).map_err(PipelineError::Io)?;
        let input_bytes = std::io::copy(&mut input, &mut input_file).map_err(PipelineError::Io)?;
        drop(input_file);

        let codec = probe_video_codec(&input_path)?;
        if codec.is_empty() || codec == "av1" {
            return Err(PipelineError::Keep(format!(
                "source video codec is {:?}",
                if codec.is_empty() { "none" } else { &codec }
            )));
        }

        // ffmpeg blocks this task's thread for the whole encode, which is
        // acceptable for the long-lived, low-concurrency video pipeline.
        let mut scored = None;
        for crf in crf_ladder(self.crf) {
            self.encode(crf, &input_path, &output_path)?;
            let byte_count = std::fs::metadata(&output_path)
                .map_err(PipelineError::Io)?
                .len();
            // A lower CRF only grows the file, so a miss here is final.
            require_saving(input_bytes, byte_count)?;

            let vmaf = mean_vmaf(&output_path, &input_path, &tmp.path().join("vmaf.json"))?;
            scored = Some((crf, vmaf, byte_count));
            if vmaf >= self.min_vmaf {
                break;
            }
        }
        let Some((crf, vmaf, byte_count)) = scored.filter(|&(_, v, _)| v >= self.min_vmaf) else {
            let (crf, vmaf, _) = scored.expect("the CRF ladder is never empty");
            return Err(PipelineError::Keep(format!(
                "VMAF {vmaf:.2} at CRF {crf} is below {:.2}",
                self.min_vmaf
            )));
        };
        tracing::debug!(
            "{} encoded {codec} to AV1 at CRF {crf}, VMAF {vmaf:.2}",
            asset.id
        );

        let elapsed = start.elapsed();
        Ok(TranscodedAsset {
            original_id: asset.id.clone(),
            codec: MediaCodec::Av1,
            kind: MediaKind::Video,
            payload: TranscodedPayload::File {
                path: output_path,
                _guard: Box::new(tmp),
            },
            byte_count,
            original_checksum: asset.checksum.clone(),
            stats: TranscodeStats {
                decode_seconds: 0.0,
                encode_seconds: elapsed.as_secs_f64(),
                total_seconds: elapsed.as_secs_f64(),
                input_bytes,
                output_bytes: byte_count,
                compression_ratio: byte_count as f64 / input_bytes.max(1) as f64,
                encoder: format!("{} crf={crf} vmaf={vmaf:.2}", self.label()),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset() -> Asset {
        Asset {
            id: "v1".into(),
            filename: "clip.mp4".into(),
            kind: MediaKind::Video,
            codec: MediaCodec::H264,
            mime_type: None,
            size_bytes: None,
            created_at: None,
            checksum: None,
            metadata: serde_json::json!({}),
        }
    }

    #[test]
    fn crf_ladder_steps_down_and_never_repeats() {
        assert_eq!(crf_ladder(28), vec![28, 24, 20]);
        assert_eq!(crf_ladder(5), vec![5, 1, 0]);
        assert_eq!(crf_ladder(2), vec![2, 0]);
        assert_eq!(crf_ladder(0), vec![0]);
    }

    /// Variable-frame-rate sources (every phone clip) must be scored frame
    /// for frame; timestamp pairing read this clip as VMAF ~82.
    #[tokio::test]
    async fn variable_frame_rate_source_passes_the_vmaf_gate() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("vfr.mp4");
        run_tool(
            Command::new("ffmpeg")
                .args(["-nostdin", "-loglevel", "error", "-f", "lavfi", "-i"])
                .arg(
                    "testsrc2=size=320x240:rate=60:duration=1,noise=alls=6:allf=t,\
                     settb=1/90000,setpts=N*1497+mod(N\\,3)*400",
                )
                .args(["-fps_mode", "passthrough", "-enc_time_base", "1/90000"])
                .args(["-video_track_timescale", "90000"])
                .args(["-c:v", "libx264", "-qp", "0", "-pix_fmt", "yuv420p"])
                .arg(&src),
        )
        .unwrap();

        SvtAv1UhqTranscoder::new(20)
            .with_preset(10)
            .transcode(Box::new(std::fs::File::open(&src).unwrap()), &asset())
            .await
            .expect("a VFR encode passes the VMAF gate");
    }

    /// Needs `ffmpeg`/`ffprobe` with libx264, libsvtav1, and libvmaf on PATH
    /// (the flake provides them for checks).
    #[tokio::test]
    async fn lossless_h264_becomes_gated_av1_and_av1_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.mp4");
        run_tool(
            Command::new("ffmpeg")
                .args(["-nostdin", "-loglevel", "error", "-f", "lavfi"])
                .args([
                    "-i",
                    "testsrc2=size=320x240:rate=30:duration=1,noise=alls=6:allf=t",
                ])
                .args(["-c:v", "libx264", "-qp", "0", "-pix_fmt", "yuv420p"])
                .arg(&src),
        )
        .unwrap();

        let t = SvtAv1UhqTranscoder::new(20).with_preset(10);
        let out = t
            .transcode(Box::new(std::fs::File::open(&src).unwrap()), &asset())
            .await
            .expect("encode passes the VMAF gate");
        let TranscodedPayload::File { path, _guard } = out.payload else {
            panic!("expected file payload");
        };
        assert_eq!(probe_video_codec(&path).unwrap(), "av1");

        // A display matrix adds stream side data that broke CSV probing.
        let rotated = dir.path().join("rotated.mp4");
        run_tool(
            Command::new("ffmpeg")
                .args(["-nostdin", "-loglevel", "error"])
                .args(["-display_rotation", "90", "-i"])
                .arg(&path)
                .args(["-c", "copy"])
                .arg(&rotated),
        )
        .unwrap();
        let again = t
            .transcode(Box::new(std::fs::File::open(&rotated).unwrap()), &asset())
            .await;
        assert!(matches!(again, Err(PipelineError::Keep(_))));
    }
}
