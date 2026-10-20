//! The browser UI, embedded in the binary so there is nothing to install and
//! nothing to serve from disk.

pub const INDEX_HTML: &str = include_str!("web/index.html");
pub const APP_CSS: &str = include_str!("web/app.css");
pub const APP_JS: &str = include_str!("web/app.js");

/// Content type and body for a UI asset, if the path is one.
pub fn asset(path: &str) -> Option<(&'static str, &'static str)> {
    match path {
        "/" | "/index.html" => Some(("text/html; charset=utf-8", INDEX_HTML)),
        "/app.css" => Some(("text/css; charset=utf-8", APP_CSS)),
        "/app.js" => Some(("application/javascript; charset=utf-8", APP_JS)),
        _ => None,
    }
}
