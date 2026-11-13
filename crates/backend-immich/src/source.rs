use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use std::collections::HashSet;
use std::io::{Read, Seek, Write};

use rs_immicher_oxide_core::codec as codec_helpers;
use rs_immicher_oxide_core::error::{PipelineError, Result};
use rs_immicher_oxide_core::source::Source;
use rs_immicher_oxide_core::types::{Asset, MediaCodec, MediaKind};

use crate::client::{AssetType, ImmichApiClient, MetadataSearchDto};
use crate::ImmichConfig;

/// Source that discovers and streams existing Immich library assets via the REST API.
pub struct ImmichSource {
    #[allow(dead_code)]
    config: ImmichConfig,
    client: ImmichApiClient,
    /// Filter to only IMAGE or VIDEO assets.
    asset_type_filter: Option<AssetType>,
    /// Optional filter: only assets taken after this ISO timestamp.
    taken_after: Option<String>,
    /// Optional filter: only assets created after this ISO timestamp.
    created_after: Option<String>,
    /// Optional explicit asset-id allowlist.
    asset_ids: Option<HashSet<String>>,
    /// Optional maximum number of assets yielded after all filters.
    limit: Option<usize>,
    /// Target codec — assets already in this format are skipped.
    target_codec: MediaCodec,
}

impl ImmichSource {
    pub fn new(config: ImmichConfig, target_codec: MediaCodec) -> Self {
        let client = ImmichApiClient::new(&config);
        Self {
            config,
            client,
            asset_type_filter: None,
            taken_after: None,
            created_after: None,
            asset_ids: None,
            limit: None,
            target_codec,
        }
    }

    /// Filter to only image assets.
    pub fn with_images_only(mut self) -> Self {
        self.asset_type_filter = Some(AssetType::Image);
        self
    }

    /// Filter to only video assets.
    pub fn with_videos_only(mut self) -> Self {
        self.asset_type_filter = Some(AssetType::Video);
        self
    }

    /// Only process assets taken after this ISO 8601 timestamp.
    pub fn with_taken_after(mut self, timestamp: &str) -> Self {
        self.taken_after = Some(timestamp.to_string());
        self
    }

    /// Only process assets created after this ISO 8601 timestamp.
    pub fn with_created_after(mut self, timestamp: &str) -> Self {
        self.created_after = Some(timestamp.to_string());
        self
    }

    /// Only process the given asset IDs.
    pub fn with_asset_ids(mut self, asset_ids: &[String]) -> Self {
        self.asset_ids = Some(asset_ids.iter().cloned().collect());
        self
    }

    /// Stop discovery after yielding `limit` matching assets.
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Map an Immich `AssetResponseDto` to our `Asset` type.
    fn map_asset(&self, dto: crate::client::AssetResponseDto) -> Asset {
        let mime = dto.original_mime_type.as_deref().unwrap_or("");
        let (kind, codec) = codec_helpers::codec_from_mime(mime);

        // If MIME didn't match, try extension
        let (kind, codec) = if codec == MediaCodec::Unknown {
            let ext = dto.original_file_name.rsplit('.').next().unwrap_or("");
            codec_helpers::codec_from_extension(ext)
        } else {
            (kind, codec)
        };

        // Override kind from the Immich type
        let kind = match dto.asset_type {
            AssetType::Image => MediaKind::Image,
            AssetType::Video => MediaKind::Video,
            _ => kind,
        };

        let exif = dto.exif_info.as_ref();
        let size = exif.and_then(|e| e.file_size_in_byte).map(|s| s as u64);

        Asset {
            id: dto.id,
            filename: dto.original_file_name,
            kind,
            codec,
            mime_type: dto.original_mime_type,
            size_bytes: size,
            created_at: dto.file_created_at,
            checksum: dto.checksum,
            metadata: serde_json::json!({
                "file_modified_at": dto.file_modified_at,
                "visibility": dto.visibility,
                "description": exif.and_then(|e| e.description.clone()),
                "rating": exif.and_then(|e| e.rating),
            }),
        }
    }
}

