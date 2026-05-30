use std::io::Cursor;
use std::path::Path;

use image::imageops::{flip_horizontal, flip_vertical, rotate180, rotate270, rotate90};
use image::{DynamicImage, ImageReader, RgbImage, RgbaImage};

/// Decode any supported image file to an RGBA8 buffer (call `.dimensions()` for size).
/// Uses magic-byte sniffing (not the extension).
pub fn decode_to_rgba8(path: &Path) -> image::ImageResult<RgbaImage> {
    Ok(ImageReader::open(path)?
        .with_guessed_format()?
        .decode()?
        .to_rgba8())
}

/// Read once, decode, normalize EXIF orientation, downscale so both sides <= `max`.
/// Defers RGBA expansion past the downscale for the no-alpha (RGB/JPEG) case so no
/// full native-resolution RGBA buffer is allocated when downscaling.
pub fn display_image(path: &Path, max: u32) -> image::ImageResult<RgbaImage> {
    let bytes = std::fs::read(path)?;
    let orientation = orientation_from_bytes(&bytes).unwrap_or(1);
    let dynimg = ImageReader::new(Cursor::new(bytes.as_slice()))
        .with_guessed_format()?
        .decode()?;
    Ok(match dynimg {
        DynamicImage::ImageRgb8(rgb) => {
            let small = downscale_rgb_to_fit(apply_orientation(rgb, orientation), max);
            DynamicImage::ImageRgb8(small).into_rgba8()
        }
        other => downscale_to_fit(apply_orientation(other.into_rgba8(), orientation), max),
    })
}

/// EXIF Orientation (1..=8) parsed from in-memory image bytes, if present.
fn orientation_from_bytes(bytes: &[u8]) -> Option<u16> {
    let exif = exif::Reader::new()
        .read_from_container(&mut Cursor::new(bytes))
        .ok()?;
    let field = exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY)?;
    match field.value.get_uint(0) {
        Some(v @ 1..=8) => Some(v as u16),
        _ => None,
    }
}

/// Header-only natural display dimensions (post-EXIF-orientation), without decoding
/// pixels. Used to size the window before the first decode. Returns None on error.
/// NOTE: `image::image_dimensions` guesses format by file EXTENSION (unlike the
/// magic-byte decode path), so a missing/wrong extension yields None — the caller
/// falls back to the default window size.
pub fn display_dimensions(path: &Path) -> Option<(u32, u32)> {
    let (w, h) = image::image_dimensions(path).ok()?;
    match read_orientation(path) {
        Some(5 | 6 | 7 | 8) => Some((h, w)), // 90°/270° transpose → swap W/H
        _ => Some((w, h)),
    }
}

/// Read the EXIF Orientation tag (1..=8) from a file path, if present.
pub fn read_orientation(path: &Path) -> Option<u16> {
    let bytes = std::fs::read(path).ok()?;
    orientation_from_bytes(&bytes)
}

/// Apply an EXIF orientation (1..=8) by transforming the pixel buffer. Generic over the
/// pixel type so it serves both the RGB (deferred) and RGBA paths. Mirrored variants
/// (2,4,5,7) compose a flip with a rotation.
pub fn apply_orientation<P>(
    img: image::ImageBuffer<P, Vec<P::Subpixel>>,
    orientation: u16,
) -> image::ImageBuffer<P, Vec<P::Subpixel>>
where
    P: image::Pixel<Subpixel = u8> + 'static,
{
    match orientation {
        2 => flip_horizontal(&img),
        3 => rotate180(&img),
        4 => flip_vertical(&img),
        5 => rotate270(&flip_horizontal(&img)), // transpose (mirror-H + rotate 270 CW)
        6 => rotate90(&img),                    // rotate 90 CW
        7 => rotate90(&flip_horizontal(&img)),  // transverse (mirror-H + rotate 90 CW)
        8 => rotate270(&img),                   // rotate 270 CW
        _ => img,                               // 1 or unknown: as-is
    }
}

