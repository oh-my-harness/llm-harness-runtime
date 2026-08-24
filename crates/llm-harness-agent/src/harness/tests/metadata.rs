use super::*;

#[tokio::test]
async fn session_metadata_accessor() {
    let h = make_harness(vec![]);
    let meta = h.session_metadata().await.unwrap();
    assert!(!meta.id.is_empty());
}
