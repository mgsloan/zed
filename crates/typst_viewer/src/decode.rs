//! Turning a wire payload into a `RenderImage`.
//!
//! This runs on the **background** executor. A ~2 Mpx page is 8 MB to convert
//! and, over the network, a PNG decode of tens of milliseconds; on the
//! foreground thread that is dropped frames in every other Zed window.

use anyhow::{Context as _, Result, bail};
use gpui::RenderImage;
use image::{Frame, ImageBuffer, Rgba};
use smallvec::SmallVec;
use std::sync::Arc;

use crate::protocol::Encoding;

/// Decodes one page image into the format GPUI uploads.
///
/// GPUI wants **BGRA8 with straight alpha**. Because we ask the server for
/// opaque pages (`opaque: true` in `view`), premultiplied and straight are the
/// same bytes, so the conversion is a bare R<->B swap.
pub fn decode_page(
    payload: &[u8],
    px_width: u32,
    px_height: u32,
    encoding: Encoding,
    rendered_scale: f32,
) -> Result<Arc<RenderImage>> {
    let mut buffer = match encoding {
        // Already RGBA8, tightly packed; the length was validated against the
        // header during parsing.
        Encoding::Raw => payload.to_vec(),
        Encoding::Png => decode_png(payload, px_width, px_height)?,
    };

    swap_rgba_to_bgra(&mut buffer);

    let image = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(px_width, px_height, buffer)
        .context("decoded buffer does not match the declared dimensions")?;

    // The *rendered* scale, which during a zoom transition is deliberately not
    // the view's current scale: it is what makes a stale-scale image draw at the
    // right size rather than merely the wrong sharpness.
    Ok(Arc::new(
        RenderImage::new(SmallVec::from_const([Frame::new(image)]))
            .with_scale_factor(rendered_scale),
    ))
}

fn decode_png(payload: &[u8], px_width: u32, px_height: u32) -> Result<Vec<u8>> {
    let decoded = image::load_from_memory_with_format(payload, image::ImageFormat::Png)
        .context("decoding PNG page")?
        .into_rgba8();
    if decoded.width() != px_width || decoded.height() != px_height {
        bail!(
            "PNG is {}x{} but the header said {px_width}x{px_height}",
            decoded.width(),
            decoded.height(),
        );
    }
    Ok(decoded.into_raw())
}

/// Swaps the red and blue channels in place.
///
/// **Do not reach for `gpui::swap_rgba_pa_to_bgra` here.** It is the
/// obvious-looking helper and the wrong tool: its guard is `a > 0` rather than
/// `a == 255`, so an opaque pixel still pays three float divides by 1.0 — about
/// 6M pointless divides on an A4 page at 2x.
///
/// The corollary is that the opacity request in `view` is load-bearing for
/// **correctness**, not just speed. If a page ever arrived with partial alpha, a
/// bare swap would render premultiplied values as straight and the page would
/// look subtly wrong rather than failing loudly. If that guarantee is ever
/// relaxed, this function must change with it.
fn swap_rgba_to_bgra(buffer: &mut [u8]) {
    for px in buffer.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_decode_swaps_channels_and_keeps_alpha() {
        // One opaque pixel: R=1, G=2, B=3, A=255.
        let payload = vec![1, 2, 3, 255];
        let image = decode_page(&payload, 1, 1, Encoding::Raw, 2.0).unwrap();
        assert_eq!(image.size(0).width.0, 1);

        // Swapping twice is the identity, which is the property that keeps the
        // conversion honest.
        let mut roundtrip = payload.clone();
        swap_rgba_to_bgra(&mut roundtrip);
        assert_eq!(roundtrip, vec![3, 2, 1, 255]);
        swap_rgba_to_bgra(&mut roundtrip);
        assert_eq!(roundtrip, payload);
    }

    #[test]
    fn raw_decode_rejects_a_payload_that_contradicts_the_dimensions() {
        assert!(decode_page(&[0; 4], 2, 2, Encoding::Raw, 1.0).is_err());
    }

    #[test]
    fn png_decode_round_trips() {
        let mut png = Vec::new();
        let source = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(2, 2, vec![9; 16]).unwrap();
        image::DynamicImage::ImageRgba8(source)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();

        let image = decode_page(&png, 2, 2, Encoding::Png, 1.0).unwrap();
        assert_eq!(image.size(0).width.0, 2);
    }

    #[test]
    fn png_decode_rejects_dimension_mismatch() {
        let mut png = Vec::new();
        let source = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(2, 2, vec![9; 16]).unwrap();
        image::DynamicImage::ImageRgba8(source)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();

        assert!(decode_page(&png, 4, 4, Encoding::Png, 1.0).is_err());
    }
}
