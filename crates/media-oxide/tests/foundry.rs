#![cfg(feature = "foundry")]

//! Foundry conversion policy tests.
//!
//! The end-to-end conversion test needs `ffmpeg` and `ffprobe` on `PATH` and
//! skips without them.

use std::path::Path;

use media_oxide::foundry::{
    allowed_upload, convert, needs_probe, normalize_stem, plan, sniff_matches, svg_is_safe,
    svg_with_size, Action, AssetKind, AudioCodec, Error, Options, Plan, Probe, MAX_SVG_BYTES,
};

fn options() -> Options {
    Options::default()
}

fn ffmpeg_arguments(planned: &Plan) -> Vec<String> {
    match &planned.action {
        Action::Ffmpeg(arguments) => arguments.clone(),
        other => panic!("expected ffmpeg arguments, got {other:?}"),
    }
}

#[test]
fn stems_are_lowercase_words() {
    assert_eq!(normalize_stem("Guard Red!"), "guard-red");
    assert_eq!(normalize_stem("guard-red"), "guard-red");
    assert_eq!(normalize_stem("  --Guard__Red--  "), "guard-red");
    assert_eq!(normalize_stem("日本語"), "asset");
    assert_eq!(normalize_stem(""), "asset");
}

#[test]
fn token_plans_a_square_transparent_pad() {
    let planned = plan(
        Path::new("Guard Red.png"),
        Some(AssetKind::Token),
        &options(),
        None,
    )
    .expect("token plan");
    assert_eq!(planned.kind, AssetKind::Token);
    assert_eq!(planned.file_name, "guard-red.webp");
    let arguments = ffmpeg_arguments(&planned);
    let filter = &arguments[arguments.iter().position(|arg| arg == "-vf").expect("-vf") + 1];
    assert!(filter.contains("format=rgba"), "{filter}");
    assert!(filter.contains("scale=400:400"), "{filter}");
    assert!(
        filter.contains("force_original_aspect_ratio=decrease"),
        "{filter}"
    );
    assert!(filter.contains("pad=400:400"), "{filter}");
    assert!(arguments.iter().any(|arg| arg == "-quality"));
}

#[test]
fn token_size_outside_the_range_is_rejected() {
    let small = Options {
        token_px: 128,
        ..Options::default()
    };
    let error = plan(Path::new("guard.png"), Some(AssetKind::Token), &small, None)
        .expect_err("token below 200 px");
    assert!(error.to_string().contains("token_px"), "{error}");
}

#[test]
fn map_snaps_to_whole_grid_cells() {
    let probe = Probe {
        width: 2048,
        height: 1536,
    };
    let planned = plan(
        Path::new("scene.png"),
        Some(AssetKind::Map),
        &options(),
        Some(probe),
    )
    .expect("map plan");
    assert_eq!(planned.file_name, "scene.webp");
    assert!(ffmpeg_arguments(&planned)
        .iter()
        .any(|arg| arg == "scale=2000:1500:flags=lanczos"));
}

#[test]
fn oversized_map_asks_for_a_smaller_grid() {
    let probe = Probe {
        width: 20000,
        height: 12000,
    };
    let error = plan(
        Path::new("huge.png"),
        Some(AssetKind::Map),
        &options(),
        Some(probe),
    )
    .expect_err("map above the WebP limit");
    let message = error.to_string();
    assert!(message.contains("16383"), "{message}");
    assert!(message.contains("grid_px"), "{message}");
}

#[test]
fn map_needs_the_source_dimensions() {
    assert!(needs_probe(Path::new("scene.png"), Some(AssetKind::Map)));
    assert!(!needs_probe(Path::new("scene.png"), Some(AssetKind::Token)));
    assert!(!needs_probe(Path::new("scene.png"), None));
    assert!(!needs_probe(Path::new("campfire.gif"), None));
    assert!(plan(
        Path::new("scene.png"),
        Some(AssetKind::Map),
        &options(),
        None
    )
    .is_err());
}

#[test]
fn existing_webp_artwork_is_copied_untouched() {
    let planned = plan(Path::new("Cover.WEBP"), None, &options(), None).expect("webp plan");
    assert_eq!(planned.kind, AssetKind::Artwork);
    assert_eq!(planned.file_name, "cover.webp");
    assert_eq!(planned.action, Action::Copy);
}

