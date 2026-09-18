use axum::extract::Path;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use include_dir::{Dir, include_dir};

static FRONTEND_DIST: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/frontend/dist");

const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; font-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

pub(crate) async fn index() -> Response {
    serve_path("")
}

pub(crate) async fn asset(Path(path): Path<String>) -> Response {
    serve_path(&path)
}

fn serve_path(path: &str) -> Response {
    if !is_safe_path(path) {
        return not_found();
    }

    let lookup_path = if path.is_empty() || path.ends_with('/') {
        format!("{path}index.html")
    } else {
        path.to_owned()
    };
    let Some(file) = FRONTEND_DIST.get_file(&lookup_path) else {
        return not_found();
    };

    let file_path = file.path().to_str().expect("embedded path must be UTF-8");
    (
        [
            (header::CONTENT_TYPE, mime_of(file_path)),
            (header::CACHE_CONTROL, cache_control_of(file_path)),
            (header::CONTENT_SECURITY_POLICY, CONTENT_SECURITY_POLICY),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::X_FRAME_OPTIONS, "DENY"),
        ],
        file.contents(),
    )
        .into_response()
}

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        [
            (header::CONTENT_SECURITY_POLICY, CONTENT_SECURITY_POLICY),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::X_FRAME_OPTIONS, "DENY"),
        ],
        "not found",
    )
        .into_response()
}

fn is_safe_path(path: &str) -> bool {
    if path.contains('\\') {
        return false;
    }

    std::path::Path::new(path)
        .components()
        .all(|component| matches!(component, std::path::Component::Normal(_)))
}

fn mime_of(path: &str) -> &'static str {
    match std::path::Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
    {
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("json" | "map") => "application/json; charset=utf-8",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("wasm") => "application/wasm",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn cache_control_of(path: &str) -> &'static str {
    if path.ends_with(".html") {
        "no-store"
    } else if is_content_hashed_asset(path) {
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=300"
    }
}

fn is_content_hashed_asset(path: &str) -> bool {
    let Some((file_stem, extension)) = path.rsplit_once('.') else {
        return false;
    };
    if !matches!(extension, "js" | "css" | "wasm" | "woff2") {
        return false;
    }

    let Some(hash) = file_stem.rsplit_once('-').map(|(_, hash)| hash) else {
        return false;
    };
    hash.len() >= 8
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serves_index_with_security_headers() {
        let response = serve_path("");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/html; charset=utf-8"
        );
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_SECURITY_POLICY)
                .unwrap(),
            CONTENT_SECURITY_POLICY
        );
        assert_eq!(
            response
                .headers()
                .get(header::X_CONTENT_TYPE_OPTIONS)
                .unwrap(),
            "nosniff"
        );
    }

    #[test]
    fn serves_embedded_asset_with_immutable_cache() {
        let index = FRONTEND_DIST.get_file("index.html").unwrap();
        let index_text = String::from_utf8(index.contents().to_vec()).unwrap();
        let script_path = index_text
            .split("src=\"/app/agent-team/")
            .nth(1)
            .and_then(|suffix| suffix.split('"').next())
            .unwrap();
        let response = serve_path(script_path);
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "public, max-age=31536000, immutable"
        );
    }

    #[test]
    fn rejects_traversal_paths() {
        for path in ["../Cargo.toml", "assets/../../Cargo.toml", "/etc/passwd"] {
            let response = serve_path(path);
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }
    }
}
