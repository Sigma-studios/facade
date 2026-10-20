//! Calls to the facade API. The session token lives in localStorage and goes in an
//! `Authorization` header; a 401 sends the user back to the login page.

use facade_core::api::ErrorBody;
use futures_util::StreamExt;
use gloo_net::http::{Method, RequestBuilder, Response};
use serde::Serialize;
use serde::de::DeserializeOwned;
use wasm_bindgen::JsCast;

const TOKEN_KEY: &str = "facade.token";

fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok()?
}

pub fn token() -> Option<String> {
    storage()?.get_item(TOKEN_KEY).ok()?
}

pub fn set_token(token: Option<&str>) {
    if let Some(storage) = storage() {
        let _ = match token {
            Some(t) => storage.set_item(TOKEN_KEY, t),
            None => storage.remove_item(TOKEN_KEY),
        };
    }
}

fn go_to_login() {
    set_token(None);
    if let Some(window) = web_sys::window() {
        let _ = window.location().set_href("/login");
    }
}

enum Body {
    None,
    Json(String),
    Text(String),
    Raw(wasm_bindgen::JsValue),
}

async fn call(
    method: Method,
    path: &str,
    body: Body,
    abort: Option<&web_sys::AbortSignal>,
) -> Result<Response, String> {
    let mut builder = RequestBuilder::new(path).method(method);
    if let Some(token) = token() {
        builder = builder.header("Authorization", &format!("Bearer {token}"));
    }
    builder = builder.abort_signal(abort);
    let request = match body {
        Body::None => builder.build(),
        Body::Json(json) => builder
            .header("Content-Type", "application/json")
            .body(json),
        Body::Text(text) => builder.header("Content-Type", "text/plain").body(text),
        Body::Raw(raw) => builder
            .header("Content-Type", "application/octet-stream")
            .body(raw),
    }
    .map_err(|e| e.to_string())?;
    let response = request
        .send()
        .await
        .map_err(|e| format!("cannot reach the server: {e}"))?;
    if response.ok() {
        return Ok(response);
    }
    if response.status() == 401 && !path.ends_with("/login") {
        go_to_login();
        return Err("session expired".into());
    }
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    Err(serde_json::from_str::<ErrorBody>(&text)
        .map(|e| e.error)
        .unwrap_or_else(|_| format!("request failed ({status})")))
}

fn json_body<B: Serialize>(body: &B) -> Body {
    Body::Json(serde_json::to_string(body).expect("serializable"))
}

async fn decode<T: DeserializeOwned>(response: Response) -> Result<T, String> {
    response.json().await.map_err(|e| e.to_string())
}

pub async fn get<T: DeserializeOwned>(path: &str) -> Result<T, String> {
    decode(call(Method::GET, path, Body::None, None).await?).await
}

pub async fn get_text(path: &str) -> Result<String, String> {
    call(Method::GET, path, Body::None, None)
        .await?
        .text()
        .await
        .map_err(|e| e.to_string())
}

pub async fn post<B: Serialize, T: DeserializeOwned>(path: &str, body: &B) -> Result<T, String> {
    decode(call(Method::POST, path, json_body(body), None).await?).await
}

pub async fn post_text<T: DeserializeOwned>(path: &str, body: String) -> Result<T, String> {
    decode(call(Method::POST, path, Body::Text(body), None).await?).await
}

/// POST without a body, ignoring the response.
pub async fn act(path: &str) -> Result<(), String> {
    call(Method::POST, path, Body::None, None).await.map(|_| ())
}

pub async fn act_json<T: DeserializeOwned>(path: &str) -> Result<T, String> {
    decode(call(Method::POST, path, Body::None, None).await?).await
}

pub async fn put<B: Serialize, T: DeserializeOwned>(path: &str, body: &B) -> Result<T, String> {
    decode(call(Method::PUT, path, json_body(body), None).await?).await
}

pub async fn put_empty<B: Serialize>(path: &str, body: &B) -> Result<(), String> {
    call(Method::PUT, path, json_body(body), None)
        .await
        .map(|_| ())
}

pub async fn patch<B: Serialize>(path: &str, body: &B) -> Result<(), String> {
    call(Method::PATCH, path, json_body(body), None)
        .await
        .map(|_| ())
}

pub async fn delete(path: &str) -> Result<(), String> {
    call(Method::DELETE, path, Body::None, None)
        .await
        .map(|_| ())
}

/// Uploads a file as an executable artifact.
pub async fn upload(file: &web_sys::File) -> Result<facade_core::api::ArtifactView, String> {
    let name = String::from(js_sys::encode_uri_component(&file.name()));
    let path = format!("/api/artifacts?filename={name}");
    let body = Body::Raw(file.clone().into());
    decode(call(Method::POST, &path, body, None).await?).await
}

/// Uploads a file as the raw request body.
pub async fn put_file(path: &str, file: &web_sys::File) -> Result<(), String> {
    call(Method::PUT, path, Body::Raw(file.clone().into()), None)
        .await
        .map(|_| ())
}

/// Fetches a file (a volume archive, a backup) and saves it under the name the
/// server suggests, or `fallback`.
pub async fn download(path: &str, fallback: &str) -> Result<(), String> {
    let response = call(Method::GET, path, Body::None, None).await?;
    let filename = response
        .headers()
        .get("content-disposition")
        .and_then(|d| {
            let name = d.split_once("filename=")?.1.trim_matches('"');
            (!name.is_empty()).then(|| name.to_string())
        })
        .unwrap_or_else(|| fallback.to_string());
    let raw: web_sys::Response = response.into();
    let blob = raw.blob().map_err(|_| "cannot read the download")?;
    let blob: web_sys::Blob = wasm_bindgen_futures::JsFuture::from(blob)
        .await
        .map_err(|_| "download interrupted")?
        .unchecked_into();
    crate::util::save_blob(&filename, &blob);
    Ok(())
}

/// Streams a text response chunk by chunk until it ends or `abort` fires.
pub async fn stream_text(
    path: &str,
    abort: &web_sys::AbortSignal,
    mut on_chunk: impl FnMut(String),
) -> Result<(), String> {
    let response = call(Method::GET, path, Body::None, Some(abort)).await?;
    let body = response.body().ok_or("empty response")?;
    let mut stream = wasm_streams::ReadableStream::from_raw(body.unchecked_into()).into_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "stream interrupted".to_string())?;
        let bytes = js_sys::Uint8Array::new(&chunk).to_vec();
        on_chunk(String::from_utf8_lossy(&bytes).into_owned());
    }
    Ok(())
}
