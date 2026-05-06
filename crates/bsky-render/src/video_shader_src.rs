//! Cg source for the Phase 5.3.x.1 video YUV→RGB shader pair.
//!
//! Compiled at runtime by libshacccg.suprx via direct sceShaccCg* API
//! (vitashark's wrapper hits SCE_KERNEL_ERROR_MODULEMGR_OLD_LIB when
//! the suprx is loaded as a user module). Result is registered with
//! vita2d's GXM shader patcher and used by `Frame::draw_video_yuv`
//! for video frames only.
//!
//! Math: BT.601 limited-range conversion. Bsky's typical clip is 480p
//! (definitely BT.601) or 720p (mixed industry; BT.601 acceptable). If
//! 1080p+ ever shows tinting, switch to BT.709 here.
//!
//! Parameter names: `position` and `texcoord` are looked up by name in
//! `init_pipeline` to discover their resource indices for
//! `SceGxmVertexAttribute`. The fragment shader's three samplers are
//! bound by index 0, 1, 2 (decl order = GXM texture register on Vita's
//! Cg compiler).

#![cfg(target_os = "vita")]

/// Vertex shader: pass through position + texcoord. Position arrives in
/// NDC (CPU does the pixel→NDC conversion), so the shader is just an
/// identity write to POSITION.
pub(crate) const VIDEO_YUV_VERT: &str = r#"
void main(
    float2 position,
    float2 texcoord,
    out float4 vPosition : POSITION,
    out float2 vTexcoord : TEXCOORD0)
{
    vPosition = float4(position, 0.0, 1.0);
    vTexcoord = texcoord;
}
"#;

/// Fragment shader: sample three luma-format planes (Y, U, V), apply
/// BT.601 limited-range matrix, output opaque RGB.
///
/// Constants:
/// - 0.0627 = 16/255 (Y black-point bias)
/// - 0.5020 = 128/255 (chroma center)
/// - 1.164, 1.596, 0.392, 0.813, 2.017 = BT.601 limited-range coefficients
pub(crate) const VIDEO_YUV_FRAG: &str = r#"
float4 main(
    float2 vTexcoord : TEXCOORD0,
    uniform sampler2D y_tex,
    uniform sampler2D u_tex,
    uniform sampler2D v_tex) : COLOR
{
    float Y = tex2D(y_tex, vTexcoord).r - 0.0627;
    float U = tex2D(u_tex, vTexcoord).r - 0.5020;
    float V = tex2D(v_tex, vTexcoord).r - 0.5020;
    float r = 1.164 * Y + 1.596 * V;
    float g = 1.164 * Y - 0.392 * U - 0.813 * V;
    float b = 1.164 * Y + 2.017 * U;
    return float4(saturate(float3(r, g, b)), 1.0);
}
"#;
