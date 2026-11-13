use async_trait::async_trait;
use futures::StreamExt;
use std::sync::Arc;

use crate::error::{PipelineError, Result};
use crate::sink::Sink;
use crate::source::Source;
use crate::transcoder::Transcoder;
use crate::types::{Asset, AssetOutcome, WriteMode};

/// State persistence for resumable pipeline runs.
#[async_trait]
pub trait StateStore: Send + Sync {
    /// Mark an asset as completed with the given outcome.
    async fn record(&self, asset_id: &str, outcome: &AssetOutcome) -> Result<()>;

    /// Check if an asset has already been completed.
    async fn is_completed(&self, asset_id: &str) -> Result<bool>;

    /// Return all completed asset IDs.
    async fn completed_ids(&self) -> Result<Vec<String>>;
}

/// The main pipeline orchestrator.
///
/// Wires a `Source`, `Transcoder`, and `Sink` together with optional
/// state tracking for resumability.
pub struct Pipeline {
    source: Arc<dyn Source>,
    transcoder: Arc<dyn Transcoder>,
    sink: Arc<dyn Sink>,
    state: Option<Arc<dyn StateStore>>,
    write_mode: WriteMode,
    concurrency: usize,
}

impl Pipeline {
    /// Create a new pipeline.
    pub fn new(
        source: Box<dyn Source>,
        transcoder: Box<dyn Transcoder>,
        sink: Box<dyn Sink>,
    ) -> Self {
        Self {
            source: source.into(),
            transcoder: transcoder.into(),
            sink: sink.into(),
            state: None,
            write_mode: WriteMode::DryRun,
            concurrency: 1,
        }
    }

    /// Attach a state store for resumability.
    pub fn with_state(mut self, state: Box<dyn StateStore>) -> Self {
        self.state = Some(state.into());
        self
    }

    /// Enable dry-run mode (discover + log only, no transcode or upload).
    pub fn with_dry_run(mut self, dry_run: bool) -> Self {
        self.write_mode = if dry_run {
            WriteMode::DryRun
        } else {
            WriteMode::TrashOriginal
        };
        self
    }

    /// Set the write mode for this pipeline.
    pub fn with_write_mode(mut self, write_mode: WriteMode) -> Self {
        self.write_mode = write_mode;
        self
    }

    /// Set the number of assets to process concurrently.
    pub fn with_concurrency(mut self, concurrency: usize) -> Self {
        self.concurrency = concurrency.max(1);
        self
    }

    /// Run the pipeline: discover, transcode, store.
    pub async fn run(
        &mut self,
        mut on_outcome: impl FnMut(AssetOutcome),
    ) -> Result<PipelineSummary> {
        let mut summary = PipelineSummary::default();
        let stream = self.source.discover().await?;
        let source = Arc::clone(&self.source);
        let transcoder = Arc::clone(&self.transcoder);
        let sink = Arc::clone(&self.sink);
        let state = self.state.clone();
        let write_mode = self.write_mode;

        let mut outcomes = stream
            .map(|asset_result| {
                let source = Arc::clone(&source);
                let transcoder = Arc::clone(&transcoder);
                let sink = Arc::clone(&sink);
                let state = state.clone();
                async move {
                    let asset = asset_result?;
                    Ok::<AssetOutcome, crate::error::PipelineError>(
                        process_asset(source, transcoder, sink, state, write_mode, asset).await,
                    )
                }
            })
            .buffer_unordered(self.concurrency);

        while let Some(outcome) = outcomes.next().await {
            let outcome = outcome?;
            summary.record(&outcome);
            on_outcome(outcome);
        }

        Ok(summary)
    }
}

/// Summary of a pipeline run.
#[derive(Debug, Clone, Default)]
pub struct PipelineSummary {
    pub succeeded: u64,
    pub partial: u64,
    pub skipped: u64,
    pub failed: u64,
}

impl PipelineSummary {
    pub fn total(&self) -> u64 {
        self.succeeded + self.partial + self.skipped + self.failed
    }

    fn record(&mut self, outcome: &AssetOutcome) {
        match outcome {
            AssetOutcome::Success { .. } => self.succeeded += 1,
            AssetOutcome::PartialSuccess { .. } => self.partial += 1,
            AssetOutcome::Skipped { .. } | AssetOutcome::Kept { .. } => self.skipped += 1,
            AssetOutcome::Failed { .. } => self.failed += 1,
        }
    }
}