#[test]
fn lossy_audio_is_copied_under_its_own_extension() {
    let planned = plan(Path::new("Tavern Loop.mp3"), None, &options(), None).expect("mp3 plan");
    assert_eq!(planned.kind, AssetKind::Audio);
    assert_eq!(planned.file_name, "tavern-loop.mp3");
    assert_eq!(planned.action, Action::Copy);
}

#[test]
fn lossless_audio_becomes_ogg_with_the_chosen_codec() {
    let planned = plan(Path::new("rain.wav"), None, &options(), None).expect("wav plan");
    assert_eq!(planned.kind, AssetKind::Audio);
    assert_eq!(planned.file_name, "rain.ogg");
    assert_eq!(
        ffmpeg_arguments(&planned),
        [
            "-c:a",
            "libvorbis",
            "-b:a",
            "128k",
            "-vn",
            "-map_metadata",
            "-1"
        ]
    );

    let opus = Options {
        audio: AudioCodec::Opus,
        ..Options::default()
    };
    let planned = plan(Path::new("rain.wav"), None, &opus, None).expect("opus plan");
    assert!(ffmpeg_arguments(&planned)
        .iter()
        .any(|arg| arg == "libopus"));
}

#[test]
fn audio_bitrate_is_clamped_to_the_declared_range() {
    let loud = Options {
        audio_kbps: 999,
        ..Options::default()
    };
    let quiet = Options {
        audio_kbps: 1,
        ..Options::default()
    };
    for (options, expected) in [(&loud, "192k"), (&quiet, "64k")] {
        let planned = plan(Path::new("rain.flac"), None, options, None).expect("flac plan");
        assert!(
            ffmpeg_arguments(&planned).iter().any(|arg| arg == expected),
            "{expected}"
        );
    }
}

#[test]
fn animations_become_webm() {
    let planned = plan(Path::new("campfire.gif"), None, &options(), None).expect("gif plan");
    assert_eq!(planned.kind, AssetKind::Map);
    assert_eq!(planned.file_name, "campfire.webm");
    let arguments = ffmpeg_arguments(&planned);
    assert!(arguments.iter().any(|arg| arg == "libvpx-vp9"));
    assert!(arguments.iter().any(|arg| arg == "-crf"));
    assert!(arguments.iter().any(|arg| arg == "-an"));
}

#[test]
fn audio_only_accepts_audio() {
    let image = plan(
        Path::new("harbor.png"),
        Some(AssetKind::Audio),
        &options(),
        None,
    )
    .expect_err("png is not audio");
    assert!(image.to_string().contains("audio"), "{image}");

    let audio = plan(
        Path::new("rain.wav"),
        Some(AssetKind::Token),
        &options(),
        None,
    )
    .expect_err("audio is not a token");
    assert!(audio.to_string().contains("audio"), "{audio}");
}

#[test]
fn unknown_extensions_are_named() {
    let error = plan(Path::new("setup.exe"), None, &options(), None).expect_err("exe");
    assert!(error.to_string().contains("exe"), "{error}");
}

