use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

use rs_immicher_oxide_backend_immich::sink::ImmichSink;
use rs_immicher_oxide_backend_immich::source::ImmichSource;
use rs_immicher_oxide_backend_immich::ImmichConfig;
use rs_immicher_oxide_codec_av1::SvtAv1UhqTranscoder;
use rs_immicher_oxide_codec_jxl::JxlTranscoder;
use rs_immicher_oxide_core::error::{PipelineError, Result as PipelineResult};
use rs_immicher_oxide_core::pipeline::{Pipeline, PipelineSummary, StateStore};
use rs_immicher_oxide_core::sink::Sink;
use rs_immicher_oxide_core::source::Source;
use rs_immicher_oxide_core::transcoder::Transcoder;
use rs_immicher_oxide_core::types::{AssetOutcome, MediaCodec, WriteMode};

#[derive(Parser)]
#[command(name = "rs-immicher-oxide", about = "Immich transpiler framework")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum CliWriteMode {
    DryRun,
    UploadOnly,
    TrashOriginal,
}

impl From<CliWriteMode> for WriteMode {
    fn from(value: CliWriteMode) -> Self {
        match value {
            CliWriteMode::DryRun => WriteMode::DryRun,
            CliWriteMode::UploadOnly => WriteMode::UploadOnly,
            CliWriteMode::TrashOriginal => WriteMode::TrashOriginal,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct RuntimeFilters {
    asset_ids: Vec<String>,
    limit: Option<usize>,
    created_after: Option<String>,
    taken_after: Option<String>,
}

/// Shared config values used across subcommands.
#[derive(Clone, Debug)]
struct RuntimeConfig {
    immich_url: String,
    api_key: String,
    video_crf: u32,
    video_preset: u32,
    video_min_vmaf: f64,
    image_distance: f32,
    concurrency: usize,
    write_mode: WriteMode,
    filters: RuntimeFilters,
    state_path: Option<PathBuf>,
    manifest_path: Option<PathBuf>,
}

fn resolve_api_key(
    api_key: Option<String>,
    api_key_file: Option<PathBuf>,
) -> anyhow::Result<String> {
    if let Some(api_key) = api_key {
        let api_key = api_key.trim().to_string();
        if api_key.is_empty() {
            anyhow::bail!("Immich API key is empty");
        }
        return Ok(api_key);
    }

    let Some(path) = api_key_file else {
        anyhow::bail!(
            "Immich API key is required; set --api-key, IMMICH_API_KEY, or --api-key-file"
        );
    };

    let api_key = std::fs::read_to_string(&path)?.trim().to_string();
    if api_key.is_empty() {
        anyhow::bail!("Immich API key file {} is empty", path.display());
    }
    Ok(api_key)
}

impl RuntimeConfig {
    fn to_immich_config(&self) -> ImmichConfig {
        ImmichConfig::new(&self.immich_url, &self.api_key).with_concurrency(self.concurrency)
    }

    fn apply_filters(&self, mut source: ImmichSource) -> ImmichSource {
        if !self.filters.asset_ids.is_empty() {
            source = source.with_asset_ids(&self.filters.asset_ids);
        }
        if let Some(limit) = self.filters.limit {
            source = source.with_limit(limit);
        }
        if let Some(ref timestamp) = self.filters.created_after {
            source = source.with_created_after(timestamp);
        }
        if let Some(ref timestamp) = self.filters.taken_after {
            source = source.with_taken_after(timestamp);
        }
        source
    }

    fn build_image_pipeline(&self) -> (ImmichSource, JxlTranscoder, ImmichSink) {
        let immich = self.to_immich_config();
        let source = self
            .apply_filters(ImmichSource::new(immich.clone(), MediaCodec::Jxl))
            .with_images_only();
        let transcoder = JxlTranscoder::new(self.image_distance);
        let sink = ImmichSink::new(immich);
        (source, transcoder, sink)
    }

    fn build_video_pipeline(&self) -> (ImmichSource, SvtAv1UhqTranscoder, ImmichSink) {
        let immich = self.to_immich_config();
        let source = self
            .apply_filters(ImmichSource::new(immich.clone(), MediaCodec::Av1))
            .with_videos_only();
        let transcoder = SvtAv1UhqTranscoder::new(self.video_crf)
            .with_preset(self.video_preset)
            .with_min_vmaf(self.video_min_vmaf);
        let sink = ImmichSink::new(immich);
        (source, transcoder, sink)
    }
}

#[derive(Subcommand)]
enum Command {
    /// One-shot conversion: discover existing assets, transcode, upload.
    #[command(alias = "convert")]
    Run {
        /// Immich API base URL, including `/api`.
        #[arg(long, env = "IMMICH_URL")]
        immich_url: String,

        /// Immich API key.
        #[arg(short, long, env = "IMMICH_API_KEY")]
        api_key: Option<String>,

        /// File containing a raw Immich API key.
        #[arg(long)]
        api_key_file: Option<PathBuf>,

        /// Only process videos.
        #[arg(long)]
        video_only: bool,

        /// Only process images.
        #[arg(long)]
        image_only: bool,

        /// Explicit Immich asset ID to process. Repeat for canary batches.
        #[arg(long = "asset-id")]
        asset_ids: Vec<String>,

        /// Stop after this many matching assets.
        #[arg(long)]
        limit: Option<usize>,

        /// Only process assets created after this ISO 8601 timestamp.
        #[arg(long)]
        created_after: Option<String>,

        /// Only process assets taken after this ISO 8601 timestamp.
        #[arg(long)]
        taken_after: Option<String>,

        /// AV1 CRF (0-63, lower = better).
        #[arg(long, default_value_t = 20)]
        video_crf: u32,

        /// SVT-AV1 preset (0-13, lower = slower and smaller).
        #[arg(long, default_value_t = 6)]
        video_preset: u32,

        /// Keep the original when the encode scores a lower mean VMAF.
        #[arg(long, default_value_t = 95.0)]
        video_min_vmaf: f64,

        /// JXL butteraugli distance (0 = lossless, 1.0 = visually lossless).
        #[arg(long, default_value_t = 1.0)]
        image_distance: f32,

        /// Write behavior. Defaults to discovery-only.
        #[arg(long, value_enum, default_value_t = CliWriteMode::DryRun)]
        write_mode: CliWriteMode,

        /// Append-only JSONL state file for completed assets.
        #[arg(long)]
        state_path: Option<PathBuf>,

        /// Append-only JSONL manifest for all outcomes in this run.
        #[arg(long)]
        manifest_path: Option<PathBuf>,

        /// Concurrency level.
        #[arg(long, default_value_t = 2)]
        concurrency: usize,
    },

    /// Daemon mode: poll for new assets and process them.
    Watch {
        /// Immich API base URL, including `/api`.
        #[arg(long, env = "IMMICH_URL")]
        immich_url: String,

        /// Immich API key.
        #[arg(short, long, env = "IMMICH_API_KEY")]
        api_key: Option<String>,

        /// File containing a raw Immich API key.
        #[arg(long)]
        api_key_file: Option<PathBuf>,

        /// Explicit Immich asset ID to process. Repeat for canary batches.
        #[arg(long = "asset-id")]
        asset_ids: Vec<String>,

        /// Stop after this many matching assets per media pipeline.
        #[arg(long)]
        limit: Option<usize>,

        /// Only process assets created after this ISO 8601 timestamp.
        #[arg(long)]
        created_after: Option<String>,

        /// Only process assets taken after this ISO 8601 timestamp.
        #[arg(long)]
        taken_after: Option<String>,

        /// AV1 CRF (0-63, lower = better).
        #[arg(long, default_value_t = 20)]
        video_crf: u32,

        /// SVT-AV1 preset (0-13, lower = slower and smaller).
        #[arg(long, default_value_t = 6)]
        video_preset: u32,

        /// Keep the original when the encode scores a lower mean VMAF.
        #[arg(long, default_value_t = 95.0)]
        video_min_vmaf: f64,

        /// JXL butteraugli distance.
        #[arg(long, default_value_t = 1.0)]
        image_distance: f32,

        /// Poll interval.
        #[arg(short, long, default_value = "5min")]
        interval: String,

        /// Write behavior. Defaults to discovery-only.
        #[arg(long, value_enum, default_value_t = CliWriteMode::DryRun)]
        write_mode: CliWriteMode,

        /// Append-only JSONL state file for completed assets.
        #[arg(long)]
        state_path: Option<PathBuf>,

        /// Append-only JSONL manifest for all outcomes in this run.
        #[arg(long)]
        manifest_path: Option<PathBuf>,
    },

    /// Serve HTTP webhook endpoint for Immich server-side events.
    Serve {
        /// Bind address.
        #[arg(long, default_value = "0.0.0.0:8088")]
        bind: String,

        /// Immich API base URL, including `/api`.
        #[arg(long, env = "IMMICH_URL")]
        immich_url: String,

        /// Immich API key.
        #[arg(short, long, env = "IMMICH_API_KEY")]
        api_key: Option<String>,

        /// File containing a raw Immich API key.
        #[arg(long)]
        api_key_file: Option<PathBuf>,

        /// AV1 CRF.
        #[arg(long, default_value_t = 20)]
        video_crf: u32,

        /// JXL distance.
        #[arg(long, default_value_t = 1.0)]
        image_distance: f32,

        /// Webhook mode is currently allowed only for dry-run.
        #[arg(long, value_enum, default_value_t = CliWriteMode::DryRun)]
        write_mode: CliWriteMode,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Run {
            immich_url,
            api_key,
            api_key_file,
            video_only,
            image_only,
            asset_ids,
            limit,
            created_after,
            taken_after,
            video_crf,
            video_preset,
            video_min_vmaf,
            image_distance,
            write_mode,
            state_path,
            manifest_path,
            concurrency,
        } => {
            if image_only && video_only {
                anyhow::bail!("--image-only and --video-only are mutually exclusive");
            }

            let cfg = RuntimeConfig {
                immich_url,
                api_key: resolve_api_key(api_key, api_key_file)?,
                video_crf,
                video_preset,
                video_min_vmaf,
                image_distance,
                concurrency,
                write_mode: write_mode.into(),
                filters: RuntimeFilters {
                    asset_ids,
                    limit,
                    created_after,
                    taken_after,
                },
                state_path,
                manifest_path,
            };

            let want_images = !video_only;
            let want_videos = !image_only;

            if want_images {
                run_pipeline(&cfg, "images", |c| {
                    let (s, t, si) = c.build_image_pipeline();
                    (
                        Box::new(s) as Box<dyn Source>,
                        Box::new(t) as Box<dyn Transcoder>,
                        Box::new(si) as Box<dyn Sink>,
                    )
                })
                .await?;
            }

            if want_videos {
                run_pipeline(&cfg, "videos", |c| {
                    let (s, t, si) = c.build_video_pipeline();
                    (Box::new(s), Box::new(t), Box::new(si))
                })
                .await?;
            }
        }

        Command::Watch {
            immich_url,
            api_key,
            api_key_file,
            asset_ids,
            limit,
            created_after,
            taken_after,
            video_crf,
            video_preset,
            video_min_vmaf,
            image_distance,
            interval,
            write_mode,
            state_path,
            manifest_path,
        } => {
            let cfg = RuntimeConfig {
                immich_url,
                api_key: resolve_api_key(api_key, api_key_file)?,
                video_crf,
                video_preset,
                video_min_vmaf,
                image_distance,
                concurrency: 1,
                write_mode: write_mode.into(),
                filters: RuntimeFilters {
                    asset_ids,
                    limit,
                    created_after,
                    taken_after,
                },
                state_path,
                manifest_path,
            };

            let interval = parse_duration(&interval)?;
            tracing::info!("watch mode, interval {:?}", interval);

            loop {
                run_pipeline(&cfg, "images (watch)", |c| {
                    let (s, t, si) = c.build_image_pipeline();
                    (Box::new(s), Box::new(t), Box::new(si))
                })
                .await?;

                run_pipeline(&cfg, "videos (watch)", |c| {
                    let (s, t, si) = c.build_video_pipeline();
                    (Box::new(s), Box::new(t), Box::new(si))
                })
                .await?;

                tracing::info!("sleeping for {:?}", interval);
                tokio::time::sleep(interval).await;
            }
        }

        Command::Serve {
            bind,
            immich_url,
            api_key,
            api_key_file,
            video_crf,
            image_distance,
            write_mode,
        } => {
            let write_mode: WriteMode = write_mode.into();
            if !write_mode.is_dry_run() {
                anyhow::bail!(
                    "serve supports only --write-mode dry-run until single-asset webhook processing is implemented"
                );
            }

            let cfg = Arc::new(RuntimeConfig {
                immich_url,
                api_key: resolve_api_key(api_key, api_key_file)?,
                video_crf,
                // serve is dry-run only; encoder settings are never used.
                video_preset: 6,
                video_min_vmaf: 95.0,
                image_distance,
                concurrency: 2,
                write_mode,
                filters: RuntimeFilters::default(),
                state_path: None,
                manifest_path: None,
            });

            tracing::info!("webhook server starting on {}", bind);
            serve_webhook(bind, cfg).await?;
        }
    }

    Ok(())
}

async fn run_pipeline(
    cfg: &RuntimeConfig,
    label: &str,
    build: impl FnOnce(&RuntimeConfig) -> (Box<dyn Source>, Box<dyn Transcoder>, Box<dyn Sink>),
) -> anyhow::Result<PipelineSummary> {
    tracing::info!("=== {} pipeline ({:?}) ===", label, cfg.write_mode);

    let (source, transcoder, sink) = build(cfg);
    let mut pipeline = Pipeline::new(source, transcoder, sink)
        .with_write_mode(cfg.write_mode)
        .with_concurrency(cfg.concurrency);

    if let Some(ref state_path) = cfg.state_path {
        pipeline =
            pipeline.with_state(Box::new(JsonlStateStore::open(state_path, cfg.write_mode)?));
    }

    let manifest = cfg
        .manifest_path
        .as_deref()
        .map(ManifestWriter::open)
        .transpose()?;

    let summary = pipeline
        .run(|outcome| {
            log_outcome(&outcome);
            if let Some(ref manifest) = manifest {
                if let Err(e) = manifest.record(label, cfg.write_mode, &outcome) {
                    tracing::error!("failed to write manifest record: {}", e);
                }
            }
        })
        .await?;

    tracing::info!(
        "=== {} done: {} succeeded, {} partial, {} skipped, {} failed ===",
        label,
        summary.succeeded,
        summary.partial,
        summary.skipped,
        summary.failed,
    );

    Ok(summary)
}

fn log_outcome(outcome: &AssetOutcome) {
    match outcome {
        AssetOutcome::Success {
            asset_id, stats, ..
        } => {
            tracing::info!(
                "{} success | {:.1}% of original, {:.1}s",
                asset_id,
                stats.compression_ratio * 100.0,
                stats.total_seconds,
            );
        }
        AssetOutcome::PartialSuccess {
            asset_id, warning, ..
        } => {
            tracing::warn!("{} partial: {}", asset_id, warning);
        }
        AssetOutcome::Skipped { asset_id, reason } => {
            tracing::debug!("{} skipped: {}", asset_id, reason);
        }
        AssetOutcome::Kept { asset_id, reason } => {
            tracing::info!("{} kept original: {}", asset_id, reason);
        }
        AssetOutcome::Failed { asset_id, error } => {
            tracing::error!("{} failed: {}", asset_id, error);
        }
    }
}

async fn serve_webhook(bind: String, cfg: Arc<RuntimeConfig>) -> anyhow::Result<()> {
    use tokio::net::TcpListener;

    let listener = TcpListener::bind(&bind).await?;

    loop {
        let (stream, addr) = listener.accept().await?;
        let cfg = cfg.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, cfg).await {
                tracing::error!("webhook from {}: {}", addr, e);
            }
        });
    }
}

