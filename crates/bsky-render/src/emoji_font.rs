use std::marker::PhantomData;

#[cfg(any(target_os = "vita", test))]
const MAX_INPUT_BYTES: usize = 4096;
#[cfg(any(target_os = "vita", test))]
const MAX_GLYPHS: usize = 512;
#[cfg(any(target_os = "vita", test))]
const MAX_DIMENSION: u32 = 256;
#[cfg(any(target_os = "vita", test))]
const MAX_PIXELS: usize = 256 * 256;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Glyph {
    pub id: u32,
    pub x_advance_26_6: i32,
    pub y_advance_26_6: i32,
    pub x_offset_26_6: i32,
    pub y_offset_26_6: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BitmapMetrics {
    pub width: u32,
    pub rows: u32,
    pub left: i32,
    pub top: i32,
}

#[derive(Debug, PartialEq, Eq)]
pub enum FontError {
    NotOnVita,
    InvalidInput,
    UnsupportedSequence,
    Capacity,
    Native,
}

#[cfg(target_os = "vita")]
mod native {
    use super::*;
    use std::ffi::{c_char, c_int, c_void, CString};
    use std::ptr::NonNull;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct NativeGlyph {
        id: u32,
        x_advance: i32,
        y_advance: i32,
        x_offset: i32,
        y_offset: i32,
    }

    unsafe extern "C" {
        fn emoji_font_open(path: *const c_char) -> *mut c_void;
        fn emoji_font_close(font: *mut c_void);
        fn emoji_font_strike_ppem(font: *const c_void) -> u32;
        fn emoji_font_shape(
            font: *mut c_void,
            text: *const u8,
            len: u32,
            out: *mut NativeGlyph,
            cap: u32,
        ) -> c_int;
        fn emoji_font_render(
            font: *mut c_void,
            glyph: u32,
            width: *mut u32,
            rows: *mut u32,
            left: *mut i32,
            top: *mut i32,
            pixels: *mut u8,
            capacity: u32,
        ) -> c_int;
    }

    pub struct EmojiFont {
        raw: NonNull<c_void>,
        ppem: u32,
        _not_send_sync: PhantomData<*mut ()>,
    }

    impl EmojiFont {
        pub fn open(path: &str) -> Result<Self, FontError> {
            let path = CString::new(path).map_err(|_| FontError::InvalidInput)?;
            let raw =
                NonNull::new(unsafe { emoji_font_open(path.as_ptr()) }).ok_or(FontError::Native)?;
            let ppem = unsafe { emoji_font_strike_ppem(raw.as_ptr()) };
            if ppem == 0 || ppem > MAX_DIMENSION {
                unsafe { emoji_font_close(raw.as_ptr()) };
                return Err(FontError::Native);
            }
            Ok(Self {
                raw,
                ppem,
                _not_send_sync: PhantomData,
            })
        }
        pub fn strike_ppem(&self) -> u32 {
            self.ppem
        }
        pub fn shape(&mut self, text: &str) -> Result<Vec<Glyph>, FontError> {
            if text.is_empty() || text.len() > MAX_INPUT_BYTES {
                return Err(FontError::Capacity);
            }
            let mut out = vec![NativeGlyph::default(); MAX_GLYPHS];
            let count = unsafe {
                emoji_font_shape(
                    self.raw.as_ptr(),
                    text.as_ptr(),
                    text.len() as u32,
                    out.as_mut_ptr(),
                    out.len() as u32,
                )
            };
            if count < 0 {
                return Err(if count == -2 {
                    FontError::UnsupportedSequence
                } else if count == -3 {
                    FontError::Capacity
                } else {
                    FontError::Native
                });
            }
            out.truncate(count as usize);
            Ok(out
                .into_iter()
                .map(|g| Glyph {
                    id: g.id,
                    x_advance_26_6: g.x_advance,
                    y_advance_26_6: g.y_advance,
                    x_offset_26_6: g.x_offset,
                    y_offset_26_6: g.y_offset,
                })
                .collect())
        }
        pub fn render_rgba(
            &mut self,
            glyph: u32,
            target_ppem: u32,
        ) -> Result<(BitmapMetrics, Vec<u8>), FontError> {
            if target_ppem == 0 || target_ppem > MAX_DIMENSION {
                return Err(FontError::Capacity);
            }
            let (mut width, mut rows, mut left, mut top) = (0, 0, 0, 0);
            let mut bgra = vec![0; MAX_PIXELS * 4];
            let status = unsafe {
                emoji_font_render(
                    self.raw.as_ptr(),
                    glyph,
                    &mut width,
                    &mut rows,
                    &mut left,
                    &mut top,
                    bgra.as_mut_ptr(),
                    bgra.len() as u32,
                )
            };
            if status != 0 {
                return Err(FontError::Native);
            }
            if width > MAX_DIMENSION
                || rows > MAX_DIMENSION
                || width as usize * rows as usize > MAX_PIXELS
            {
                return Err(FontError::Capacity);
            }
            let len = width as usize * rows as usize * 4;
            bgra.truncate(len);
            let factor_num = target_ppem;
            let factor_den = self.ppem;
            let out_w =
                ((width * factor_num + factor_den / 2) / factor_den).max(u32::from(width > 0));
            let out_h =
                ((rows * factor_num + factor_den / 2) / factor_den).max(u32::from(rows > 0));
            if out_w > MAX_DIMENSION
                || out_h > MAX_DIMENSION
                || out_w as usize * out_h as usize > MAX_PIXELS
            {
                return Err(FontError::Capacity);
            }
            let rgba = resample_bgra_to_rgba(
                &bgra,
                width as usize,
                rows as usize,
                out_w as usize,
                out_h as usize,
            )?;
            let scale = target_ppem as f32 / self.ppem as f32;
            Ok((
                BitmapMetrics {
                    width: out_w,
                    rows: out_h,
                    left: (left as f32 * scale).round() as i32,
                    top: (top as f32 * scale).round() as i32,
                },
                rgba,
            ))
        }
    }
    impl Drop for EmojiFont {
        fn drop(&mut self) {
            unsafe { emoji_font_close(self.raw.as_ptr()) }
        }
    }
}

#[cfg(target_os = "vita")]
pub use native::EmojiFont;
#[cfg(not(target_os = "vita"))]
pub struct EmojiFont(PhantomData<*mut ()>);
#[cfg(not(target_os = "vita"))]
impl EmojiFont {
    pub fn open(_: &str) -> Result<Self, FontError> {
        Err(FontError::NotOnVita)
    }
    pub fn shape(&mut self, _: &str) -> Result<Vec<Glyph>, FontError> {
        Err(FontError::NotOnVita)
    }
    pub fn strike_ppem(&self) -> u32 {
        0
    }
    pub fn render_rgba(&mut self, _: u32, _: u32) -> Result<(BitmapMetrics, Vec<u8>), FontError> {
        Err(FontError::NotOnVita)
    }
}

#[cfg(any(target_os = "vita", test))]
fn resample_bgra_to_rgba(
    input: &[u8],
    width: usize,
    rows: usize,
    out_w: usize,
    out_h: usize,
) -> Result<Vec<u8>, FontError> {
    if width == 0 || rows == 0 {
        return if input.is_empty() {
            Ok(Vec::new())
        } else {
            Err(FontError::InvalidInput)
        };
    }
    if width > MAX_DIMENSION as usize
        || rows > MAX_DIMENSION as usize
        || out_w == 0
        || out_h == 0
        || out_w > MAX_DIMENSION as usize
        || out_h > MAX_DIMENSION as usize
        || input.len()
            != width
                .checked_mul(rows)
                .and_then(|n| n.checked_mul(4))
                .ok_or(FontError::Capacity)?
    {
        return Err(FontError::InvalidInput);
    }
    let mut out = vec![0; out_w * out_h * 4];
    for oy in 0..out_h {
        for ox in 0..out_w {
            let x0 = ox * width / out_w;
            let x1 = ((ox + 1) * width / out_w).max(x0 + 1).min(width);
            let y0 = oy * rows / out_h;
            let y1 = ((oy + 1) * rows / out_h).max(y0 + 1).min(rows);
            let n = ((x1 - x0) * (y1 - y0)) as u32;
            let mut sums = [0u32; 4];
            for y in y0..y1 {
                for x in x0..x1 {
                    let i = (y * width + x) * 4;
                    let a = u32::from(input[i + 3]);
                    sums[0] += u32::from(input[i + 2]);
                    sums[1] += u32::from(input[i + 1]);
                    sums[2] += u32::from(input[i]);
                    sums[3] += a;
                }
            }
            let o = (oy * out_w + ox) * 4;
            let a = ((sums[3] + n / 2) / n) as u8;
            out[o..o + 4].copy_from_slice(&[
                unpremultiply(((sums[0] + n / 2) / n) as u8, a),
                unpremultiply(((sums[1] + n / 2) / n) as u8, a),
                unpremultiply(((sums[2] + n / 2) / n) as u8, a),
                a,
            ]);
        }
    }
    Ok(out)
}

#[cfg(any(target_os = "vita", test))]
fn unpremultiply(channel: u8, alpha: u8) -> u8 {
    if alpha == 0 {
        0
    } else {
        ((u16::from(channel) * 255 + u16::from(alpha) / 2) / u16::from(alpha)).min(255) as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resamples_premultiplied_edges_before_unpremultiplying() {
        let bgra = [0, 0, 0, 0, 0, 0, 128, 128];
        assert_eq!(
            resample_bgra_to_rgba(&bgra, 2, 1, 1, 1).unwrap(),
            [255, 0, 0, 64]
        );
    }
    #[test]
    fn native_bitmap_empty_contract_is_safe() {
        assert_eq!(
            resample_bgra_to_rgba(&[], 0, 0, 0, 0).unwrap(),
            Vec::<u8>::new()
        );
    }
}
