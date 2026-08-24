use axum::response::Html;
pub async fn serve_ui() -> Html<&'static str> {
    Html(crate::ui::INDEX_HTML)
}
