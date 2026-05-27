//! Local-image helpers: decode on the CPU (via `bsky_media::image`) and
//! upload a small texture, instead of asking the GPU (vita2d) to decode a
//! local file. vita2d crashes decoding multi-MB images into large GPU
//! textures, so the picker thumbnails, compose previews, and the upload
//! downscale path all go through here.

use bsky_render::Texture;

/// Decode PNG/JPEG `bytes` on the CPU, downscale to fit `max_w × max_h`,
/// and upload the result as a small RGBA texture. `None` on decode error.
pub fn decode_thumb(bytes: &[u8], max_w: u32, max_h: u32) -> Option<Texture> {
    let (rgba, w, h) = bsky_media::image::decode_rgba(bytes).ok()?;
    let (rgba, w, h) = bsky_media::image::downscale_rgba(rgba, w, h, max_w, max_h);
    let tex = Texture::new_rgba(w, h).ok()?;
    tex.upload_rgba(&rgba);
    Some(tex)
}
