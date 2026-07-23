use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};

pub fn content_revision(content: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(content))
}