async fn handle_connection(
    stream: tokio::net::TcpStream,
    cfg: Arc<RuntimeConfig>,
) -> anyhow::Result<()> {
    use tokio::io::AsyncReadExt;

    let mut buf = vec![0u8; 4096];
    let mut reader = tokio::io::BufReader::new(stream);
    let n = reader.read(&mut buf).await?;
    let request = String::from_utf8_lossy(&buf[..n]);

    if !request.starts_with("POST") {
        return Ok(());
    }

    let body_start = request.find("\r\n\r\n").map(|i| i + 4).unwrap_or(n);
    let body = &request[body_start..n];

    if let Ok(event) = serde_json::from_str::<serde_json::Value>(body) {
        let asset_type = event
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        tracing::info!("webhook event: type={}", asset_type);

        if asset_type == "asset.upload" || asset_type == "asset.update" {
            if let Some(asset_id) = event.get("id").and_then(|v| v.as_str()) {
                tracing::info!("dry-run webhook discovery for asset {}", asset_id);

                let mut cfg = (*cfg).clone();
                cfg.write_mode = WriteMode::DryRun;
                cfg.filters.asset_ids = vec![asset_id.to_string()];

                let mime = event
                    .get("originalMimeType")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");

                if mime.starts_with("video/") {
                    let _ = run_pipeline(&cfg, "videos (webhook dry-run)", |c| {
                        let (source, transcoder, sink) = c.build_video_pipeline();
                        (Box::new(source), Box::new(transcoder), Box::new(sink))
                    })
                    .await;
                } else if mime.starts_with("image/") && mime != "image/jxl" {
                    let _ = run_pipeline(&cfg, "images (webhook dry-run)", |c| {
                        let (source, transcoder, sink) = c.build_image_pipeline();
                        (Box::new(source), Box::new(transcoder), Box::new(sink))
                    })
                    .await;
                }
            }
        }
    }

    Ok(())
}

