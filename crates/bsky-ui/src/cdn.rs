//! Bluesky CDN URL helpers.
//!
//! atrium's `ProfileViewBasic.avatar` field returns the canonical CDN
//! URL — but that points at the WebP variant of the full-size (1000×1000)
//! avatar. vita2d only decodes PNG and JPEG, so we transform the URL to:
//! - swap `/avatar/plain/` for `/avatar_thumbnail/plain/` (128×128, ~2 KB
//!   JPEG instead of ~10 KB WebP — a huge bandwidth + memory win)
//! - append `@jpeg` (forces JPEG output, which our libjpeg-turbo path
//!   handles)

const BSKY_CDN_PREFIX: &str = "https://cdn.bsky.app/img/avatar/plain/";
const BSKY_CDN_THUMB: &str = "https://cdn.bsky.app/img/avatar_thumbnail/plain/";

/// Transform a Bluesky avatar URL into the small-JPEG-thumbnail variant.
/// Returns the input unchanged if it's not a recognized Bluesky avatar
/// URL (e.g. a third-party CDN, custom PDS, or already-thumbnailed URL).
pub fn avatar_thumbnail_jpeg(url: &str) -> String {
    if !url.starts_with(BSKY_CDN_PREFIX) {
        return url.to_string();
    }
    let path = url.replacen(BSKY_CDN_PREFIX, BSKY_CDN_THUMB, 1);
    if path.ends_with("@jpeg") || path.ends_with("@png") {
        path
    } else {
        format!("{path}@jpeg")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transforms_standard_bsky_avatar() {
        let input = "https://cdn.bsky.app/img/avatar/plain/did:plc:abc/bafkrei123";
        let want = "https://cdn.bsky.app/img/avatar_thumbnail/plain/did:plc:abc/bafkrei123@jpeg";
        assert_eq!(avatar_thumbnail_jpeg(input), want);
    }

    #[test]
    fn preserves_existing_jpeg_suffix() {
        let input = "https://cdn.bsky.app/img/avatar/plain/did:plc:abc/bafkrei123@jpeg";
        let want = "https://cdn.bsky.app/img/avatar_thumbnail/plain/did:plc:abc/bafkrei123@jpeg";
        assert_eq!(avatar_thumbnail_jpeg(input), want);
    }

    #[test]
    fn unchanged_for_non_bsky_url() {
        let input = "https://example.com/foo.jpg";
        assert_eq!(avatar_thumbnail_jpeg(input), input);
    }
}