#[test]
fn svg_gains_dimensions_from_its_view_box() {
    let sized = svg_with_size(r#"<svg viewBox="0 0 120 80" fill="red"><rect/></svg>"#)
        .expect("sized from viewBox");
    assert_eq!(
        sized,
        r#"<svg width="120" height="80" viewBox="0 0 120 80" fill="red"><rect/></svg>"#
    );
    let untouched = r#"<svg width="10" height="20" viewBox="0 0 1 1"/>"#;
    assert_eq!(svg_with_size(untouched).expect("unchanged"), untouched);
    assert!(svg_with_size("<svg><rect/></svg>").is_err());
}

#[test]
fn svg_size_finds_the_root_past_comments_and_prolog() {
    // The first `<svg` is inside a comment; sizing it would leave the real
    // root unsized.
    let commented = r#"<!-- <svg viewBox="0 0 3 4"> --><svg viewBox="0 0 120 80"/>"#;
    assert_eq!(
        svg_with_size(commented).expect("sized past the comment"),
        r#"<!-- <svg viewBox="0 0 3 4"> --><svg width="120" height="80" viewBox="0 0 120 80"/>"#
    );
    let prolog = concat!(
        "<?xml version=\"1.0\"?>\n",
        "<!DOCTYPE svg PUBLIC \"-//W3C//DTD SVG 1.1//EN\" ",
        "\"http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd\">\n",
        "<!-- <svg viewBox=\"0 0 3 4\"/> -->\n",
        "<svg viewBox=\"0 0 120 80\"/>",
    );
    assert_eq!(
        svg_with_size(prolog).expect("sized past the prolog"),
        concat!(
            "<?xml version=\"1.0\"?>\n",
            "<!DOCTYPE svg PUBLIC \"-//W3C//DTD SVG 1.1//EN\" ",
            "\"http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd\">\n",
            "<!-- <svg viewBox=\"0 0 3 4\"/> -->\n",
            "<svg width=\"120\" height=\"80\" viewBox=\"0 0 120 80\"/>",
        )
    );
    // A prefixed root is sized after its own name, not after `<svg`.
    assert_eq!(
        svg_with_size(r#"<s:svg xmlns:s="http://www.w3.org/2000/svg" viewBox="0 0 5 6"/>"#)
            .expect("sized prefixed root"),
        r#"<s:svg width="5" height="6" xmlns:s="http://www.w3.org/2000/svg" viewBox="0 0 5 6"/>"#
    );
}

#[test]
fn convert_sizes_an_svg_and_refuses_a_scripted_one() {
    let directory = tempfile::tempdir().expect("temp dir");
    let benign = directory.path().join("Icon Mask.svg");
    std::fs::write(
        &benign,
        concat!(
            "<!-- <svg viewBox=\"0 0 3 4\"/> -->\n",
            "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 120 80\"><rect/></svg>",
        ),
    )
    .expect("write svg");
    let output = convert(&benign, None, &options(), directory.path()).expect("svg conversion");
    assert_eq!(output, directory.path().join("artwork/icon-mask.svg"));
    assert_eq!(
        std::fs::read_to_string(&output).expect("read svg"),
        concat!(
            "<!-- <svg viewBox=\"0 0 3 4\"/> -->\n",
            "<svg width=\"120\" height=\"80\" xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 120 80\"><rect/></svg>",
        )
    );

    let scripted = directory.path().join("Scripted.svg");
    std::fs::write(
        &scripted,
        concat!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\"><iframe ",
            "xmlns=\"http://www.w3.org/1999/xhtml\" ",
            "srcdoc=\"&lt;script&gt;alert(1)&lt;/script&gt;\"/></svg>",
        ),
    )
    .expect("write scripted svg");
    let refused = convert(&scripted, None, &options(), directory.path());
    assert!(matches!(refused, Err(Error::Invalid(_))), "{refused:?}");
    assert!(!directory.path().join("artwork/scripted.svg").exists());
}

#[test]
fn unsafe_svg_is_rejected() {
    assert!(!svg_is_safe("<svg><script>alert(1)</script></svg>"));
    assert!(!svg_is_safe("<svg onload=\"alert(1)\"/>"));
    assert!(!svg_is_safe("<svg><a href=\"javascript:alert(1)\"/></svg>"));
    assert!(!svg_is_safe("<svg><foreignObject/></svg>"));
    // Bypasses a naive denylist misses.
    assert!(!svg_is_safe("<svg onload = \"alert(1)\"/>"));
    assert!(!svg_is_safe("<svg onload\n=\"alert(1)\"/>"));
    assert!(!svg_is_safe(
        "<s:svg xmlns:s=\"http://www.w3.org/2000/svg\"><s:script>alert(1)</s:script></s:svg>"
    ));
    assert!(!svg_is_safe(
        "<svg><a href=\"java&#x73;cript:alert(1)\"/></svg>"
    ));
    assert!(!svg_is_safe(
        "<svg><a href=\"java\tscript:alert(1)\"/></svg>"
    ));
    assert!(!svg_is_safe(
        "<!DOCTYPE svg [<!ENTITY js \"javascript:\">]><svg><a href=\"&js;alert(1)\"/></svg>"
    ));
    assert!(svg_is_safe(
        "<svg width=\"1\" height=\"1\"><rect font-size=\"2\"/></svg>"
    ));
}

#[test]
fn namespaced_frames_and_foreign_elements_are_rejected() {
    // Chromium executes the embedded script when it opens this SVG, and
    // nothing in the markup spells `script` outside the `srcdoc` value.
    assert!(!svg_is_safe(
        r#"<svg xmlns="http://www.w3.org/2000/svg"><iframe xmlns="http://www.w3.org/1999/xhtml" srcdoc="&lt;script&gt;alert(1)&lt;/script&gt;"/></svg>"#
    ));
    assert!(!svg_is_safe(
        r#"<svg xmlns="http://www.w3.org/2000/svg"><foreignObject><iframe xmlns="http://www.w3.org/1999/xhtml" srcdoc="&lt;script&gt;alert(1)&lt;/script&gt;"/></foreignObject></svg>"#
    ));
    // A foreign namespace is refused even for an allowlisted local name.
    assert!(!svg_is_safe(
        r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:h="http://www.w3.org/1999/xhtml"><h:rect/></svg>"#
    ));
    assert!(!svg_is_safe(
        r#"<svg xmlns="http://www.w3.org/2000/svg"><embed xmlns="http://www.w3.org/1999/xhtml"/></svg>"#
    ));
    // Elements outside the allowlist cannot arrive through another prefix.
    assert!(!svg_is_safe(
        r#"<s:svg xmlns:s="http://www.w3.org/2000/svg"><s:script>alert(1)</s:script></s:svg>"#
    ));
}

#[test]
fn script_reach_in_attributes_is_rejected() {
    assert!(!svg_is_safe(
        r#"<svg xmlns="http://www.w3.org/2000/svg"><a xlink:href="javascript:alert(1)" xmlns:xlink="http://www.w3.org/1999/xlink"/></svg>"#
    ));
    assert!(!svg_is_safe(
        r#"<svg xmlns="http://www.w3.org/2000/svg"><a href="data:text/html,&lt;script&gt;alert(1)&lt;/script&gt;"/></svg>"#
    ));
    // CSS and presentation attributes can reach the network.
    assert!(!svg_is_safe(
        r#"<svg xmlns="http://www.w3.org/2000/svg"><rect style="fill:url(https://example.invalid/a.svg#x)"/></svg>"#
    ));
    assert!(!svg_is_safe(
        r#"<svg xmlns="http://www.w3.org/2000/svg"><rect fill="url(https://example.invalid/a.svg#x)"/></svg>"#
    ));
    assert!(!svg_is_safe(
        r#"<?xml-stylesheet href="https://example.invalid/a.css"?><svg xmlns="http://www.w3.org/2000/svg"/>"#
    ));
}

#[test]
fn css_escapes_in_attribute_values_are_rejected() {
    // `u\72l(` is `url(` to a CSS parser but not to a literal search.
    assert!(!svg_is_safe(
        r#"<svg xmlns="http://www.w3.org/2000/svg"><rect fill="u\72l(https://example.invalid/a.svg#x)"/></svg>"#
    ));
    assert!(!svg_is_safe(
        r#"<svg xmlns="http://www.w3.org/2000/svg"><rect fill="\75rl(https://example.invalid/a.svg#x)"/></svg>"#
    ));
    assert!(!svg_is_safe(
        r#"<svg xmlns="http://www.w3.org/2000/svg"><rect fill="url(#g\)"/></svg>"#
    ));
}

#[test]
fn internal_dtd_subsets_are_refused_before_parsing() {
    // A defaulted event handler lives in the subset, where roxmltree cannot see
    // it and Chromium applies it.
    assert!(!svg_is_safe(
        r#"<!DOCTYPE svg [<!ATTLIST svg onload CDATA "alert(1)">]><svg/>"#
    ));
    assert!(!svg_is_safe(
        r#"<!DOCTYPE svg [<!ENTITY js "javascript:">]><svg><a href="&js;"/></svg>"#
    ));
    assert!(!svg_is_safe(
        "<!-- <!DOCTYPE svg [<!ENTITY a \"x\">]> --><!DOCTYPE svg [<!ENTITY b \"y\">]><svg/>"
    ));
    // A doctype without a subset is still a plain document.
    assert!(svg_is_safe(
        r#"<!DOCTYPE svg PUBLIC "-//W3C//DTD SVG 1.1//EN" "http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd"><svg width="1" height="1"/>"#
    ));
}

#[test]
fn entity_bombs_and_oversized_documents_are_rejected_quickly() {
    // Nothing is expanded: the subset is refused from the raw text.
    let mut bomb = String::from(r#"<!DOCTYPE svg [<!ENTITY a "lollollollollollol">"#);
    for level in 1..24 {
        bomb.push_str(&format!(
            r#"<!ENTITY l{level} "{}">"#,
            format!("&l{};", level - 1).repeat(10)
        ));
    }
    bomb.push_str("]><svg>&l23;</svg>");
    let started = std::time::Instant::now();
    assert!(!svg_is_safe(&bomb));
    assert!(
        started.elapsed() < std::time::Duration::from_millis(250),
        "the bomb took {:?}",
        started.elapsed()
    );

    let mut oversized = String::from(r#"<svg xmlns="http://www.w3.org/2000/svg"/>"#);
    oversized.push_str(&"a".repeat(MAX_SVG_BYTES));
    assert!(!svg_is_safe(&oversized));
    assert!(matches!(svg_with_size(&oversized), Err(Error::Invalid(_))));
}

#[test]
fn quoted_xml_declaration_cannot_hide_an_entity_subset() {
    // `?>` inside the quoted version value used to end the prolog scan early,
    // so the subset below was never inspected while roxmltree expanded it:
    // ~22 KB of text becoming 16 MiB, through both entry points.
    let mut bomb = String::from(r#"<?xml version="1.0?>"?>"#);
    bomb.push_str(r#"<!DOCTYPE svg [<!ENTITY a ""#);
    bomb.push_str(&"x".repeat(2_580));
    bomb.push_str(r#"">]>"#);
    bomb.push_str(r#"<svg xmlns="http://www.w3.org/2000/svg"><text>"#);
    bomb.push_str(&"&a;".repeat(6_500));
    bomb.push_str("</text></svg>");
    assert!(bomb.len() < 64 * 1024, "reproducer is {} bytes", bomb.len());

    let started = std::time::Instant::now();
    assert!(!svg_is_safe(&bomb), "the declaration bypass was accepted");
    assert!(
        matches!(svg_with_size(&bomb), Err(Error::Invalid(_))),
        "the declaration bypass was accepted for sizing"
    );
    // Rejection is a scan of the raw text; expanding the subset takes hundreds
    // of milliseconds and 16 MiB even in a debug build.
    assert!(
        started.elapsed() < std::time::Duration::from_millis(100),
        "the declaration bypass took {:?}",
        started.elapsed()
    );
}

#[test]
fn plain_drawing_svg_still_passes() {
    assert!(svg_is_safe(
        r##"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE svg PUBLIC "-//W3C//DTD SVG 1.1//EN" "http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd">
<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" viewBox="0 0 120 80" width="120" height="80">
  <!-- icon -->
  <defs><linearGradient id="g"><stop offset="0" stop-color="#fff"/></linearGradient><clipPath id="c"><rect width="10" height="10"/></clipPath></defs>
  <g fill="url(#g)" clip-path="url(#c)">
    <path d="M0 0h10v10H0z"/>
    <circle cx="5" cy="5" r="4"/>
    <use xlink:href="#c"/>
    <a href="#c"><text x="1" y="2" font-size="2">A</text></a>
  </g>
</svg>"##
    ));
}

#[test]
fn upload_names_are_normalized_and_kind_appropriate() {
    assert!(allowed_upload(AssetKind::Token, "guard-red.webp"));
    assert!(allowed_upload(AssetKind::Artwork, "guard.webm"));
    assert!(allowed_upload(AssetKind::Audio, "tavern-loop.ogg"));
    assert!(allowed_upload(AssetKind::Audio, "tavern.webm"));
    assert!(allowed_upload(AssetKind::Audio, "tavern.m4a"));
    assert!(allowed_upload(AssetKind::Audio, "tavern.aac"));
    assert!(!allowed_upload(AssetKind::Token, "../x.webp"));
    assert!(!allowed_upload(AssetKind::Token, "Upper.webp"));
    assert!(!allowed_upload(AssetKind::Token, "a.exe"));
    assert!(!allowed_upload(AssetKind::Artwork, "track.flac"));
    assert!(!allowed_upload(AssetKind::Audio, "token.png"));
    assert!(!allowed_upload(AssetKind::Token, "guard--red.webp"));
    assert!(!allowed_upload(AssetKind::Token, "guard-red"));
}

#[test]
fn magic_bytes_must_match_the_extension() {
    assert!(sniff_matches("a.png", b"\x89PNG\r\n\x1a\n\0\0"));
    assert!(!sniff_matches("a.webp", b"\x89PNG\r\n\x1a\n\0\0"));
    assert!(sniff_matches("a.webp", b"RIFF\0\0\0\0WEBPVP8 "));
    assert!(sniff_matches("a.jpg", b"\xff\xd8\xff\xe0"));
    assert!(sniff_matches("a.mp4", b"\0\0\0\x18ftypisom"));
    assert!(sniff_matches(
        "a.webm",
        b"\x1a\x45\xdf\xa3\x9f\x42\x86\x81\x01\x42\x82\x84webm"
    ));
    assert!(!sniff_matches(
        "a.webm",
        b"\x1a\x45\xdf\xa3\xa3\x42\x82\x88matroska"
    ));
    assert!(sniff_matches("a.avif", b"\0\0\0\x1cftypavif"));
    assert!(!sniff_matches("a.avif", b"\0\0\0\x18ftypisom"));
    assert!(!sniff_matches("a.mp4", b"\0\0\0\x1cftypavif"));
    // AVIF commonly declares major brand `mif1` with `avif` compatible.
    let mif1 = b"\0\0\0\x1cftypmif1\0\0\0\0mif1avifmiaf";
    assert!(sniff_matches("a.avif", mif1));
    assert!(!sniff_matches("a.mp4", mif1));
    assert!(sniff_matches("a.m4a", b"\0\0\0\x20ftypM4A "));
    assert!(sniff_matches("a.aac", &[0xff, 0xf1, 0x50]));
    assert!(sniff_matches(
        "a.svg",
        b"\xef\xbb\xbf<?xml version=\"1.0\"?>\n<!-- icon -->\n<svg width=\"1\">"
    ));
    assert!(sniff_matches(
        "a.svg",
        b"<?xml version=\"1.0\"?>\n<!DOCTYPE svg PUBLIC \"-//W3C//DTD SVG 1.1//EN\" \"http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd\">\n<svg width=\"1\">"
    ));
    assert!(!sniff_matches("a.svg", b"<html><svg/></html>"));
    // Multi-byte text right after the root must not panic the sniffer.
    assert!(sniff_matches("a.svg", "<svg>日本語</svg>".as_bytes()));
    assert!(!sniff_matches("a.svg", "<!doc日本語".as_bytes()));
    assert!(!sniff_matches("a.svg", b"hello <svg"));
    assert!(sniff_matches("a.ogg", b"OggS\0\x02"));
    assert!(sniff_matches("a.flac", b"fLaC\0\0"));
    assert!(sniff_matches("a.mp3", b"ID3\x04"));
    assert!(sniff_matches("a.mp3", &[0xff, 0xfb, 0x90]));
    assert!(sniff_matches(
        "a.svg",
        b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>"
    ));
    assert!(!sniff_matches("a.avif", b"RIFF\0\0\0\0WEBP"));
    assert!(!sniff_matches("a.svg", b"\xff\xfe<svg"));
    assert!(!sniff_matches("a.exe", b"MZ"));
}

fn tool_available(program: &str) -> bool {
    std::process::Command::new(program)
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn dimensions(path: &Path) -> (u32, u32) {
    let output = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .expect("run ffprobe");
    assert!(
        output.status.success(),
        "ffprobe failed on {}",
        path.display()
    );
    let text = String::from_utf8_lossy(&output.stdout);
    let mut fields = text.trim().split(',');
    let width = fields.next().and_then(|value| value.parse().ok());
    let height = fields.next().and_then(|value| value.parse().ok());
    (width.expect("width"), height.expect("height"))
}

#[test]
fn convert_writes_sized_webp_with_real_ffmpeg() {
    if !(tool_available("ffmpeg") && tool_available("ffprobe")) {
        eprintln!("ffmpeg or ffprobe is not on PATH; skipping the end-to-end test");
        return;
    }
    let directory = tempfile::tempdir().expect("temp dir");
    let input = directory.path().join("Guard Red!.png");
    image::RgbaImage::from_fn(1024, 640, |x, y| {
        image::Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255])
    })
    .save(&input)
    .expect("write png");

    let token = convert(&input, Some(AssetKind::Token), &options(), directory.path())
        .expect("token conversion");
    assert_eq!(token, directory.path().join("tokens/guard-red.webp"));
    assert_eq!(dimensions(&token), (400, 400));

    let map = convert(&input, Some(AssetKind::Map), &options(), directory.path())
        .expect("map conversion");
    assert_eq!(map, directory.path().join("maps/guard-red.webp"));
    assert_eq!(dimensions(&map), (1000, 600));

    let again = convert(&input, Some(AssetKind::Token), &options(), directory.path());
    assert!(matches!(again, Err(Error::Exists(_))), "{again:?}");
    // Neither the successes nor the refused run leave temp files behind.
    let leftovers: Vec<_> = std::fs::read_dir(directory.path().join("tokens"))
        .expect("tokens dir")
        .map(|entry| entry.expect("entry").file_name())
        .filter(|name| name.to_string_lossy().contains(".partial."))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}
