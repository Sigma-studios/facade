use std::future::Future;
use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;

/// Data fetched on mount, every `every`, and on `reload()`.
pub struct Polled<T: Send + Sync + 'static> {
    pub data: RwSignal<Option<T>>,
    pub error: RwSignal<Option<String>>,
    version: RwSignal<u32>,
}

impl<T: Send + Sync + 'static> Clone for Polled<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T: Send + Sync + 'static> Copy for Polled<T> {}

impl<T: Send + Sync + 'static> Polled<T> {
    pub fn reload(&self) {
        self.version.update(|v| *v += 1);
    }

    pub fn loaded(&self) -> bool {
        self.data.with(Option::is_some)
    }
}

/// `fetch` is called inside an effect: signals it reads before its first `.await`
/// are tracked, so the data reloads when they change (e.g. a route parameter).
pub fn polled<T, F, Fut>(every: Option<Duration>, fetch: F) -> Polled<T>
where
    T: Send + Sync + 'static,
    F: Fn() -> Fut + 'static,
    Fut: Future<Output = Result<T, String>> + 'static,
{
    let polled = Polled {
        data: RwSignal::new(None),
        error: RwSignal::new(None),
        version: RwSignal::new(0),
    };
    Effect::new(move |_| {
        polled.version.track();
        let future = fetch();
        spawn_local(async move {
            match future.await {
                Ok(value) => {
                    polled.data.try_set(Some(value));
                    polled.error.try_set(None);
                }
                Err(e) => {
                    polled.error.try_set(Some(e));
                }
            }
        });
    });
    if let Some(every) = every
        && let Ok(handle) = set_interval_with_handle(move || polled.reload(), every)
    {
        on_cleanup(move || handle.clear());
    }
    polled
}

// ---------------------------------------------------------------- toasts

#[derive(Clone, PartialEq)]
pub struct Toast {
    pub id: u64,
    pub message: String,
    pub error: bool,
}

#[derive(Clone, Copy)]
pub struct Toasts(pub RwSignal<Vec<Toast>>);

thread_local! {
    // Global rather than a context: toasts are raised from async tasks, which run
    // outside any reactive owner.
    static TOASTS: Toasts = Toasts(RwSignal::new(vec![]));
}

pub fn toasts() -> Toasts {
    TOASTS.with(|t| *t)
}

impl Toasts {
    fn push(&self, message: String, error: bool) {
        let id = (js_sys::Math::random() * 1e15) as u64;
        self.0.update(|t| t.push(Toast { id, message, error }));
        let list = self.0;
        set_timeout(
            move || list.update(|t| t.retain(|x| x.id != id)),
            Duration::from_millis(if error { 8000 } else { 3500 }),
        );
    }

    pub fn ok(&self, message: impl Into<String>) {
        self.push(message.into(), false);
    }

    pub fn err(&self, message: impl Into<String>) {
        self.push(message.into(), true);
    }

    pub fn dismiss(&self, id: u64) {
        self.0.update(|t| t.retain(|x| x.id != id));
    }
}

/// Runs an API call, reporting success or failure as a toast.
pub fn run<Fut>(success: impl Into<String>, future: Fut, then: impl FnOnce() + 'static)
where
    Fut: Future<Output = Result<(), String>> + 'static,
{
    let toasts = toasts();
    let success = success.into();
    spawn_local(async move {
        match future.await {
            Ok(()) => {
                if !success.is_empty() {
                    toasts.ok(success);
                }
                then();
            }
            Err(e) => toasts.err(e),
        }
    });
}

// ---------------------------------------------------------------- formatting

pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn now() -> u64 {
    (js_sys::Date::now() / 1000.0) as u64
}

pub fn ago(at: u64) -> String {
    let secs = now().saturating_sub(at);
    match secs {
        0..5 => "just now".into(),
        5..60 => format!("{secs}s ago"),
        60..3600 => format!("{}m ago", secs / 60),
        3600..86400 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

pub fn clock(at: u64) -> String {
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(at as f64 * 1000.0));
    format!("{:02}:{:02}", date.get_hours(), date.get_minutes())
}

/// Local date, `2027-01-18`.
pub fn date(at: u64) -> String {
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(at as f64 * 1000.0));
    format!(
        "{:04}-{:02}-{:02}",
        date.get_full_year(),
        date.get_month() + 1,
        date.get_date()
    )
}

/// The 7-character form of a git commit hash.
pub fn short_hash(hash: &str) -> &str {
    hash.get(..7).unwrap_or(hash)
}

pub fn percent(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 * 100.0 / whole as f64
    }
}

/// Removes ANSI colour codes that apps write to their logs.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub fn copy_to_clipboard(text: &str) {
    if let Some(window) = web_sys::window() {
        let _ = window.navigator().clipboard().write_text(text);
    }
}

pub fn download(filename: &str, contents: &str, mime: &str) {
    let parts = js_sys::Array::of1(&wasm_bindgen::JsValue::from_str(contents));
    let options = web_sys::BlobPropertyBag::new();
    options.set_type(mime);
    if let Ok(blob) = web_sys::Blob::new_with_str_sequence_and_options(&parts, &options) {
        save_blob(filename, &blob);
    }
}

/// Hands a blob to the browser as a file download.
pub fn save_blob(filename: &str, blob: &web_sys::Blob) {
    use wasm_bindgen::JsCast;
    let Ok(url) = web_sys::Url::create_object_url_with_blob(blob) else {
        return;
    };
    let document = window().document().expect("document");
    if let Ok(a) = document.create_element("a") {
        let a: web_sys::HtmlAnchorElement = a.unchecked_into();
        a.set_href(&url);
        a.set_download(filename);
        a.click();
    }
    // Revoked later: some browsers still read a large blob after the click returns.
    set_timeout(
        move || {
            let _ = web_sys::Url::revoke_object_url(&url);
        },
        Duration::from_secs(60),
    );
}
