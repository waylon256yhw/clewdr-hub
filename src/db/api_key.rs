use rand::RngExt;

const KEY_PREFIX: &str = "sk-";
const KEY_BODY_LEN: usize = 40;
const LOOKUP_KEY_LEN: usize = 8;
const CHARSET: &[u8] = b"abcdefghijkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// Generate a new API key. Returns (plaintext_key, lookup_key, blake3_hash_bytes).
pub fn generate_api_key() -> (String, String, [u8; 32]) {
    let mut rng = rand::rng();
    let body: String = (0..KEY_BODY_LEN)
        .map(|_| {
            let idx = rng.random_range(0..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect();
    let plaintext = format!("{KEY_PREFIX}{body}");
    let lookup_key = body[..LOOKUP_KEY_LEN].to_string();
    let key_hash = blake3::hash(plaintext.as_bytes());
    (plaintext, lookup_key, *key_hash.as_bytes())
}

/// Parse a submitted API key into (lookup_key, blake3_hash) for verification.
/// Returns None if the key doesn't have the expected format.
pub fn parse_api_key(key: &str) -> Option<(String, [u8; 32])> {
    let body = key.strip_prefix(KEY_PREFIX)?;
    if body.len() < LOOKUP_KEY_LEN {
        return None;
    }
    let lookup_key = body[..LOOKUP_KEY_LEN].to_string();
    let hash = blake3::hash(key.as_bytes());
    Some((lookup_key, *hash.as_bytes()))
}