async fn process_asset(
    source: Arc<dyn Source>,
    transcoder: Arc<dyn Transcoder>,
    sink: Arc<dyn Sink>,
    state: Option<Arc<dyn StateStore>>,
    write_mode: WriteMode,
    asset: Asset,
) -> AssetOutcome {
    if let Some(ref state) = state {
        if state.is_completed(&asset.id).await.unwrap_or(false) {
            return AssetOutcome::Skipped {
                asset_id: asset.id,
                reason: "already completed".into(),
            };
        }
    }

    let outcome = if !transcoder.can_handle(&asset) {
        AssetOutcome::Skipped {
            asset_id: asset.id.clone(),
            reason: format!(
                "codec {:?} not supported by {}",
                asset.codec,
                transcoder.label()
            ),
        }
    } else if write_mode.is_dry_run() {
        AssetOutcome::Skipped {
            asset_id: asset.id.clone(),
            reason: "dry run".into(),
        }
    } else {
        process_opened_asset(&*source, &*transcoder, &*sink, write_mode, &asset).await
    };

    // A partial success already uploaded a replacement and a kept original was
    // judged on purpose; redoing either on the next poll duplicates work.
    if matches!(
        outcome,
        AssetOutcome::Success { .. }
            | AssetOutcome::PartialSuccess { .. }
            | AssetOutcome::Kept { .. }
    ) {
        if let Some(ref state) = state {
            let _ = state.record(&asset.id, &outcome).await;
        }
    }

    outcome
}