/// Target dims that fit `(w, h)` within `max` preserving aspect, or None if already within.
fn fit_dims(w: u32, h: u32, max: u32) -> Option<(u32, u32)> {
    if w <= max && h <= max {
        return None;
    }
    let scale = (max as f32 / w as f32).min(max as f32 / h as f32);
    Some((
        ((w as f32 * scale).round() as u32).max(1),
        ((h as f32 * scale).round() as u32).max(1),
    ))
}

/// Shrink an RGBA image so both sides are <= `max` (no upscaling). SIMD Lanczos3.
pub fn downscale_to_fit(img: RgbaImage, max: u32) -> RgbaImage {
    let (w, h) = img.dimensions();
    match fit_dims(w, h, max) {
        None => img,
        Some((nw, nh)) => resize_u8x4(img, nw, nh),
    }
}

/// Shrink an RGB image so both sides are <= `max` (no upscaling). SIMD Lanczos3.
fn downscale_rgb_to_fit(img: RgbImage, max: u32) -> RgbImage {
    let (w, h) = img.dimensions();
    match fit_dims(w, h, max) {
        None => img,
        Some((nw, nh)) => resize_u8x3(img, nw, nh),
    }
}

/// SIMD RGBA8 downscale via `fast_image_resize` (Lanczos3). The `expect`s are
/// unreachable for a well-formed `RgbaImage`: its buffer is exactly `w*h*4`, the
/// pixel types match (U8x4), and the target dims are clamped to >= 1 by the caller.
fn resize_u8x4(img: RgbaImage, nw: u32, nh: u32) -> RgbaImage {
    use fast_image_resize::images::Image as FirImage;
    use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};

    let (w, h) = img.dimensions();
    let src = FirImage::from_vec_u8(w, h, img.into_raw(), PixelType::U8x4)
        .expect("RgbaImage backing buffer is exactly w*h*4 bytes");
    let mut dst = FirImage::new(nw, nh, PixelType::U8x4);
    Resizer::new()
        .resize(
            &src,
            &mut dst,
            &ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3)),
        )
        .expect("U8x4 -> U8x4 convolution with nonzero dimensions");
    RgbaImage::from_raw(nw, nh, dst.into_vec()).expect("resized buffer is exactly nw*nh*4 bytes")
}

