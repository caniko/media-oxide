use futures::StreamExt;
use rs_immicher_oxide_backend_immich::source::ImmichSource;
use rs_immicher_oxide_backend_immich::ImmichConfig;
use rs_immicher_oxide_core::source::Source;
use rs_immicher_oxide_core::types::MediaCodec;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn make_asset_item(id: &str, fname: &str, mime: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "type": "IMAGE",
        "originalFileName": fname,
        "originalMimeType": mime,
        "fileCreatedAt": "2024-01-01T00:00:00.000Z",
        "fileModifiedAt": "2024-01-01T00:00:00.000Z",
        "isFavorite": false,
        "ownerId": "me",
        "visibility": "timeline",
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
async fn discover_pages_past_first_and_skips_unreplaceable_assets() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/users/me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "me"})))
        .mount(&mock)
        .await;

    let mut partner = make_asset_item("partner", "p.jpg", "image/jpeg");
    partner["ownerId"] = "someone-else".into();
    let mut motion = make_asset_item("motion", "m.jpg", "image/jpeg");
    motion["visibility"] = "hidden".into();
    let mut live = make_asset_item("live", "l.jpg", "image/jpeg");
    live["livePhotoVideoId"] = "motion".into();
    let mut edited = make_asset_item("edited", "e.jpg", "image/jpeg");
    edited["isEdited"] = true.into();
    let page1 = vec![
        make_asset_item("a1", "a1.jpg", "image/jpeg"),
        partner,
        motion,
        live,
        edited,
    ];

    // Immich reports the page length as `total`; paging must follow `nextPage`.
    Mock::given(method("POST"))
        .and(path("/search/metadata"))
        .and(body_partial_json(
            serde_json::json!({ "page": 1, "withExif": true, "withStacked": false }),
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(make_search_response(page1, Some("2"), 4)),
        )
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/search/metadata"))
        .and(body_partial_json(serde_json::json!({ "page": 2 })))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(make_search_response(
                vec![make_asset_item("a2", "a2.png", "image/png")],
                None,
                1,
            )),
        )
        .mount(&mock)
        .await;

    let source = ImmichSource::new(
        ImmichConfig::new(mock.uri(), "test-api-key"),
        MediaCodec::Jxl,
    )
    .with_images_only();

    let ids: Vec<String> = source
        .discover()
        .await
        .expect("discover should start")
        .map(|a| a.expect("asset maps").id)
        .collect()
        .await;

    assert_eq!(ids, ["a1", "a2"]);
}