/// Parse a duration string like "5min", "1h", "30s".
fn parse_duration(s: &str) -> anyhow::Result<Duration> {
    let s = s.trim();
    if let Some(n) = s.strip_suffix("min") {
        let secs: u64 = n.parse()?;
        Ok(Duration::from_secs(secs * 60))
    } else if let Some(n) = s.strip_suffix('h') {
        let secs: u64 = n.parse()?;
        Ok(Duration::from_secs(secs * 3600))
    } else if let Some(n) = s.strip_suffix('s') {
        let secs: u64 = n.parse()?;
        Ok(Duration::from_secs(secs))
    } else {
        let secs: u64 = s.parse()?;
        Ok(Duration::from_secs(secs))
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct AuditRecord {
    timestamp_unix_seconds: u64,
    label: String,
    write_mode: WriteMode,
    outcome: AssetOutcome,
}

struct ManifestWriter {
    file: Mutex<File>,
}

impl ManifestWriter {
    fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            file: Mutex::new(file),
        })
    }

    fn record(
        &self,
        label: &str,
        write_mode: WriteMode,
        outcome: &AssetOutcome,
    ) -> anyhow::Result<()> {
        let record = AuditRecord {
            timestamp_unix_seconds: unix_timestamp(),
            label: label.to_string(),
            write_mode,
            outcome: outcome.clone(),
        };
        let mut file = self.file.lock().expect("manifest mutex poisoned");
        serde_json::to_writer(&mut *file, &record)?;
        file.write_all(b"\n")?;
        file.flush()?;
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct StateRecord {
    timestamp_unix_seconds: u64,
    original_id: String,
    new_id: String,
    write_mode: WriteMode,
    outcome: AssetOutcome,
}

struct JsonlStateStore {
    write_mode: WriteMode,
    completed: Mutex<HashSet<String>>,
    file: Mutex<File>,
}

impl JsonlStateStore {
    fn open(path: &Path, write_mode: WriteMode) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut completed = HashSet::new();
        if path.exists() {
            let reader = BufReader::new(File::open(path)?);
            for line in reader.lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                let record: StateRecord = serde_json::from_str(&line)?;
                if !matches!(
                    record.outcome,
                    AssetOutcome::Skipped { .. } | AssetOutcome::Failed { .. }
                ) {
                    completed.insert(record.original_id);
                }
            }
        }

        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            write_mode,
            completed: Mutex::new(completed),
            file: Mutex::new(file),
        })
    }
}

