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
    ensure_jpeg(&path)
}

/// Force a `@jpeg` suffix on the URL if it doesn't already have an
/// explicit format selector. The Bluesky CDN uses `@jpeg` / `@png` /
/// `@webp` to pick the response encoding; vita2d only decodes JPEG and
/// PNG, so we coerce. Already-suffixed URLs pass through (lets us
/// re-apply this idempotently on lookup + dispatch paths).
pub fn ensure_jpeg(url: &str) -> String {
    if url.ends_with("@jpeg") || url.ends_with("@png") {
        url.to_string()
    } else if url.ends_with("@webp") || url.ends_with("@avif") {
        // Strip the foreign format selector and re-suffix with jpeg.
        let cut = url.rfind('@').expect("'@' present");
        format!("{}@jpeg", &url[..cut])
    } else {
        format!("{url}@jpeg")
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
