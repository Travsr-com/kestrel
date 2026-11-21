//! The UI: files from the build in `ui/dist`, embedded in the binary. Paths without a file
//! extension are UI routes and get `index.html`. Without a build, a page says how to make one.

use axum::extract::State;
use axum::http::{HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};

use crate::AppState;
use crate::api::error;

/// One file of the UI build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub body: Vec<u8>,
    pub mime: String,
}

/// Where the server finds the UI files, by path relative to the build root (`index.html`,
/// `assets/index-abc.js`).
pub trait Assets: Send + Sync + 'static {
    fn get(&self, path: &str) -> Option<Asset>;
}

/// The UI build embedded at compile time from `ui/dist` (`npm run build` in `ui/`). Compiles
/// without one: then every UI route gets [`PLACEHOLDER`].
pub struct EmbeddedUi;

#[derive(rust_embed::RustEmbed)]
#[folder = "../../ui/dist/"]
#[allow_missing = true]
struct Dist;

impl Assets for EmbeddedUi {
    fn get(&self, path: &str) -> Option<Asset> {
        Dist::get(path).map(|file| Asset {
            mime: file.metadata.mimetype().to_string(),
            body: file.data.into_owned(),
        })
    }
}

/// The page served when the binary has no UI build.
pub const PLACEHOLDER: &str = "<!doctype html>
<html lang=\"en\">
<head><meta charset=\"utf-8\"><title>Kestrel</title></head>
<body>
<h1>Kestrel is running</h1>
<p>This build has no web UI. To add it, build the UI and then Kestrel:</p>
<pre>cd ui
npm ci
npm run build
cd ..
cargo build --release -p kestrel-cli</pre>
<p>The API works without it: see docs/api.md.</p>
</body>
</html>
";

/// Serves a UI file, `index.html` for a UI route, or the placeholder. Only GET and HEAD.
pub(crate) async fn serve(State(s): State<AppState>, method: Method, uri: Uri) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return error(StatusCode::NOT_FOUND, "not found");
    }
    let path = uri.path().trim_start_matches('/');
    if !path.is_empty()
        && let Some(asset) = s.ui.get(path)
    {
        return file(asset, false);
    }
    let last = path.rsplit('/').next().unwrap_or_default();
    if last.contains('.') {
        return error(StatusCode::NOT_FOUND, format!("no file /{path}"));
    }
    match s.ui.get("index.html") {
        Some(index) => file(index, true),
        None => file(
            Asset {
                body: PLACEHOLDER.as_bytes().to_vec(),
                mime: "text/html; charset=utf-8".into(),
            },
            true,
        ),
    }
}

fn file(asset: Asset, page: bool) -> Response {
    let mut res = asset.body.into_response();
    let headers = res.headers_mut();
    if let Ok(mime) = HeaderValue::from_str(&asset.mime) {
        headers.insert(header::CONTENT_TYPE, mime);
    }
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    if page {
        // The page may carry the token in its URL; keep it out of caches.
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    res
}