/// SIMD RGB8 downscale via `fast_image_resize` (Lanczos3). The `expect`s are
/// unreachable for a well-formed `RgbImage`: its buffer is exactly `w*h*3`, the
/// pixel types match (U8x3), and the target dims are clamped to >= 1 by the caller.
fn resize_u8x3(img: RgbImage, nw: u32, nh: u32) -> RgbImage {
    use fast_image_resize::images::Image as FirImage;
    use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};

    let (w, h) = img.dimensions();
    let src = FirImage::from_vec_u8(w, h, img.into_raw(), PixelType::U8x3)
        .expect("RgbImage backing buffer is exactly w*h*3 bytes");
    let mut dst = FirImage::new(nw, nh, PixelType::U8x3);
    Resizer::new()
        .resize(
            &src,
            &mut dst,
            &ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3)),
        )
        .expect("U8x3 -> U8x3 convolution with nonzero dimensions");
    RgbImage::from_raw(nw, nh, dst.into_vec()).expect("resized buffer is exactly nw*nh*3 bytes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};

    fn two_px() -> RgbaImage {
        // width 2, height 1: left RED, right BLUE
        let mut img = RgbaImage::new(2, 1);
        img.put_pixel(0, 0, Rgba([255, 0, 0, 255]));
        img.put_pixel(1, 0, Rgba([0, 0, 255, 255]));
        img
    }

    #[test]
    fn orientation_1_is_identity() {
        let out = apply_orientation(two_px(), 1);
        assert_eq!(out.dimensions(), (2, 1));
        assert_eq!(out.get_pixel(0, 0), &Rgba([255, 0, 0, 255]));
        assert_eq!(out.get_pixel(1, 0), &Rgba([0, 0, 255, 255]));
    }

    #[test]
    fn orientation_2_mirrors_horizontally() {
        let out = apply_orientation(two_px(), 2);
        assert_eq!(out.dimensions(), (2, 1));
        assert_eq!(out.get_pixel(0, 0), &Rgba([0, 0, 255, 255]));
        assert_eq!(out.get_pixel(1, 0), &Rgba([255, 0, 0, 255]));
    }

    #[test]
    fn orientation_3_rotates_180() {
        let out = apply_orientation(two_px(), 3);
        assert_eq!(out.dimensions(), (2, 1));
        assert_eq!(out.get_pixel(0, 0), &Rgba([0, 0, 255, 255]));
        assert_eq!(out.get_pixel(1, 0), &Rgba([255, 0, 0, 255]));
    }

    #[test]
    fn rotating_orientations_swap_dimensions() {
        for o in [5u16, 6, 7, 8] {
            let out = apply_orientation(two_px(), o);
            assert_eq!(out.dimensions(), (1, 2), "orientation {o} should swap W/H");
        }
    }

    #[test]
    fn decode_roundtrip_via_temp_png() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.png");
        let mut src = RgbaImage::new(3, 2);
        src.put_pixel(0, 0, Rgba([10, 20, 30, 255]));
        src.save(&path).unwrap();

        let rgba = decode_to_rgba8(&path).unwrap();
        assert_eq!(rgba.dimensions(), (3, 2));
        assert_eq!(rgba.get_pixel(0, 0), &Rgba([10, 20, 30, 255]));
    }

    #[test]
    fn read_orientation_is_none_without_exif() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.png");
        RgbaImage::new(2, 2).save(&path).unwrap();
        assert_eq!(read_orientation(&path), None);
    }

    #[test]
    fn downscale_fits_within_max_and_leaves_small_alone() {
        let big = RgbaImage::new(100, 50);
        let out = downscale_to_fit(big, 40);
        assert!(out.width() <= 40 && out.height() <= 40);
        assert_eq!(out.width(), 40);

        let small = RgbaImage::new(30, 20);
        let out2 = downscale_to_fit(small, 40);
        assert_eq!(out2.dimensions(), (30, 20));
    }

    #[test]
    fn display_image_decodes_and_downscales() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.png");
        image::RgbaImage::new(100, 50).save(&path).unwrap();
        let out = display_image(&path, 40).unwrap();
        assert!(out.width() <= 40 && out.height() <= 40);
        assert_eq!(out.width(), 40); // limiting side hits max
    }

    #[test]
    fn display_dimensions_reads_header_without_exif_swap_for_orientation_1() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.png");
        image::RgbaImage::new(40, 20).save(&path).unwrap(); // no EXIF → orientation 1
        assert_eq!(display_dimensions(&path), Some((40, 20)));
    }

    #[test]
    fn display_dimensions_is_none_for_missing_file() {
        assert_eq!(display_dimensions(std::path::Path::new("/no/such/file.jpg")), None);
    }

    #[test]
    fn display_image_rgb_path_matches_naive_and_fits_cap() {
        use image::{Rgb, RgbImage};
        // An RGB (no-alpha) PNG decodes to DynamicImage::ImageRgb8 → exercises the deferred path.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rgb.png");
        let mut src = RgbImage::new(100, 50);
        for (x, _y, px) in src.enumerate_pixels_mut() {
            *px = Rgb([(x % 256) as u8, 20, 200]); // some horizontal gradient
        }
        src.save(&path).unwrap();

        // Reference: the old naive path (decode → rgba8 → downscale).
        let naive = {
            let dynimg = image::ImageReader::open(&path).unwrap().decode().unwrap();
            downscale_to_fit(dynimg.to_rgba8(), 40)
        };
        let got = display_image(&path, 40).unwrap();

        assert!(got.width() <= 40 && got.height() <= 40);
        assert_eq!(got.dimensions(), naive.dimensions());
        // Per-channel equality within rounding tolerance (Lanczos on RGB vs RGBA).
        for (a, b) in got.pixels().zip(naive.pixels()) {
            for c in 0..4 {
                assert!((a.0[c] as i16 - b.0[c] as i16).abs() <= 1, "channel {c} differs");
            }
        }
        // Opaque alpha preserved.
        assert_eq!(got.get_pixel(0, 0).0[3], 255);
    }
}