#[async_trait]
impl Source for ImmichSource {
    fn label(&self) -> &'static str {
        "immich-api"
    }

    async fn discover(&self) -> Result<BoxStream<'_, Result<Asset>>> {
        // Assets of partners are visible to this key but cannot be replaced by it.
        let owner_id = self
            .client
            .get_my_user()
            .await
            .map_err(|e| PipelineError::Source(Box::new(e)))?
            .id;
        let state = DiscoveryState {
            page: 1,
            page_size: 500,
            pending: Vec::new().into_iter(),
            done: false,
            seen: 0,
            yielded: 0,
            owner_id,
        };

        Ok(futures::stream::unfold(state, move |mut state| async move {
            loop {
                if let Some(asset) = state.pending.next() {
                    return Some((Ok(asset), state));
                }

                if state.done {
                    return None;
                }

                let dto = MetadataSearchDto {
                    page: state.page,
                    size: state.page_size,
                    asset_type: self.asset_type_filter.clone(),
                    taken_after: self.taken_after.clone(),
                    taken_before: None,
                    created_after: self.created_after.clone(),
                    with_deleted: Some(false),
                    with_exif: Some(true),
                    // Stacks keep a primary asset that /assets/copy does not move.
                    with_stacked: Some(false),
                };

                let result = match self.client.search_assets(&dto).await {
                    Ok(result) => result,
                    Err(e) => {
                        state.done = true;
                        return Some((Err(PipelineError::Source(Box::new(e))), state));
                    }
                };

                state.seen += result.assets.items.len();
                let owner_id = state.owner_id.clone();
                let mut matched = result
                    .assets
                    .items
                    .into_iter()
                    // Hidden assets are live-photo motion parts, live-photo stills
                    // would lose their pairing, and edits live outside the original
                    // file; leave all of them untouched.
                    .filter(|dto| {
                        dto.owner_id.as_deref() == Some(owner_id.as_str())
                            && dto.visibility.as_deref() != Some("hidden")
                            && dto.live_photo_video_id.is_none()
                            && dto.is_edited != Some(true)
                    })
                    .map(|dto| self.map_asset(dto))
                    .filter(|a| {
                        a.codec != self.target_codec && self.target_codec != MediaCodec::Unknown
                    })
                    .filter(|a| {
                        self.asset_ids
                            .as_ref()
                            .is_none_or(|asset_ids| asset_ids.contains(&a.id))
                    })
                    .collect::<Vec<_>>();

                if let Some(limit) = self.limit {
                    let remaining = limit.saturating_sub(state.yielded);
                    matched.truncate(remaining);
                    state.yielded += matched.len();
                    if state.yielded >= limit {
                        state.done = true;
                    }
                }

                state.pending = matched.into_iter();

                if !state.done {
                    match result.assets.next_page {
                        Some(ref token) if !token.is_empty() => {
                            state.page = token.parse().unwrap_or(state.page + 1);
                        }
                        _ => state.done = true,
                    }
                }
            }
        })
        .boxed())
    }

    async fn open_original(&self, asset: &Asset) -> Result<Box<dyn Read + Send + Unpin + 'static>> {
        let resp = self
            .client
            .download_original(&asset.id)
            .await
            .map_err(|e| PipelineError::Source(Box::new(e)))?;

        // Spool to an unlinked temp file: videos can be larger than RAM allows.
        let mut file = tempfile::tempfile().map_err(PipelineError::Io)?;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| PipelineError::Source(Box::new(e)))?;
            file.write_all(&chunk).map_err(PipelineError::Io)?;
        }
        file.seek(std::io::SeekFrom::Start(0))
            .map_err(PipelineError::Io)?;
        Ok(Box::new(file))
    }
}

struct DiscoveryState {
    page: i32,
    page_size: i32,
    pending: std::vec::IntoIter<Asset>,
    done: bool,
    seen: usize,
    yielded: usize,
    owner_id: String,
}