async fn process_opened_asset(
    source: &dyn Source,
    transcoder: &dyn Transcoder,
    sink: &dyn Sink,
    write_mode: WriteMode,
    asset: &Asset,
) -> AssetOutcome {
    let input = match source.open_original(asset).await {
        Ok(stream) => stream,
        Err(e) => {
            return AssetOutcome::Failed {
                asset_id: asset.id.clone(),
                error: format!("failed to open original: {e}"),
            };
        }
    };

    let mut transcoded = match transcoder.transcode(input, asset).await {
        Ok(t) => t,
        // Policy decisions and decode/encode failures are deterministic:
        // keep the original instead of re-encoding it on every poll.
        Err(PipelineError::Keep(reason)) => {
            return AssetOutcome::Kept {
                asset_id: asset.id.clone(),
                reason,
            };
        }
        Err(e @ PipelineError::Transcoder(_)) => {
            return AssetOutcome::Kept {
                asset_id: asset.id.clone(),
                reason: format!("transcode failed: {e}"),
            };
        }
        Err(e) => {
            return AssetOutcome::Failed {
                asset_id: asset.id.clone(),
                error: format!("transcode failed: {e}"),
            };
        }
    };

    sink.process(asset, &mut transcoded, write_mode)
        .await
        .unwrap_or_else(|e| AssetOutcome::Failed {
            asset_id: asset.id.clone(),
            error: format!("sink processing failed: {e}"),
        })
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use async_trait::async_trait;
    use futures::stream::BoxStream;

    use super::*;
    use crate::error::Result;
    use crate::sink::Sink;
    use crate::source::Source;
    use crate::transcoder::Transcoder;
    use crate::types::{
        Asset, MediaCodec, MediaKind, TranscodeStats, TranscodedAsset, TranscodedPayload,
    };

    // ── Mock Source ──────────────────────────────────────────────

    struct MockSource {
        assets: Vec<Asset>,
        open_ok: bool,
    }

    impl MockSource {
        fn new(assets: Vec<Asset>) -> Self {
            Self {
                assets,
                open_ok: true,
            }
        }

        fn with_failures(assets: Vec<Asset>, open_ok: bool) -> Self {
            Self { assets, open_ok }
        }
    }

    #[async_trait]
    impl Source for MockSource {
        fn label(&self) -> &'static str {
            "mock"
        }

        async fn discover(&self) -> Result<BoxStream<'_, Result<Asset>>> {
            let items: Vec<Result<Asset>> = self.assets.iter().map(|a| Ok(a.clone())).collect();
            Ok(futures::stream::iter(items).boxed())
        }

        async fn open_original(
            &self,
            _asset: &Asset,
        ) -> Result<Box<dyn std::io::Read + Send + Unpin + 'static>> {
            if self.open_ok {
                Ok(Box::new(Cursor::new(b"mock-pixels".to_vec())))
            } else {
                Err(crate::error::PipelineError::Io(std::io::Error::other(
                    "open failed",
                )))
            }
        }
    }

    // ── Mock Transcoder ──────────────────────────────────────────

    struct MockTranscoder {
        supported: &'static [MediaCodec],
        output_codec: MediaCodec,
        kind: MediaKind,
        should_fail: bool,
        delayed_completion: bool,
    }

    impl MockTranscoder {
        fn new(supported: &'static [MediaCodec], output: MediaCodec, kind: MediaKind) -> Self {
            Self {
                supported,
                output_codec: output,
                kind,
                should_fail: false,
                delayed_completion: false,
            }
        }

        fn with_failure(mut self) -> Self {
            self.should_fail = true;
            self
        }

        fn with_delayed_completion(mut self) -> Self {
            self.delayed_completion = true;
            self
        }
    }

    #[async_trait]
    impl Transcoder for MockTranscoder {
        fn label(&self) -> &'static str {
            "mock-transcoder"
        }

        fn kind(&self) -> MediaKind {
            self.kind
        }

        fn input_codecs(&self) -> &'static [MediaCodec] {
            self.supported
        }

        fn output_codec(&self) -> MediaCodec {
            self.output_codec
        }

        async fn transcode(
            &self,
            _input: Box<dyn std::io::Read + Send + Unpin + 'static>,
            asset: &Asset,
        ) -> Result<TranscodedAsset> {
            if self.delayed_completion {
                let millis = if asset.id == "slow" { 50 } else { 1 };
                tokio::time::sleep(std::time::Duration::from_millis(millis)).await;
            }

            if self.should_fail {
                return Err(crate::error::PipelineError::Transcoder("mock error".into()));
            }
            Ok(TranscodedAsset {
                original_id: asset.id.clone(),
                codec: self.output_codec,
                kind: self.kind,
                payload: TranscodedPayload::Reader(Box::new(Cursor::new(b"transcoded"))),
                byte_count: 10,
                original_checksum: None,
                stats: TranscodeStats {
                    decode_seconds: 0.0,
                    encode_seconds: 0.1,
                    total_seconds: 0.1,
                    input_bytes: 10,
                    output_bytes: 10,
                    compression_ratio: 1.0,
                    encoder: "mock".into(),
                },
            })
        }
    }

    // ── Mock Sink ────────────────────────────────────────────────

    struct MockSink {
        store_fail: bool,
        outcomes: std::sync::Mutex<Vec<AssetOutcome>>,
        delete_count: std::sync::Arc<std::sync::Mutex<u64>>,
    }

    impl MockSink {
        fn new() -> Self {
            Self {
                store_fail: false,
                outcomes: std::sync::Mutex::new(Vec::new()),
                delete_count: std::sync::Arc::new(std::sync::Mutex::new(0)),
            }
        }

        fn delete_count(&self) -> std::sync::Arc<std::sync::Mutex<u64>> {
            std::sync::Arc::clone(&self.delete_count)
        }

        #[allow(dead_code)]
        fn outcomes(&self) -> Vec<AssetOutcome> {
            self.outcomes.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Sink for MockSink {
        fn label(&self) -> &'static str {
            "mock"
        }

        async fn store(
            &self,
            _original: &Asset,
            _transcoded: &mut TranscodedAsset,
        ) -> Result<String> {
            if self.store_fail {
                Err(crate::error::PipelineError::Sink("store failed".into()))
            } else {
                Ok("new-id".into())
            }
        }

        async fn copy_metadata(&self, _original: &Asset, _new_id: &str) -> Result<()> {
            Ok(())
        }

        async fn verify(
            &self,
            _original: &Asset,
            _output_codec: MediaCodec,
            _byte_count: u64,
            _new_id: &str,
        ) -> Result<()> {
            Ok(())
        }

        async fn delete_original(&self, _asset: &Asset) -> Result<()> {
            *self.delete_count.lock().unwrap() += 1;
            Ok(())
        }
    }

    // ── Mock State Store ─────────────────────────────────────────

    struct MockState {
        completed: std::sync::Mutex<Vec<String>>,
    }

    impl MockState {
        fn new() -> Self {
            Self {
                completed: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl StateStore for MockState {
        async fn record(&self, asset_id: &str, _outcome: &AssetOutcome) -> Result<()> {
            self.completed.lock().unwrap().push(asset_id.to_string());
            Ok(())
        }

        async fn is_completed(&self, asset_id: &str) -> Result<bool> {
            Ok(self
                .completed
                .lock()
                .unwrap()
                .contains(&asset_id.to_string()))
        }

        async fn completed_ids(&self) -> Result<Vec<String>> {
            Ok(self.completed.lock().unwrap().clone())
        }
    }

    struct RecordingState {
        completed: std::sync::Mutex<Vec<String>>,
        records: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl RecordingState {
        fn new(records: std::sync::Arc<std::sync::Mutex<Vec<String>>>) -> Self {
            Self {
                completed: std::sync::Mutex::new(Vec::new()),
                records,
            }
        }
    }

    #[async_trait]
    impl StateStore for RecordingState {
        async fn record(&self, asset_id: &str, _outcome: &AssetOutcome) -> Result<()> {
            self.completed.lock().unwrap().push(asset_id.to_string());
            self.records.lock().unwrap().push(asset_id.to_string());
            Ok(())
        }

        async fn is_completed(&self, asset_id: &str) -> Result<bool> {
            Ok(self
                .completed
                .lock()
                .unwrap()
                .contains(&asset_id.to_string()))
        }

        async fn completed_ids(&self) -> Result<Vec<String>> {
            Ok(self.completed.lock().unwrap().clone())
        }
    }

    fn make_asset(id: &str, codec: MediaCodec, kind: MediaKind) -> Asset {
        Asset {
            id: id.into(),
            filename: format!("{id}.bin"),
            kind,
            codec,
            mime_type: None,
            size_bytes: Some(10),
            created_at: None,
            checksum: None,
            metadata: serde_json::json!({}),
        }
    }

    // ── Tests ────────────────────────────────────────────────────

    #[test]
    fn summary_default_is_all_zeros() {
        let s = PipelineSummary::default();
        assert_eq!(s.total(), 0);
        assert_eq!((s.succeeded, s.partial, s.skipped, s.failed), (0, 0, 0, 0));
    }

    #[test]
    fn summary_total_sums_all_categories() {
        let s = PipelineSummary {
            succeeded: 5,
            partial: 2,
            skipped: 3,
            failed: 1,
        };
        assert_eq!(s.total(), 11);
    }

    #[tokio::test]
    async fn pipeline_skips_unsupported_codec() {
        let asset = make_asset("a1", MediaCodec::Jpeg, MediaKind::Image);
        let source = MockSource::new(vec![asset]);
        let transcoder = MockTranscoder::new(&[MediaCodec::Png], MediaCodec::Jxl, MediaKind::Image);
        let sink = MockSink::new();
        let mut pipeline = Pipeline::new(Box::new(source), Box::new(transcoder), Box::new(sink))
            .with_write_mode(WriteMode::UploadOnly);

        let outcomes = std::sync::Mutex::new(Vec::new());
        let summary = pipeline
            .run(|o| {
                outcomes.lock().unwrap().push(o);
            })
            .await
            .unwrap();

        assert_eq!(summary.skipped, 1);
        assert_eq!(summary.total(), 1);
    }

    #[tokio::test]
    async fn dry_run_skips_all_assets() {
        let asset = make_asset("a1", MediaCodec::Jpeg, MediaKind::Image);
        let source = MockSource::new(vec![asset]);
        let transcoder =
            MockTranscoder::new(&[MediaCodec::Jpeg], MediaCodec::Jxl, MediaKind::Image);
        let sink = MockSink::new();
        let mut pipeline = Pipeline::new(Box::new(source), Box::new(transcoder), Box::new(sink))
            .with_dry_run(true);

        let summary = pipeline.run(|_| {}).await.unwrap();
        assert_eq!(summary.skipped, 1);
        assert_eq!(summary.succeeded, 0);
    }

    #[tokio::test]
    async fn pipeline_happy_path() {
        let asset = make_asset("a1", MediaCodec::Jpeg, MediaKind::Image);
        let source = MockSource::new(vec![asset]);
        let transcoder =
            MockTranscoder::new(&[MediaCodec::Jpeg], MediaCodec::Jxl, MediaKind::Image);
        let sink = MockSink::new();
        let mut pipeline = Pipeline::new(Box::new(source), Box::new(transcoder), Box::new(sink))
            .with_write_mode(WriteMode::UploadOnly);

        let summary = pipeline.run(|_| {}).await.unwrap();
        assert_eq!(summary.succeeded, 1);
        assert_eq!(summary.failed, 0);
    }

    #[tokio::test]
    async fn pipeline_defaults_to_dry_run() {
        let asset = make_asset("a1", MediaCodec::Jpeg, MediaKind::Image);
        let source = MockSource::new(vec![asset]);
        let transcoder =
            MockTranscoder::new(&[MediaCodec::Jpeg], MediaCodec::Jxl, MediaKind::Image);
        let sink = MockSink::new();
        let mut pipeline = Pipeline::new(Box::new(source), Box::new(transcoder), Box::new(sink));

        let summary = pipeline.run(|_| {}).await.unwrap();
        assert_eq!(summary.skipped, 1);
        assert_eq!(summary.succeeded, 0);
    }

    #[tokio::test]
    async fn upload_only_never_deletes_original() {
        let asset = make_asset("a1", MediaCodec::Jpeg, MediaKind::Image);
        let source = MockSource::new(vec![asset]);
        let transcoder =
            MockTranscoder::new(&[MediaCodec::Jpeg], MediaCodec::Jxl, MediaKind::Image);
        let sink = MockSink::new();
        let delete_count = sink.delete_count();
        let mut pipeline = Pipeline::new(Box::new(source), Box::new(transcoder), Box::new(sink))
            .with_write_mode(WriteMode::UploadOnly);

        let summary = pipeline.run(|_| {}).await.unwrap();
        assert_eq!(summary.succeeded, 1);
        assert_eq!(*delete_count.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn trash_original_deletes_once_after_verification() {
        let asset = make_asset("a1", MediaCodec::Jpeg, MediaKind::Image);
        let source = MockSource::new(vec![asset]);
        let transcoder =
            MockTranscoder::new(&[MediaCodec::Jpeg], MediaCodec::Jxl, MediaKind::Image);
        let sink = MockSink::new();
        let delete_count = sink.delete_count();
        let mut pipeline = Pipeline::new(Box::new(source), Box::new(transcoder), Box::new(sink))
            .with_write_mode(WriteMode::TrashOriginal);

        let summary = pipeline.run(|_| {}).await.unwrap();
        assert_eq!(summary.succeeded, 1);
        assert_eq!(*delete_count.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn transcode_failure_keeps_original_and_is_not_retried() {
        let asset = make_asset("a1", MediaCodec::Jpeg, MediaKind::Image);
        let source = MockSource::new(vec![asset]);
        let transcoder =
            MockTranscoder::new(&[MediaCodec::Jpeg], MediaCodec::Jxl, MediaKind::Image)
                .with_failure();
        let sink = MockSink::new();
        let records = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let state = RecordingState::new(std::sync::Arc::clone(&records));
        let mut pipeline = Pipeline::new(Box::new(source), Box::new(transcoder), Box::new(sink))
            .with_state(Box::new(state))
            .with_write_mode(WriteMode::UploadOnly);

        let mut kept = false;
        let summary = pipeline
            .run(|o| kept = matches!(o, AssetOutcome::Kept { .. }))
            .await
            .unwrap();
        assert!(kept);
        assert_eq!((summary.failed, summary.skipped), (0, 1));
        assert_eq!(records.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn pipeline_source_open_failure() {
        let asset = make_asset("a1", MediaCodec::Jpeg, MediaKind::Image);
        let source = MockSource::with_failures(vec![asset], false);
        let transcoder =
            MockTranscoder::new(&[MediaCodec::Jpeg], MediaCodec::Jxl, MediaKind::Image);
        let sink = MockSink::new();
        let mut pipeline = Pipeline::new(Box::new(source), Box::new(transcoder), Box::new(sink))
            .with_write_mode(WriteMode::UploadOnly);

        let summary = pipeline.run(|_| {}).await.unwrap();
        assert_eq!(summary.failed, 1);
    }

    #[tokio::test]
    async fn state_store_prevents_reprocessing() {
        let assets = vec![
            make_asset("a1", MediaCodec::Jpeg, MediaKind::Image),
            make_asset("a2", MediaCodec::Jpeg, MediaKind::Image),
        ];
        let source = MockSource::new(assets);
        let transcoder =
            MockTranscoder::new(&[MediaCodec::Jpeg], MediaCodec::Jxl, MediaKind::Image);
        let sink = MockSink::new();
        let state = MockState::new();

        // Mark a1 as already completed
        state
            .record(
                "a1",
                &AssetOutcome::Skipped {
                    asset_id: "a1".into(),
                    reason: "done".into(),
                },
            )
            .await
            .unwrap();

        let mut pipeline = Pipeline::new(Box::new(source), Box::new(transcoder), Box::new(sink))
            .with_state(Box::new(state))
            .with_write_mode(WriteMode::UploadOnly);

        let summary = pipeline.run(|_| {}).await.unwrap();
        // a1 skipped (already done), a2 succeeds
        assert_eq!(summary.skipped, 1);
        assert_eq!(summary.succeeded, 1);
    }

    #[tokio::test]
    async fn multiple_assets_all_succeed() {
        let assets = vec![
            make_asset("a1", MediaCodec::Jpeg, MediaKind::Image),
            make_asset("a2", MediaCodec::Jpeg, MediaKind::Image),
            make_asset("a3", MediaCodec::Jpeg, MediaKind::Image),
        ];
        let source = MockSource::new(assets);
        let transcoder =
            MockTranscoder::new(&[MediaCodec::Jpeg], MediaCodec::Jxl, MediaKind::Image);
        let sink = MockSink::new();
        let mut pipeline = Pipeline::new(Box::new(source), Box::new(transcoder), Box::new(sink))
            .with_write_mode(WriteMode::UploadOnly);

        let summary = pipeline.run(|_| {}).await.unwrap();
        assert_eq!(summary.succeeded, 3);
        assert_eq!(summary.total(), 3);
    }

    #[tokio::test]
    async fn concurrent_pipeline_preserves_summary_counts() {
        let assets = vec![
            make_asset("a1", MediaCodec::Jpeg, MediaKind::Image),
            make_asset("a2", MediaCodec::Png, MediaKind::Image),
            make_asset("a3", MediaCodec::Jpeg, MediaKind::Image),
        ];
        let source = MockSource::new(assets);
        let transcoder =
            MockTranscoder::new(&[MediaCodec::Jpeg], MediaCodec::Jxl, MediaKind::Image);
        let sink = MockSink::new();
        let mut pipeline = Pipeline::new(Box::new(source), Box::new(transcoder), Box::new(sink))
            .with_concurrency(2)
            .with_write_mode(WriteMode::UploadOnly);

        let summary = pipeline.run(|_| {}).await.unwrap();
        assert_eq!(summary.succeeded, 2);
        assert_eq!(summary.skipped, 1);
        assert_eq!(summary.total(), 3);
    }

    #[tokio::test]
    async fn concurrent_pipeline_reports_completion_order() {
        let assets = vec![
            make_asset("slow", MediaCodec::Jpeg, MediaKind::Image),
            make_asset("fast", MediaCodec::Jpeg, MediaKind::Image),
        ];
        let source = MockSource::new(assets);
        let transcoder =
            MockTranscoder::new(&[MediaCodec::Jpeg], MediaCodec::Jxl, MediaKind::Image)
                .with_delayed_completion();
        let sink = MockSink::new();
        let mut pipeline = Pipeline::new(Box::new(source), Box::new(transcoder), Box::new(sink))
            .with_concurrency(2)
            .with_write_mode(WriteMode::UploadOnly);

        let outcomes = std::sync::Mutex::new(Vec::new());
        let summary = pipeline
            .run(|outcome| {
                let asset_id = match outcome {
                    AssetOutcome::Success { asset_id, .. }
                    | AssetOutcome::PartialSuccess { asset_id, .. }
                    | AssetOutcome::Skipped { asset_id, .. }
                    | AssetOutcome::Kept { asset_id, .. }
                    | AssetOutcome::Failed { asset_id, .. } => asset_id,
                };
                outcomes.lock().unwrap().push(asset_id);
            })
            .await
            .unwrap();

        assert_eq!(summary.succeeded, 2);
        assert_eq!(&*outcomes.lock().unwrap(), &["fast", "slow"]);
    }

    #[tokio::test]
    async fn state_store_records_once_per_processed_asset() {
        let assets = vec![
            make_asset("a1", MediaCodec::Jpeg, MediaKind::Image),
            make_asset("a2", MediaCodec::Jpeg, MediaKind::Image),
            make_asset("a3", MediaCodec::Jpeg, MediaKind::Image),
        ];
        let source = MockSource::new(assets);
        let transcoder =
            MockTranscoder::new(&[MediaCodec::Jpeg], MediaCodec::Jxl, MediaKind::Image);
        let sink = MockSink::new();
        let records = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let state = RecordingState::new(std::sync::Arc::clone(&records));
        let mut pipeline = Pipeline::new(Box::new(source), Box::new(transcoder), Box::new(sink))
            .with_state(Box::new(state))
            .with_concurrency(2)
            .with_write_mode(WriteMode::UploadOnly);

        let summary = pipeline.run(|_| {}).await.unwrap();
        assert_eq!(summary.succeeded, 3);

        let mut records = records.lock().unwrap().clone();
        records.sort();
        assert_eq!(records, ["a1", "a2", "a3"]);
    }
}