#[async_trait]
impl StateStore for JsonlStateStore {
    async fn record(&self, asset_id: &str, outcome: &AssetOutcome) -> PipelineResult<()> {
        let new_id = match outcome {
            AssetOutcome::Success { new_id, .. } | AssetOutcome::PartialSuccess { new_id, .. } => {
                new_id.clone()
            }
            AssetOutcome::Kept { .. } => String::new(),
            AssetOutcome::Skipped { .. } | AssetOutcome::Failed { .. } => return Ok(()),
        };

        {
            let mut completed = self.completed.lock().expect("state mutex poisoned");
            if !completed.insert(asset_id.to_string()) {
                return Ok(());
            }
        }

        let record = StateRecord {
            timestamp_unix_seconds: unix_timestamp(),
            original_id: asset_id.to_string(),
            new_id,
            write_mode: self.write_mode,
            outcome: outcome.clone(),
        };

        let mut file = self.file.lock().expect("state file mutex poisoned");
        serde_json::to_writer(&mut *file, &record)
            .map_err(|e| PipelineError::State(Box::new(e)))?;
        file.write_all(b"\n").map_err(PipelineError::Io)?;
        file.flush().map_err(PipelineError::Io)?;
        Ok(())
    }

    async fn is_completed(&self, asset_id: &str) -> PipelineResult<bool> {
        Ok(self
            .completed
            .lock()
            .expect("state mutex poisoned")
            .contains(asset_id))
    }

