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
/// sources gain no banding. HDR (PQ/HLG) originals are kept: VMAF's SDR
/// models cannot vouch for them.
/// The display rotation is applied to the pixels, container metadata
/// (creation time, Apple location/make/model keys) is copied, and audio is
/// re-encoded to transparent-rate Opus. Every encode is scored against its
/// source with VMAF, frame for frame. It passes when the mean reaches
/// `min_vmaf` and both the worst 1% of frames and the worst second of playback
/// reach `min_vmaf - LOW_TAIL_MARGIN`. The first encode uses `crf`. While an
/// encode fails, it is retried at a CRF lowered by [`CRF_STEP`], up to
/// [`CRF_ATTEMPTS`] encodes in total. If none pass, the original is kept, so
/// the highest CRF that clears the floors wins.
pub struct SvtAv1UhqTranscoder {
    /// First (highest) CRF tried (0-63, lower = better quality).
    crf: u32,
    /// SVT-AV1 preset (0-13, lower = slower/smaller).
    preset: u32,
    /// Minimum mean VMAF a replacement must reach; the low tail may sit
    /// [`LOW_TAIL_MARGIN`] below it.
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
                // timestamp (in the source's timebase) so the encode stays 1:1
                // with the source. Passthrough alone still re-times frames
                // onto the encoder's timebase and can collapse two of them.
                .args(["-fps_mode", "passthrough", "-enc_time_base:v", "demux"])
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

/// Run an ffmpeg tool that must neither fail nor report errors. ffprobe and
/// ffmpeg exit 0 after skipping corrupt frames, and [`run_tool`] drops stderr
/// on success, so `-v error` output is the only sign of a damaged stream.
fn run_clean(cmd: &mut Command) -> Result<Vec<u8>> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let out = cmd
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| PipelineError::Transcoder(format!("failed to run {program}: {e}").into()))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() || !stderr.trim().is_empty() {
        return Err(PipelineError::Keep(format!(
            "{program} did not decode cleanly: {}",
            stderr.trim().lines().next().unwrap_or("non-zero exit")
        )));
    }
    Ok(out.stdout)
}

/// `key=value` fields of a single ffprobe stream section.
fn probe_stream(input: &Path, entries: &str) -> Result<String> {
    let out = run_tool(
        Command::new("ffprobe")
            .args(["-v", "error", "-select_streams", "v:0"])
            .args(["-show_entries", &format!("stream={entries}")])
            .args(["-of", "default=noprint_wrappers=1"])
            .arg(input),
    )?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

fn field<'a>(text: &'a str, key: &str) -> &'a str {
    text.lines()
        .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
        .unwrap_or("")
}

/// Codec name and transfer characteristic of the first video stream.
fn probe_video_codec(input: &Path) -> Result<(String, String)> {
    let text = probe_stream(input, "codec_name,color_transfer")?;
    Ok((
        field(&text, "codec_name").trim().to_string(),
        field(&text, "color_transfer").trim().to_string(),
    ))
}

/// PQ and HLG transfers. VMAF's SDR models do not score them meaningfully,
/// and HDR10+/Dolby Vision metadata is invisible to a pixel comparison.
const HDR_TRANSFERS: &[&str] = &["smpte2084", "arib-std-b67"];

/// How far below `min_vmaf` the worst 1% of frames and the worst one-second
/// window may fall. A mean alone passed five black frames in five seconds.
pub const LOW_TAIL_MARGIN: f64 = 5.0;

/// VMAF of an encode against its source.
#[derive(Debug, Clone, Copy, PartialEq)]
struct VmafScore {
    mean: f64,
    /// 1st-percentile frame score.
    p1: f64,
    /// Lowest display-time-weighted mean over any one second of playback.
    worst_second: f64,
}

impl VmafScore {
    /// Whether the encode clears `min_vmaf` on average and in its low tail.
    fn passes(&self, min_vmaf: f64) -> bool {
        let tail = min_vmaf - LOW_TAIL_MARGIN;
        self.mean >= min_vmaf && self.p1 >= tail && self.worst_second >= tail
    }
}

impl std::fmt::Display for VmafScore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "VMAF {:.2} (p1 {:.2}, worst second {:.2})",
            self.mean, self.p1, self.worst_second
        )
    }
}

