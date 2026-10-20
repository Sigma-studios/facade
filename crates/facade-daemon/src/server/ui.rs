//! The web UI (crates/facade-ui, built with trunk) served next to the API.
//!
//! Release builds embed `crates/facade-ui/dist`; debug builds read it from disk, so a
//! `trunk build` shows up on reload without rebuilding the daemon.

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};

#[derive(rust_embed::Embed)]
#[folder = "../facade-ui/dist"]
#[allow_missing = true]
struct Assets;

/// Files by path, and `index.html` for every other route (client-side routing).
pub async fn serve(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if path.starts_with("api/") {
        return (StatusCode::NOT_FOUND, "no such API endpoint").into_response();
    }
    if let Some(file) = Assets::get(path).filter(|_| !path.is_empty()) {
        // Trunk puts a content hash in asset names, so they never change.
        return file_response(file, "public, max-age=31536000, immutable");
    }
    match Assets::get("index.html") {
        Some(file) => file_response(file, "no-cache"),
        None => (
            StatusCode::NOT_FOUND,
            "The web UI is not built into this binary. Build it with \
             `cd crates/facade-ui && trunk build --release`, then rebuild facaded.",
        )
            .into_response(),
    }
}

fn file_response(file: rust_embed::EmbeddedFile, cache: &'static str) -> Response {
    let mut response = Body::from(file.data.into_owned()).into_response();
    let headers = response.headers_mut();
    if let Ok(mime) = HeaderValue::from_str(file.metadata.mimetype()) {
        headers.insert(header::CONTENT_TYPE, mime);
    }
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}