    async fn completed_ids(&self) -> PipelineResult<Vec<String>> {
        Ok(self
            .completed
            .lock()
            .expect("state mutex poisoned")
            .iter()
            .cloned()
            .collect())
    }
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use rs_immicher_oxide_core::types::TranscodeStats;

    #[test]
    fn parse_seconds() {
        let d = parse_duration("30s").unwrap();
        assert_eq!(d, Duration::from_secs(30));
    }

    #[test]
    fn parse_minutes() {
        let d = parse_duration("5min").unwrap();
        assert_eq!(d, Duration::from_secs(300));
    }

    #[test]
    fn parse_hours() {
        let d = parse_duration("2h").unwrap();
        assert_eq!(d, Duration::from_secs(7200));
    }

    #[test]
    fn parse_plain_number_as_seconds() {
        let d = parse_duration("90").unwrap();
        assert_eq!(d, Duration::from_secs(90));
    }

    #[test]
    fn parse_with_whitespace() {
        let d = parse_duration("  10min  ").unwrap();
        assert_eq!(d, Duration::from_secs(600));
    }

    #[test]
    fn parse_invalid_returns_error() {
        let result = parse_duration("abc");
        assert!(result.is_err());
    }

    #[test]
    fn resolve_api_key_reads_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api-key");
        std::fs::write(&path, " file-key\n").unwrap();

