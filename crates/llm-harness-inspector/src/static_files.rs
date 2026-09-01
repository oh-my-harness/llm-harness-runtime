//! Minimal static file serving for `extra_static` mounts.
//!
//! GET-only, path-traversal safe, tiny MIME whitelist. Deliberately avoids
//! a `tower-http` dependency: upper-layer apps host a handful of files
//! (one HTML panel), not a CDN.

use axum::extract::Path;
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use std::path::PathBuf;

pub(crate) fn serve(
    root: PathBuf,
) -> impl Fn(Option<Path<String>>) -> std::future::Ready<Response> + Clone + Send {
    move |rest: Option<Path<String>>| {
        let rel = rest.map(|p| p.0).unwrap_or_default();
        std::future::ready(serve_inner(&root, &rel))
    }
}

fn serve_inner(root: &std::path::Path, rel: &str) -> Response {
    // Reject traversal: no ".." segments and no absolute escapes.
    let rel_path = std::path::Path::new(rel);
    if rel.split('/').any(|seg| seg == "..") {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }

    // Empty path (or trailing slash) → index.html.
    let file_path = if rel.is_empty() || rel.ends_with('/') {
        root.join("index.html")
    } else {
        root.join(rel_path)
    };

    // Containment check: canonicalized path must stay under the root.
    let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    match file_path.canonicalize() {
        Ok(canonical) if canonical.starts_with(&canonical_root) => {}
        _ => return (StatusCode::NOT_FOUND, "not found").into_response(),
    }

    match std::fs::read(&file_path) {
        Ok(bytes) => {
            let mime = mime_of(&file_path);
            if mime == "text/html" {
                match String::from_utf8(bytes) {
                    Ok(text) => Html(text).into_response(),
                    Err(_) => (StatusCode::NOT_FOUND, "not found").into_response(),
                }
            } else {
                ([(header::CONTENT_TYPE, mime)], bytes).into_response()
            }
        }
        Err(_) => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

fn mime_of(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") | Some("htm") => "text/html",
        Some("css") => "text/css",
        Some("js") | Some("mjs") => "application/javascript",
        Some("json") => "application/json",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("txt") | Some("md") => "text/plain",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "insp-static-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn serves_file_and_rejects_traversal() {
        let dir = temp_dir();
        std::fs::write(dir.join("a.txt"), "ok").unwrap();

        let resp = serve_inner(&dir, "a.txt");
        assert_eq!(resp.status(), 200);

        let resp = serve_inner(&dir, "../secrets");
        assert_eq!(resp.status(), 404);

        let resp = serve_inner(&dir, "missing.txt");
        assert_eq!(resp.status(), 404);
    }

    #[test]
    fn empty_path_serves_index() {
        let dir = temp_dir();
        std::fs::write(dir.join("index.html"), "<h1>hi</h1>").unwrap();
        let resp = serve_inner(&dir, "");
        assert_eq!(resp.status(), 200);
    }
}