/// Summarise per-frame scores. `durations` holds each frame's display time in
/// seconds, so the one-second window follows playback time: a variable-frame-
/// rate clip can show one damaged frame for seconds.
fn summarize(frames: &[f64], durations: &[f64]) -> VmafScore {
    let mean = frames.iter().sum::<f64>() / frames.len() as f64;
    let mut sorted = frames.to_vec();
    sorted.sort_by(f64::total_cmp);
    let p1 = sorted[sorted.len() / 100];

    // Piecewise-constant score over time; area(t) integrates it from 0 to t.
    let mut starts = Vec::with_capacity(frames.len());
    let mut cum = Vec::with_capacity(frames.len() + 1);
    let (mut t, mut area) = (0.0, 0.0);
    cum.push(0.0);
    for (v, d) in frames.iter().zip(durations) {
        starts.push(t);
        t += d;
        area += v * d;
        cum.push(area);
    }
    let total = t;
    let window = total.min(1.0);
    let area_at = |t: f64| {
        let i = starts.partition_point(|&s| s <= t).saturating_sub(1);
        cum[i] + frames[i] * (t - starts[i]).clamp(0.0, durations[i])
    };
    // The minimum of a moving average of a step function sits where a window
    // edge meets a frame boundary.
    let worst_second = if window > 0.0 {
        starts
            .iter()
            .flat_map(|&s| [s, s - window])
            .chain(std::iter::once(total - window))
            .map(|a| a.clamp(0.0, total - window))
            .map(|a| (area_at(a + window) - area_at(a)) / window)
            .fold(f64::INFINITY, f64::min)
    } else {
        mean
    };
    VmafScore {
        mean,
        p1,
        worst_second,
    }
}

/// Display duration in seconds of every decoded frame of the first video
/// stream, in presentation order. Decoding the whole stream also proves it
/// decodes cleanly; packet counts differ from presented frames under edit
/// lists and discarded packets.
fn probe_frames(input: &Path) -> Result<Vec<f64>> {
    let out = run_clean(
        Command::new("ffprobe")
            .args(["-v", "error", "-select_streams", "v:0"])
            .args(["-show_entries", "frame=best_effort_timestamp_time"])
            .args(["-of", "csv=p=0"])
            .arg(input),
    )?;
    let pts: Vec<f64> = String::from_utf8_lossy(&out)
        .lines()
        .filter_map(|l| l.trim().trim_end_matches(',').parse().ok())
        .collect();
    let format = run_tool(
        Command::new("ffprobe")
            .args(["-v", "error", "-show_entries", "format=duration"])
            .args(["-of", "default=noprint_wrappers=1:nokey=1"])
            .arg(input),
    )?;
    let end: f64 = String::from_utf8_lossy(&format)
        .trim()
        .parse()
        .unwrap_or(0.0);
    let mut durations: Vec<f64> = pts.windows(2).map(|w| (w[1] - w[0]).max(0.0)).collect();
    if let Some(&last) = pts.last() {
        // The last frame lasts until the container ends, or one typical frame.
        let typical = durations.last().copied().unwrap_or(0.0);
        durations.push(if end > last { end - last } else { typical });
    }
    Ok(durations)
}

