use std::io::Cursor;
use std::path::Path;

use image::imageops::{
    flip_horizontal, flip_vertical, resize, rotate180, rotate270, rotate90, FilterType,
};
use image::{ImageReader, RgbaImage};

/// Decode any supported image file to an RGBA8 buffer (call `.dimensions()` for size).
/// Uses magic-byte sniffing (not the extension).
pub fn decode_to_rgba8(path: &Path) -> image::ImageResult<RgbaImage> {
    Ok(ImageReader::open(path)?.with_guessed_format()?.decode()?.to_rgba8())
}

/// Read the file once, decode to RGBA8, normalize EXIF orientation, and downscale
/// so both sides are <= `max`. A single filesystem read serves BOTH the pixel
/// decode and the EXIF orientation lookup (avoids opening the file twice).
pub fn display_image(path: &Path, max: u32) -> image::ImageResult<RgbaImage> {
    let bytes = std::fs::read(path)?;
    let orientation = orientation_from_bytes(&bytes).unwrap_or(1);
    let rgba = ImageReader::new(Cursor::new(bytes.as_slice()))
        .with_guessed_format()?
        .decode()?
        .to_rgba8();
    Ok(downscale_to_fit(apply_orientation(rgba, orientation), max))
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

/// Read the EXIF Orientation tag (1..=8) from a file path, if present.
pub fn read_orientation(path: &Path) -> Option<u16> {
    let bytes = std::fs::read(path).ok()?;
    orientation_from_bytes(&bytes)
}

/// Apply an EXIF orientation (1..=8) by transforming the pixel buffer.
/// Handles the mirrored variants, which a single rotation angle cannot.
pub fn apply_orientation(img: RgbaImage, orientation: u16) -> RgbaImage {
    match orientation {
        2 => flip_horizontal(&img),
        3 => rotate180(&img),
        4 => flip_vertical(&img),
        5 => rotate270(&flip_horizontal(&img)), // transpose (mirror-H + rotate 270 CW)
        6 => rotate90(&img),                     // rotate 90 CW
        7 => rotate90(&flip_horizontal(&img)),  // transverse (mirror-H + rotate 90 CW)
        8 => rotate270(&img),                    // rotate 270 CW
        _ => img,                                // 1 or unknown: as-is
    }
}

/// Shrink so both sides are <= `max`, preserving aspect ratio. Images already
/// within bounds are returned unchanged (no upscaling).
pub fn downscale_to_fit(img: RgbaImage, max: u32) -> RgbaImage {
    let (w, h) = img.dimensions();
    if w <= max && h <= max {
        return img;
    }
    let scale = (max as f32 / w as f32).min(max as f32 / h as f32);
    let nw = ((w as f32 * scale).round() as u32).max(1);
    let nh = ((h as f32 * scale).round() as u32).max(1);
    resize(&img, nw, nh, FilterType::Triangle)
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
}
