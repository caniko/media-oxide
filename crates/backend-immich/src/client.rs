use std::path::Path;

use reqwest::multipart;
use serde::{Deserialize, Serialize};
use tokio_util::io::ReaderStream;

use super::ImmichConfig;

/// Immich-specific error.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("HTTP error: {status} {body}")]
    Http {
        status: reqwest::StatusCode,
        body: String,
    },
    #[error("request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("API returned error status: {0}")]
    Status(String),
}

type ApiResult<T> = std::result::Result<T, ApiError>;

// ── DTOs ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetResponseDto {
    pub id: String,
    pub checksum: Option<String>,
    #[serde(rename = "type")]
    pub asset_type: AssetType,
    pub original_file_name: String,
    pub original_mime_type: Option<String>,
    pub file_created_at: Option<String>,
    pub file_modified_at: Option<String>,
    pub is_favorite: Option<bool>,
    pub is_trashed: Option<bool>,
    /// Immich keeps edits (crops, rotations) apart from the original file.
    pub is_edited: Option<bool>,
    pub owner_id: Option<String>,
    /// `timeline`, `archive`, `hidden` (live-photo motion parts), or `locked`.
    pub visibility: Option<String>,
    pub live_photo_video_id: Option<String>,
    pub exif_info: Option<ExifResponseDto>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExifResponseDto {
    pub file_size_in_byte: Option<i64>,
    pub description: Option<String>,
    pub rating: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum AssetType {
    Image,
    Video,
    Audio,
    Other,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetadataSearchDto {
    pub page: i32,
    pub size: i32,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub asset_type: Option<AssetType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub taken_after: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub taken_before: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_after: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub with_deleted: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub with_exif: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub with_stacked: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SearchResponseDto {
    pub assets: SearchAssetResponseDto,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchAssetResponseDto {
    pub items: Vec<AssetResponseDto>,
    pub next_page: Option<String>,
    pub total: Option<i32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AssetMediaResponseDto {
    pub id: String,
    pub status: AssetMediaStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AssetMediaStatus {
    Created,
    Duplicate,
}

/// PUT /assets/{id} body; only set fields are sent.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateAssetDto {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rating: Option<i32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UserResponseDto {
    pub id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AssetBulkDeleteDto {
    pub ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub force: Option<bool>,
}

// ── Client ────────────────────────────────────────────────────────

/// Typed Immich REST API client.
///
/// Covers the subset of the Immich API needed for the transpiler:
/// search, download, upload, metadata copy, album management, and delete.
pub struct ImmichApiClient {
    base_url: String,
    client: reqwest::Client,
}

impl ImmichApiClient {
    pub fn new(config: &ImmichConfig) -> Self {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::HeaderName::from_static("x-api-key"),
            reqwest::header::HeaderValue::from_str(&config.api_key).unwrap(),
        );

        let client = reqwest::Client::builder()
            .user_agent("rs-immicher-oxide/0.1")
            .default_headers(headers)
            .build()
            .expect("failed to build reqwest client");

        Self {
            base_url: config.server_url.trim_end_matches('/').to_string(),
            client,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    async fn check_response(&self, resp: reqwest::Response) -> ApiResult<reqwest::Response> {
        let status = resp.status();
        if status.is_success() {
            Ok(resp)
        } else {
            let body = resp.text().await.unwrap_or_default();
            Err(ApiError::Http { status, body })
        }
    }

    // ── Asset discovery ───────────────────────────────────────────

    /// POST /search/metadata with paginated results.
    pub async fn search_assets(&self, dto: &MetadataSearchDto) -> ApiResult<SearchResponseDto> {
        let resp = self
            .client
            .post(self.url("/search/metadata"))
            .json(dto)
            .send()
            .await?;
        let resp = self.check_response(resp).await?;
        Ok(resp.json().await?)
    }

    /// Discover all assets of a given type, page by page.
    pub async fn search_all_assets(
        &self,
        asset_type: Option<AssetType>,
        taken_after: Option<String>,
    ) -> ApiResult<Vec<AssetResponseDto>> {
        let mut all = Vec::new();
        let mut page = 1;
        let size = 500;

        loop {
            let dto = MetadataSearchDto {
                page,
                size,
                asset_type: asset_type.clone(),
                taken_after: taken_after.clone(),
                taken_before: None,
                created_after: None,
                with_deleted: Some(false),
                with_exif: None,
                with_stacked: None,
            };

            let result = self.search_assets(&dto).await?;
            all.extend(result.assets.items);

            match result.assets.next_page {
                Some(ref token) if !token.is_empty() => {
                    page = token.parse().unwrap_or(page + 1);
                }
                _ => break,
            }
        }

        Ok(all)
    }

    // ── Download ──────────────────────────────────────────────────

    /// GET /assets/{id}/original — returns a streaming response.
    pub async fn download_original(&self, asset_id: &str) -> ApiResult<reqwest::Response> {
        let resp = self
            .client
            .get(self.url(&format!("/assets/{asset_id}/original")))
            .send()
            .await?;
        self.check_response(resp).await
    }

    // ── Asset info ────────────────────────────────────────────────

    /// GET /assets/{id} — get full asset metadata.
    pub async fn get_asset_info(&self, asset_id: &str) -> ApiResult<AssetResponseDto> {
        let resp = self
            .client
            .get(self.url(&format!("/assets/{asset_id}")))
            .send()
            .await?;
        let resp = self.check_response(resp).await?;
        Ok(resp.json().await?)
    }

    // ── Upload ────────────────────────────────────────────────────

    /// POST /assets — upload a transcoded asset.
    pub async fn upload_asset(
        &self,
        asset_data: Vec<u8>,
        file_created_at: &str,
        file_modified_at: &str,
        filename: &str,
        visibility: &str,
    ) -> ApiResult<AssetMediaResponseDto> {
        let part = multipart::Part::bytes(asset_data)
            .file_name(filename.to_string())
            .mime_str(asset_mime(filename))
            .map_err(|e| ApiError::Status(e.to_string()))?;

        self.upload_asset_part(part, file_created_at, file_modified_at, visibility)
            .await
    }

    /// POST /assets — upload a transcoded asset from a file without buffering it.
    pub async fn upload_asset_file(
        &self,
        asset_path: &Path,
        byte_count: u64,
        file_created_at: &str,
        file_modified_at: &str,
        filename: &str,
        visibility: &str,
    ) -> ApiResult<AssetMediaResponseDto> {
        let file = tokio::fs::File::open(asset_path)
            .await
            .map_err(|e| ApiError::Status(format!("open upload file: {e}")))?;
        let stream = ReaderStream::new(file);
        let body = reqwest::Body::wrap_stream(stream);
        let part = multipart::Part::stream_with_length(body, byte_count)
            .file_name(filename.to_string())
            .mime_str(asset_mime(filename))
            .map_err(|e| ApiError::Status(e.to_string()))?;

        self.upload_asset_part(part, file_created_at, file_modified_at, visibility)
            .await
    }

    async fn upload_asset_part(
        &self,
        part: multipart::Part,
        file_created_at: &str,
        file_modified_at: &str,
        visibility: &str,
    ) -> ApiResult<AssetMediaResponseDto> {
        let form = multipart::Form::new()
            .part("assetData", part)
            .text("fileCreatedAt", file_created_at.to_string())
            .text("fileModifiedAt", file_modified_at.to_string())
            .text("visibility", visibility.to_string());

        let resp = self
            .client
            .post(self.url("/assets"))
            .multipart(form)
            .send()
            .await?;

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();

        if status.is_success() {
            Ok(serde_json::from_str(&body)
                .map_err(|e| ApiError::Status(format!("parse upload response: {e}: {body}")))?)
        } else {
            Err(ApiError::Http { status, body })
        }
    }

    // ── Metadata ──────────────────────────────────────────────────

    /// GET /users/me — the user that owns this API key.
    pub async fn get_my_user(&self) -> ApiResult<UserResponseDto> {
        let resp = self.client.get(self.url("/users/me")).send().await?;
        let resp = self.check_response(resp).await?;
        Ok(resp.json().await?)
    }

    /// PUT /assets/copy — copy albums, shared links, stack, favorite, and the
    /// XMP sidecar (user edits such as tags, dates, and location) from
    /// `source_id` to `target_id`.
    pub async fn copy_asset(&self, source_id: &str, target_id: &str) -> ApiResult<()> {
        let resp = self
            .client
            .put(self.url("/assets/copy"))
            .json(&serde_json::json!({ "sourceId": source_id, "targetId": target_id }))
            .send()
            .await?;
        self.check_response(resp).await?;
        Ok(())
    }

    /// PUT /assets/{id} — update asset metadata.
    pub async fn update_asset(&self, asset_id: &str, dto: &UpdateAssetDto) -> ApiResult<()> {
        let resp = self
            .client
            .put(self.url(&format!("/assets/{asset_id}")))
            .json(dto)
            .send()
            .await?;
        self.check_response(resp).await?;
        Ok(())
    }

    // ── Delete ────────────────────────────────────────────────────

    /// DELETE /assets — trash assets.
    pub async fn delete_assets(&self, ids: &[String], force: bool) -> ApiResult<()> {
        let resp = self
            .client
            .delete(self.url("/assets"))
            .json(&AssetBulkDeleteDto {
                ids: ids.to_vec(),
                force: Some(force),
            })
            .send()
            .await?;
        self.check_response(resp).await?;
        Ok(())
    }
}

fn asset_mime(_filename: &str) -> &'static str {
    "application/octet-stream"
}