/// VMAF of `distorted` against `reference`, both decoded with their display
/// rotation applied and compared at 10-bit with VMAF's default model. The 4K
/// model was tried and is more lenient on native 4K blur, so it is not used.
///
/// Frames are paired by index, not timestamp: a variable-frame-rate source and
/// its re-muxed encode round timestamps differently, and timestamp pairing then
/// scores many frames against a neighbour (a 60 fps phone clip read VMAF ~54
/// instead of ~95). Index pairing is only sound for equal streams, so both
/// files must decode without errors, both decoded frame counts and the scored
/// frame count must match, and framesync may not repeat or extend frames. Any
/// mismatch keeps the original.
fn score_vmaf(distorted: &Path, reference: &Path, log: &Path) -> Result<VmafScore> {
    let durations = probe_frames(reference)?;
    let enc_frames = probe_frames(distorted)?.len();
    let ref_frames = durations.len();
    if ref_frames == 0 || enc_frames != ref_frames {
        return Err(PipelineError::Keep(format!(
            "encode has {enc_frames} frames, source has {ref_frames}"
        )));
    }

    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let graph = format!(
        "[0:v]format=yuv420p10le,settb=1/1000,setpts=N[d];\
         [1:v]format=yuv420p10le,settb=1/1000,setpts=N[r];\
         [d][r]libvmaf=n_threads={threads}:shortest=1:repeatlast=0:\
         log_fmt=json:log_path={}",
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
    let frames: Vec<f64> = json["frames"]
        .as_array()
        .map(|frames| {
            frames
                .iter()
                .filter_map(|f| f["metrics"]["vmaf"].as_f64())
                .collect()
        })
        .unwrap_or_default();
    if frames.len() != ref_frames {
        return Err(PipelineError::Keep(format!(
            "VMAF scored {} frames, source has {ref_frames}",
            frames.len()
        )));
    }
    Ok(summarize(&frames, &durations))
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

        let (codec, transfer) = probe_video_codec(&input_path)?;
        if codec.is_empty() || codec == "av1" {
            return Err(PipelineError::Keep(format!(
                "source video codec is {:?}",
                if codec.is_empty() { "none" } else { &codec }
            )));
        }
        if HDR_TRANSFERS.contains(&transfer.as_str()) {
            return Err(PipelineError::Keep(format!(
                "HDR source ({transfer}) is outside the SDR VMAF gate"
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

            let vmaf = score_vmaf(&output_path, &input_path, &tmp.path().join("vmaf.json"))?;
            scored = Some((crf, vmaf, byte_count));
            if vmaf.passes(self.min_vmaf) {
                break;
            }
        }
        let Some((crf, vmaf, byte_count)) = scored.filter(|(_, v, _)| v.passes(self.min_vmaf))
        else {
            let (crf, vmaf, _) = scored.expect("the CRF ladder is never empty");
            return Err(PipelineError::Keep(format!(
                "{vmaf} at CRF {crf} misses the {:.2} floor (tail {:.2})",
                self.min_vmaf,
                self.min_vmaf - LOW_TAIL_MARGIN
            )));
        };
        tracing::debug!("{} encoded {codec} to AV1 at CRF {crf}, {vmaf}", asset.id);

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
                encoder: format!(
                    "{} crf={crf} vmaf={:.2} p1={:.2} worst_second={:.2}",
                    self.label(),
                    vmaf.mean,
                    vmaf.p1,
                    vmaf.worst_second
                ),
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

    #[test]
    fn low_tail_fails_an_encode_with_a_good_mean() {
        // Five black frames in five seconds at 60 fps: the mean still clears 95.
        let mut frames = vec![99.0; 300];
        frames[100..105].fill(0.0);
        let durations = vec![1.0 / 60.0; 300];
        let score = summarize(&frames, &durations);
        assert!(score.mean >= 95.0);
        assert!(!score.passes(95.0), "{score}");
        assert!(summarize(&[99.0; 300], &durations).passes(95.0));
    }

    #[test]
    fn worst_second_follows_display_time() {
        // One damaged frame held for two seconds of a VFR clip, among 299 good
        // frames: per-frame statistics stay high, but playback shows damage.
        let mut frames = vec![99.0; 300];
        frames[150] = 0.0;
        let mut durations = vec![1.0 / 60.0; 300];
        durations[150] = 2.0;
        let score = summarize(&frames, &durations);
        assert!(score.mean >= 95.0 && score.p1 >= 90.0, "{score}");
        assert!(score.worst_second < 1.0, "{score}");
        assert!(!score.passes(95.0));
    }

    #[tokio::test]
    async fn hdr_source_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("hdr.mp4");
        run_tool(
            Command::new("ffmpeg")
                .args(["-nostdin", "-loglevel", "error", "-f", "lavfi"])
                .arg("-i")
                .arg(
                    "testsrc2=size=320x240:rate=30:duration=1,setparams=\
                     color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc",
                )
                .args(["-c:v", "libx264", "-qp", "0", "-pix_fmt", "yuv420p10le"])
                .arg(&src),
        )
        .unwrap();
        let result = SvtAv1UhqTranscoder::new(20)
            .with_preset(10)
            .transcode(Box::new(std::fs::File::open(&src).unwrap()), &asset())
            .await;
        assert!(matches!(result, Err(PipelineError::Keep(_))));
    }

    /// A damaged source decodes "successfully" with fewer frames; equal frame
    /// counts must not hide that.
    #[test]
    fn decode_errors_keep_the_original() {
        let dir = tempfile::tempdir().unwrap();
        let src = vfr_clip(dir.path());
        let whole = dir.path().join("whole.mp4");
        run_tool(
            Command::new("ffmpeg")
                .args(["-nostdin", "-loglevel", "error", "-i"])
                .arg(&src)
                .args(["-c", "copy", "-movflags", "+faststart"])
                .arg(&whole),
        )
        .unwrap();
        let bytes = std::fs::read(&whole).unwrap();
        let cut = dir.path().join("cut.mp4");
        std::fs::write(&cut, &bytes[..bytes.len() * 4 / 5]).unwrap();
        let result = score_vmaf(&cut, &cut, &dir.path().join("vmaf.json"));
        assert!(matches!(result, Err(PipelineError::Keep(_))), "{result:?}");
    }

    /// Presentation timestamps of the first video stream, in seconds.
    fn frame_pts(path: &Path) -> Vec<f64> {
        let out = run_tool(
            Command::new("ffprobe")
                .args(["-v", "error", "-select_streams", "v:0"])
                .args(["-show_entries", "frame=pts_time", "-of", "csv=p=0"])
                .arg(path),
        )
        .unwrap();
        String::from_utf8_lossy(&out)
            .lines()
            .map(|l| l.trim_end_matches(',').parse().unwrap())
            .collect()
    }

    /// Audio start minus video start, in seconds.
    fn av_offset(path: &Path) -> f64 {
        let start = |stream: &str| -> f64 {
            let out = run_tool(
                Command::new("ffprobe")
                    .args(["-v", "error", "-select_streams", stream])
                    .args(["-show_entries", "stream=start_time"])
                    .args(["-of", "default=noprint_wrappers=1:nokey=1"])
                    .arg(path),
            )
            .unwrap();
            String::from_utf8_lossy(&out).trim().parse().unwrap()
        };
        start("a:0") - start("v:0")
    }

    /// A lossless 60-frame 60 fps clip whose timestamps jitter like a phone's
    /// variable-frame-rate recording, with a sine audio track.
    fn vfr_clip(dir: &Path) -> std::path::PathBuf {
        let src = dir.join("vfr.mp4");
        run_tool(
            Command::new("ffmpeg")
                .args(["-nostdin", "-loglevel", "error", "-f", "lavfi", "-i"])
                .arg(
                    "testsrc2=size=320x240:rate=60:duration=1,noise=alls=6:allf=t,\
                     settb=1/90000,setpts=N*1497+mod(N\\,3)*400",
                )
                .args(["-f", "lavfi", "-i", "sine=f=440:d=1"])
                .args(["-fps_mode", "passthrough", "-enc_time_base:v", "1/90000"])
                .args(["-video_track_timescale", "90000"])
                .args(["-c:v", "libx264", "-qp", "0", "-pix_fmt", "yuv420p"])
                .args(["-c:a", "aac"])
                .arg(&src),
        )
        .unwrap();
        src
    }

    /// Variable-frame-rate sources (every phone clip) must be scored frame
    /// for frame (timestamp pairing read this clip as VMAF ~82) and keep their
    /// cadence and audio sync.
    #[tokio::test]
    async fn variable_frame_rate_source_keeps_timing_and_passes_the_gate() {
        let dir = tempfile::tempdir().unwrap();
        let src = vfr_clip(dir.path());

        let out = SvtAv1UhqTranscoder::new(20)
            .with_preset(10)
            .transcode(Box::new(std::fs::File::open(&src).unwrap()), &asset())
            .await
            .expect("a VFR encode passes the VMAF gate");
        let TranscodedPayload::File { path, _guard } = out.payload else {
            panic!("expected file payload");
        };

        let (want, got) = (frame_pts(&src), frame_pts(&path));
        assert_eq!(want.len(), got.len());
        for (w, g) in want.iter().zip(&got) {
            assert!((w - g).abs() < 1e-4, "frame at {w}s moved to {g}s");
        }
        assert!((av_offset(&src) - av_offset(&path)).abs() < 0.01);
    }

    /// Index pairing is only sound for equal frame counts; a missing or a
    /// repeated frame must keep the original rather than score high.
    #[test]
    fn frame_count_mismatch_keeps_the_original() {
        let dir = tempfile::tempdir().unwrap();
        let src = vfr_clip(dir.path());
        let variants = [
            ("short.mp4", vec!["-frames:v", "59"]),
            ("long.mp4", vec!["-vf", "loop=loop=1:size=1:start=30"]),
        ];
        for (name, args) in variants {
            let path = dir.path().join(name);
            run_tool(
                Command::new("ffmpeg")
                    .args(["-nostdin", "-loglevel", "error", "-i"])
                    .arg(&src)
                    .args(&args)
                    // Without passthrough, ffmpeg drops the repeated frame again.
                    .args(["-fps_mode", "passthrough"])
                    .args(["-an", "-c:v", "libx264", "-qp", "0", "-pix_fmt", "yuv420p"])
                    .arg(&path),
            )
            .unwrap();
            let result = score_vmaf(&path, &src, &dir.path().join("vmaf.json"));
            assert!(
                matches!(result, Err(PipelineError::Keep(_))),
                "{name}: {result:?}"
            );
        }
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
        assert_eq!(probe_video_codec(&path).unwrap().0, "av1");

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
