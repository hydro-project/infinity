#[cfg(feature = "bundled-web")]
use include_dir::{Dir, include_dir};

#[cfg(feature = "bundled-web")]
static WEB_DIST: Dir = include_dir!("$CARGO_MANIFEST_DIR/../../infinity-web/dist");

/// Serve a static file from the bundled web UI.
/// Returns `(content_type, body)` or `None` if not found / not bundled.
pub fn serve_static(path: &str) -> Option<(&'static str, &'static [u8])> {
    #[cfg(feature = "bundled-web")]
    {
        let path = if path == "/" {
            "index.html"
        } else {
            path.strip_prefix('/').unwrap_or(path)
        };
        let file = WEB_DIST.get_file(path)?;
        let content_type = match path.rsplit_once('.').map(|(_, ext)| ext) {
            Some("html") => "text/html; charset=utf-8",
            Some("js") => "application/javascript",
            Some("css") => "text/css",
            Some("json") => "application/json",
            Some("svg") => "image/svg+xml",
            Some("png") => "image/png",
            Some("ico") => "image/x-icon",
            Some("woff2") => "font/woff2",
            Some("woff") => "font/woff",
            Some("ttf") => "font/ttf",
            Some("map") => "application/json",
            _ => "application/octet-stream",
        };
        Some((content_type, file.contents()))
    }

    #[cfg(not(feature = "bundled-web"))]
    {
        let _ = path;
        None
    }
}
