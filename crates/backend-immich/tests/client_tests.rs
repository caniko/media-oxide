use rs_immicher_oxide_backend_immich::client::{AssetType, ImmichApiClient, MetadataSearchDto};
use rs_immicher_oxide_backend_immich::ImmichConfig;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn make_client(mock: &MockServer) -> ImmichApiClient {
    let config = ImmichConfig::new(mock.uri(), "test-api-key");
    ImmichApiClient::new(&config)
}

fn make_asset_item(id: &str, fname: &str, mime: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "type": "IMAGE",
        "originalFileName": fname,
        "originalMimeType": mime,
        "fileCreatedAt": "2024-01-01T00:00:00.000Z",
        "fileModifiedAt": "2024-01-01T00:00:00.000Z",
        "isFavorite": false,
        "isArchived": false,
        "checksum": null,
        "exifInfo": null
    })
}

fn make_search_response(
    items: Vec<serde_json::Value>,
    next_page: Option<&str>,
    total: i32,
) -> serde_json::Value {
    serde_json::json!({
        "assets": {
            "items": items,
            "nextPage": next_page,
            "total": total,
            "facets": []
        },
        "albums": { "items": [], "nextPage": null, "total": 0 }
    })
}

#[tokio::test]
async fn search_assets_returns_items() {
    let mock = MockServer::start().await;
    let body = make_search_response(
        vec![make_asset_item("asset-1", "photo.jpg", "image/jpeg")],
        None,
        1,
    );

    Mock::given(method("POST"))
        .and(path("/search/metadata"))
        .and(header("x-api-key", "test-api-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&mock)
        .await;

    let client = make_client(&mock);
    let result = client
        .search_assets(&MetadataSearchDto {
            page: 1,
            size: 100,
            asset_type: Some(AssetType::Image),
            taken_after: None,
            taken_before: None,
            created_after: None,
            with_deleted: Some(false),
            with_exif: None,
            with_stacked: None,
        })
        .await;

    assert!(result.is_ok(), "search should succeed: {:?}", result.err());
    let resp = result.unwrap();
    assert_eq!(resp.assets.items.len(), 1);
    assert_eq!(resp.assets.items[0].id, "asset-1");
    assert_eq!(resp.assets.items[0].original_file_name, "photo.jpg");
}

#[tokio::test]
async fn search_all_assets_returns_all_items() {
    let mock = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/search/metadata"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(make_search_response(
                vec![make_asset_item("a1", "a1.jpg", "image/jpeg")],
                None,
                1,
            )),
        )
        .mount(&mock)
        .await;

    let client = make_client(&mock);
    let assets = client
        .search_all_assets(Some(AssetType::Image), None)
        .await
        .expect("search_all_assets should succeed");

    assert_eq!(assets.len(), 1);
    assert_eq!(assets[0].id, "a1");
}

#[tokio::test]
async fn download_original_returns_bytes() {
    let mock = MockServer::start().await;
    let content = b"fake-image-bytes";

    Mock::given(method("GET"))
        .and(path("/assets/abc-123/original"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(content))
        .mount(&mock)
        .await;

    let client = make_client(&mock);
    let resp = client
        .download_original("abc-123")
        .await
        .expect("download should succeed");

    let body = resp.bytes().await.unwrap();
    assert_eq!(&body[..], content);
}

#[tokio::test]
async fn upload_asset_returns_id() {
    let mock = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/assets"))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(serde_json::json!({"id": "new-asset-id", "status": "created"})),
        )
        .mount(&mock)
        .await;

    let client = make_client(&mock);
    let result = client
        .upload_asset(
            b"fake-content".to_vec(),
            "2024-01-01T00:00:00.000Z",
            "2024-01-01T00:00:00.000Z",
            "output.mp4",
            "timeline",
        )
        .await;

    assert!(result.is_ok(), "upload failed: {:?}", result.err());
    let resp = result.unwrap();
    assert_eq!(resp.id, "new-asset-id");
}

#[tokio::test]
async fn copy_asset_sends_camel_case_ids() {
    let mock = MockServer::start().await;

    Mock::given(method("PUT"))
        .and(path("/assets/copy"))
        .and(body_partial_json(
            serde_json::json!({"sourceId": "old", "targetId": "new"}),
        ))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&mock)
        .await;

    make_client(&mock)
        .copy_asset("old", "new")
        .await
        .expect("copy should succeed");
}

#[tokio::test]
async fn delete_assets_succeeds() {
    let mock = MockServer::start().await;

    Mock::given(method("DELETE"))
        .and(path("/assets"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&mock)
        .await;

    let client = make_client(&mock);
    let result = client.delete_assets(&["asset-1".into()], false).await;

    assert!(result.is_ok(), "delete failed: {:?}", result.err());
}

#[tokio::test]
async fn unauthorized_returns_error() {
    let mock = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/search/metadata"))
        .respond_with(ResponseTemplate::new(401).set_body_string("Unauthorized"))
        .mount(&mock)
        .await;

    let client = make_client(&mock);
    let result = client
        .search_assets(&MetadataSearchDto {
            page: 1,
            size: 10,
            asset_type: None,
            taken_after: None,
            taken_before: None,
            created_after: None,
            with_deleted: None,
            with_exif: None,
            with_stacked: None,
        })
        .await;

    assert!(result.is_err(), "should return error for 401");
    let err = result.unwrap_err();
    let err_str = err.to_string();
    assert!(
        err_str.contains("401"),
        "error should include status 401: {err_str}"
    );
}