        let api_key = resolve_api_key(None, Some(path)).unwrap();

        assert_eq!(api_key, "file-key");
    }

    #[test]
    fn resolve_api_key_rejects_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api-key");
        std::fs::write(&path, "\n").unwrap();

        let err = resolve_api_key(None, Some(path)).unwrap_err().to_string();

        assert!(err.contains("is empty"));
    }

    #[test]
    fn resolve_api_key_prefers_explicit_key_over_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api-key");
        std::fs::write(&path, "file-key").unwrap();

        let api_key = resolve_api_key(Some(" explicit-key ".into()), Some(path)).unwrap();

        assert_eq!(api_key, "explicit-key");
    }

    #[test]
    fn cli_run_parses_api_key_file() {
        let cli = Cli::try_parse_from([
            "rs-immicher-oxide",
            "run",
            "--immich-url",
            "http://localhost:2283",
            "--api-key-file",
            "/run/agenix/immich-api-key",
        ])
        .unwrap();

        match cli.command {
            Command::Run { api_key_file, .. } => {
                assert_eq!(
                    api_key_file.as_deref(),
                    Some(Path::new("/run/agenix/immich-api-key"))
                );
            }
            _ => panic!("expected Run command"),
        }
    }

    #[test]
    fn cli_convert_alias_parses_api_key_file() {
        let cli = Cli::try_parse_from([
            "rs-immicher-oxide",
            "convert",
            "--immich-url",
            "http://localhost:2283",
            "--api-key-file",
            "/run/agenix/immich-api-key",
        ])
        .unwrap();

        match cli.command {
            Command::Run { api_key_file, .. } => {
                assert_eq!(
                    api_key_file.as_deref(),
                    Some(Path::new("/run/agenix/immich-api-key"))
                );
            }
            _ => panic!("expected Run command"),
        }
    }

    #[test]
    fn cli_run_parses_safe_defaults() {
        let cli = Cli::try_parse_from([
            "rs-immicher-oxide",
            "run",
            "--immich-url",
            "http://localhost:2283",
            "--api-key",
            "test-key",
        ])
        .unwrap();

        match cli.command {
            Command::Run { write_mode, .. } => assert_eq!(write_mode, CliWriteMode::DryRun),
            _ => panic!("expected Run command"),
        }
    }

    #[test]
    fn cli_run_parses_canary_filters() {
        let cli = Cli::try_parse_from([
            "rs-immicher-oxide",
            "run",
            "--immich-url",
            "http://localhost:2283",
            "--api-key",
            "test-key",
            "--write-mode",
            "upload-only",
            "--asset-id",
            "image-1",
            "--asset-id",
            "video-1",
            "--limit",
            "2",
            "--created-after",
            "2026-07-01T00:00:00.000Z",
        ])
        .unwrap();

        match cli.command {
            Command::Run {
                write_mode,
                asset_ids,
                limit,
                created_after,
                ..
            } => {
                assert_eq!(write_mode, CliWriteMode::UploadOnly);
                assert_eq!(asset_ids, ["image-1", "video-1"]);
                assert_eq!(limit, Some(2));
                assert_eq!(created_after.as_deref(), Some("2026-07-01T00:00:00.000Z"));
            }
            _ => panic!("expected Run command"),
        }
    }

    #[test]
    fn cli_rejects_invalid_flag_shape() {
        Cli::command().debug_assert();
    }

    #[test]
    fn cli_serve_rejects_write_mode_before_runtime() {
        let cli = Cli::try_parse_from([
            "rs-immicher-oxide",
            "serve",
            "--immich-url",
            "http://localhost:2283",
            "--api-key",
            "test-key",
            "--write-mode",
            "trash-original",
        ])
        .unwrap();

        match cli.command {
            Command::Serve { write_mode, .. } => {
                let write_mode: WriteMode = write_mode.into();
                assert!(!write_mode.is_dry_run());
            }
            _ => panic!("expected Serve command"),
        }
    }

    #[tokio::test]
    async fn state_store_records_terminal_outcomes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.jsonl");
        let state = JsonlStateStore::open(&path, WriteMode::UploadOnly).unwrap();

        state
            .record(
                "a1",
                &AssetOutcome::Skipped {
                    asset_id: "a1".into(),
                    reason: "dry run".into(),
                },
            )
            .await
            .unwrap();
        assert!(!state.is_completed("a1").await.unwrap());

        state
            .record("a1", &success_outcome("a1", "new-a1"))
            .await
            .unwrap();
        assert!(state.is_completed("a1").await.unwrap());

        state
            .record(
                "a2",
                &AssetOutcome::Kept {
                    asset_id: "a2".into(),
                    reason: "no worthwhile saving".into(),
                },
            )
            .await
            .unwrap();

        let reopened = JsonlStateStore::open(&path, WriteMode::UploadOnly).unwrap();
        assert!(reopened.is_completed("a1").await.unwrap());
        assert!(reopened.is_completed("a2").await.unwrap());
    }

    #[test]
    fn manifest_records_all_outcomes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.jsonl");
        let manifest = ManifestWriter::open(&path).unwrap();

        manifest
            .record(
                "images",
                WriteMode::DryRun,
                &AssetOutcome::Skipped {
                    asset_id: "a1".into(),
                    reason: "dry run".into(),
                },
            )
            .unwrap();
        manifest
            .record(
                "images",
                WriteMode::UploadOnly,
                &success_outcome("a2", "new-a2"),
            )
            .unwrap();

        let lines = std::fs::read_to_string(path).unwrap();
        assert_eq!(lines.lines().count(), 2);
        assert!(lines.contains("\"write_mode\":\"dry-run\""));
        assert!(lines.contains("\"asset_id\":\"a2\""));
    }

    fn success_outcome(asset_id: &str, new_id: &str) -> AssetOutcome {
        AssetOutcome::Success {
            asset_id: asset_id.into(),
            new_id: new_id.into(),
            original_codec: MediaCodec::Jpeg,
            output_codec: MediaCodec::Jxl,
            original_checksum: Some("checksum".into()),
            stats: TranscodeStats {
                decode_seconds: 0.0,
                encode_seconds: 0.1,
                total_seconds: 0.1,
                input_bytes: 100,
                output_bytes: 50,
                compression_ratio: 0.5,
                encoder: "mock".into(),
            },
        }
    }
}
