//! Images to insert in place of a placeholder: format and pixel size,
//! read with img-parts, display size and the `<w:drawing>` snippet.

use std::{
    fmt::{self, Debug},
    num::{NonZeroU32, NonZeroU128},
};

use bytes::Bytes;
use img_parts::DynImage;
use thiserror::Error;

use crate::{length::Length, xml::escape_attr};

// ── Limits ───────────────────────────────────────────────────────────────────

/// Largest width or height a drawing can have, in EMU: the `maxInclusive`
/// of `ST_PositiveCoordinate`, the type of `wp:extent` and `a:ext`
/// (ISO/IEC 29500-4:2016, `dml-main.xsd`). That is 2^31 - 1 points, about
/// 757 km.
const MAX_EXTENT_EMU: u64 = 27_273_042_316_900;

// ── Errors ───────────────────────────────────────────────────────────────────

/// Error returned when [`Image::from_bytes`] can't read an image's format
/// or pixel size.
///
/// The enum may grow new rejection reasons: match with a wildcard arm.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum InvalidImage {
    /// The data has no PNG, JPEG or WebP signature, as with GIF files and
    /// data too short to hold one, or it is a WebP file whose chunks are
    /// intact: WebP files are not supported either.
    #[error("unsupported image format")]
    UnsupportedFormat,
    /// The data has the signature of a PNG, JPEG or WebP file, but its
    /// chunks or segments are truncated or corrupt, or it is a PNG or JPEG
    /// file that gives no pixel size or a zero width or height.
    #[error("malformed image")]
    Malformed,
}

// ── Formats ──────────────────────────────────────────────────────────────────

/// An image format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    /// Portable Network Graphics.
    Png,
    /// JPEG, whether JFIF, Exif or bare.
    Jpeg,
}

impl Format {
    /// Return the file extension for the media part, without the dot.
    const fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            // Word names its JPEG media parts `imageN.jpeg`.
            Self::Jpeg => "jpeg",
        }
    }

    /// Return the media type for the `[Content_Types].xml` default.
    const fn content_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
        }
    }
}

/// Return the format and the pixel size, width first, of the image file
/// `data`, read with img-parts: from the `IHDR` chunk of a PNG file, or
/// from the first frame header of a JPEG file.
///
/// # Errors
///
/// Returns [`InvalidImage::Malformed`] when img-parts recognizes `data` but
/// can't read its structure, or reads no size, or a zero one, from a PNG
/// or JPEG file, and [`InvalidImage::UnsupportedFormat`] when it doesn't
/// recognize `data` or reads a WebP file.
fn identify(data: Bytes) -> Result<(Format, NonZeroU32, NonZeroU32), InvalidImage> {
    let (format, image) = match DynImage::from_bytes(data) {
        Ok(Some(image @ DynImage::Png(_))) => (Format::Png, image),
        Ok(Some(image @ DynImage::Jpeg(_))) => (Format::Jpeg, image),
        // Older versions of Word can't show WebP pictures.
        Ok(Some(DynImage::WebP(_)) | None) => return Err(InvalidImage::UnsupportedFormat),
        Err(_) => return Err(InvalidImage::Malformed),
    };
    let (width, height) = image.dimensions().ok_or(InvalidImage::Malformed)?;
    let width = NonZeroU32::new(width).ok_or(InvalidImage::Malformed)?;
    let height = NonZeroU32::new(height).ok_or(InvalidImage::Malformed)?;
    Ok((format, width, height))
}

// ── Image ────────────────────────────────────────────────────────────────────

/// An image to insert in place of a placeholder, as an inline picture.
///
/// [`from_bytes`](Self::from_bytes) accepts PNG and JPEG files, which it
/// reads with [img-parts](https://crates.io/crates/img-parts) for their
/// pixel size; the file itself is embedded unchanged. GIF and WebP files
/// are not supported: convert them to PNG first (older versions of Word
/// can't show WebP pictures).
///
/// The display size is computed as follows:
///
/// - by default, the pixel size at 96 DPI (9,525 EMU per pixel), whatever
///   resolution the file declares;
/// - with only [`width`](Self::width) set, the height keeps the image's
///   aspect ratio; with only [`height`](Self::height) set, the width does;
/// - with both set, the image is stretched to exactly that size.
///
/// A computed dimension is rounded to the nearest EMU. Each dimension is
/// capped at the largest size a drawing can have, 27,273,042,316,900 EMU
/// (about 757 km).
///
/// ```no_run
/// use ciclostile::{Image, Length};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let logo = Image::from_bytes(std::fs::read("logo.png")?)?
///     .width(Length::from_cm(4.0))
///     .alt_text("Company logo");
/// # Ok(())
/// # }
/// ```
#[derive(Clone, PartialEq, Eq)]
pub struct Image {
    /// The image file, embedded unchanged; clones share it.
    data: Bytes,
    /// The format detected from the signature.
    format: Format,
    /// Width in pixels, from the header.
    pixel_width: NonZeroU32,
    /// Height in pixels, from the header.
    pixel_height: NonZeroU32,
    /// Display width, if set.
    width: Option<Length>,
    /// Display height, if set.
    height: Option<Length>,
    /// Alternative text, if set; never empty.
    alt_text: Option<String>,
}

impl Image {
    /// Read a PNG or JPEG file, recognized from its content.
    ///
    /// `data` can be anything that converts into [`Bytes`] without a copy,
    /// such as the `Vec<u8>` that [`std::fs::read`] returns or a
    /// `&'static [u8]`. The file's structure is checked: the chunks of a
    /// PNG file and their CRC-32s, the segments of a JPEG file up to its
    /// image data. The pixel size comes from the PNG header, or from the
    /// first JPEG frame header. The Exif orientation of JPEG photos is
    /// ignored: width and height are those of the stored pixels, so a photo
    /// that relies on Exif rotation may display sideways.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidImage::Malformed`] when `data` has the signature of
    /// a PNG, JPEG or WebP file but is truncated or corrupt, or is a PNG or
    /// JPEG file that gives no pixel size or a zero width or height, and
    /// [`InvalidImage::UnsupportedFormat`] when it is any other file that
    /// is not a PNG or JPEG file, such as a GIF file or an intact WebP file.
    pub fn from_bytes(data: impl Into<Bytes>) -> Result<Self, InvalidImage> {
        let data = data.into();
        let (format, pixel_width, pixel_height) = identify(data.clone())?;
        Ok(Self {
            data,
            format,
            pixel_width,
            pixel_height,
            width: None,
            height: None,
            alt_text: None,
        })
    }

    /// Set the display width.
    ///
    /// Unless [`height`](Self::height) is set too, the height follows the
    /// image's aspect ratio.
    ///
    /// Default: the pixel width at 96 DPI
    #[must_use]
    pub fn width(mut self, width: Length) -> Self {
        self.width = Some(width);
        self
    }

    /// Set the display height.
    ///
    /// Unless [`width`](Self::width) is set too, the width follows the
    /// image's aspect ratio.
    ///
    /// Default: the pixel height at 96 DPI
    #[must_use]
    pub fn height(mut self, height: Length) -> Self {
        self.height = Some(height);
        self
    }

    /// Set the alternative text, which screen readers announce in place of
    /// the image (Word's "Alt Text").
    ///
    /// Characters that XML can't represent (most control characters) are
    /// dropped. An empty text is the same as none.
    ///
    /// Default: no alternative text
    #[must_use]
    pub fn alt_text(mut self, text: impl Into<String>) -> Self {
        let text = text.into();
        self.alt_text = (!text.is_empty()).then_some(text);
        self
    }

    /// Return the image file, for the media part.
    pub(crate) const fn data(&self) -> &Bytes {
        &self.data
    }

    /// Return the media part's file extension: `png` or `jpeg`.
    pub(crate) const fn extension(&self) -> &'static str {
        self.format.extension()
    }

    /// Return the media type that `[Content_Types].xml` must map
    /// [`extension`](Self::extension) to.
    pub(crate) const fn content_type(&self) -> &'static str {
        self.format.content_type()
    }

    /// Return the display size in EMU, width first, as written to the
    /// drawing (see the type-level docs for the rules).
    pub(crate) fn size_emu(&self) -> (u64, u64) {
        let emu = |length: Length| u128::from(length.as_emu());
        let (width, height) = match (self.width, self.height) {
            (Some(width), Some(height)) => (emu(width), emu(height)),
            (Some(width), None) => (
                emu(width),
                scale(emu(width), self.pixel_height, self.pixel_width),
            ),
            (None, Some(height)) => (
                scale(emu(height), self.pixel_width, self.pixel_height),
                emu(height),
            ),
            (None, None) => (
                emu(Length::from_px(self.pixel_width.get())),
                emu(Length::from_px(self.pixel_height.get())),
            ),
        };
        (cap_extent(width), cap_extent(height))
    }

    /// Return the `<w:drawing>` element that shows this image inline.
    ///
    /// `rel_id` is the id of the image relationship from the story part to
    /// the media part, and `doc_pr_id` the drawing's `wp:docPr` id, which
    /// must be unique in the whole document. The element declares the five
    /// namespaces it uses itself, so it can go into any run whatever
    /// prefixes the document uses. It is modeled on what Word writes for
    /// an inline picture, without whitespace between elements.
    pub(crate) fn drawing_xml(&self, rel_id: &str, doc_pr_id: u32) -> String {
        let (cx, cy) = self.size_emu();
        let descr = match &self.alt_text {
            Some(text) => format!(" descr=\"{}\"", escape_attr(text)),
            None => String::new(),
        };
        format!(
            concat!(
                "<w:drawing",
                " xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"",
                " xmlns:wp=",
                "\"http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing\"",
                " xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\"",
                " xmlns:pic=\"http://schemas.openxmlformats.org/drawingml/2006/picture\"",
                " xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\">",
                "<wp:inline distT=\"0\" distB=\"0\" distL=\"0\" distR=\"0\">",
                "<wp:extent cx=\"{cx}\" cy=\"{cy}\"/>",
                "<wp:effectExtent l=\"0\" t=\"0\" r=\"0\" b=\"0\"/>",
                "<wp:docPr id=\"{id}\" name=\"Picture {id}\"{descr}/>",
                "<wp:cNvGraphicFramePr>",
                "<a:graphicFrameLocks noChangeAspect=\"1\"/>",
                "</wp:cNvGraphicFramePr>",
                "<a:graphic>",
                "<a:graphicData uri=\"http://schemas.openxmlformats.org/drawingml/2006/picture\">",
                "<pic:pic>",
                "<pic:nvPicPr>",
                "<pic:cNvPr id=\"0\" name=\"Picture {id}\"/><pic:cNvPicPr/>",
                "</pic:nvPicPr>",
                "<pic:blipFill>",
                "<a:blip r:embed=\"{rel_id}\"/><a:stretch><a:fillRect/></a:stretch>",
                "</pic:blipFill>",
                "<pic:spPr>",
                "<a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"{cx}\" cy=\"{cy}\"/></a:xfrm>",
                "<a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom>",
                "</pic:spPr>",
                "</pic:pic>",
                "</a:graphicData>",
                "</a:graphic>",
                "</wp:inline>",
                "</w:drawing>",
            ),
            cx = cx,
            cy = cy,
            id = doc_pr_id,
            descr = descr,
            rel_id = escape_attr(rel_id),
        )
    }
}

impl Debug for Image {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The file can be megabytes long: show its length, not its bytes.
        f.debug_struct("Image")
            .field("format", &self.format)
            .field("data", &format_args!("<{} bytes>", self.data.len()))
            .field("pixel_width", &self.pixel_width)
            .field("pixel_height", &self.pixel_height)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("alt_text", &self.alt_text)
            .finish()
    }
}

/// Return `value * numerator / denominator`, rounded to the nearest
/// integer (halves round up).
///
/// `value` comes from a `u64` and `numerator` from a `u32`, so the product
/// stays below 2^96: no overflow.
fn scale(value: u128, numerator: NonZeroU32, denominator: NonZeroU32) -> u128 {
    let denominator = NonZeroU128::from(denominator);
    (value * u128::from(numerator.get()) + denominator.get() / 2) / denominator
}

/// Cap a dimension at [`MAX_EXTENT_EMU`].
fn cap_extent(emu: u128) -> u64 {
    u64::try_from(emu).map_or(MAX_EXTENT_EMU, |emu| emu.min(MAX_EXTENT_EMU))
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::snippet;

    // ── Fixtures ──────────────────────────────────────────────────────────────
    //
    // Complete, valid 3x2 images, generated with Python's zlib/struct and
    // decoded correctly by Pillow, ImageMagick, ffmpeg, gdk-pixbuf and
    // LibreOffice.

    /// A 3x2 8-bit RGB PNG: red, green, blue / white, black, gray.
    const PNG_3X2: &[u8] = include_bytes!("testdata/3x2.png");

    /// A 3x2 baseline grayscale JPEG (JFIF), every pixel 160.
    const JPEG_3X2: &[u8] = include_bytes!("testdata/3x2.jpeg");

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Parse `data`, which must be a valid image.
    fn image(data: &'static [u8]) -> Image {
        Image::from_bytes(data).expect("valid image")
    }

    /// Return the pixel size of `image`, width first.
    fn pixels(image: &Image) -> (u32, u32) {
        (image.pixel_width.get(), image.pixel_height.get())
    }

    /// Return [`PNG_3X2`] with another pixel size: only the size is ours to
    /// test once the file is read.
    fn sized(width: u32, height: u32) -> Image {
        Image {
            pixel_width: NonZeroU32::new(width).unwrap(),
            pixel_height: NonZeroU32::new(height).unwrap(),
            ..image(PNG_3X2)
        }
    }

    /// Return a PNG file holding the chunks `(type, data)`, after the
    /// signature, with correct CRC-32s.
    fn png(chunks: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
        let mut data = b"\x89PNG\r\n\x1a\n".to_vec();
        for (kind, contents) in chunks {
            data.extend_from_slice(&u32::try_from(contents.len()).unwrap().to_be_bytes());
            let start = data.len();
            data.extend_from_slice(*kind);
            data.extend_from_slice(contents);
            let crc = rawzip::crc32(&data[start..]);
            data.extend_from_slice(&crc.to_be_bytes());
        }
        data
    }

    /// Return a 13-byte `IHDR` for an 8-bit RGB image of `width` by `height`.
    fn ihdr(width: u32, height: u32) -> Vec<u8> {
        let mut header = width.to_be_bytes().to_vec();
        header.extend_from_slice(&height.to_be_bytes());
        header.extend_from_slice(&[8, 2, 0, 0, 0]);
        header
    }

    /// Return a JPEG file made of the SOI marker, `segments` and the EOI
    /// marker.
    fn jpeg(segments: &[&[u8]]) -> Vec<u8> {
        let mut data = vec![0xff, 0xd8];
        for segment in segments {
            data.extend_from_slice(segment);
        }
        data.extend_from_slice(&[0xff, 0xd9]);
        data
    }

    /// Return a one-component baseline frame header segment (SOF0).
    fn frame_header(width: u16, height: u16) -> Vec<u8> {
        let mut segment = vec![0xff, 0xc0, 0x00, 0x0b, 0x08];
        segment.extend_from_slice(&height.to_be_bytes());
        segment.extend_from_slice(&width.to_be_bytes());
        segment.extend_from_slice(&[0x01, 0x01, 0x11, 0x00]);
        segment
    }

    /// Return why `data` was rejected.
    fn rejection(data: Vec<u8>) -> InvalidImage {
        match Image::from_bytes(data) {
            Ok(image) => panic!("accepted {image:?}"),
            Err(err) => err,
        }
    }

    // Namespace URIs the drawing declares.
    const W_NS: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
    const WP_NS: &str = "http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing";
    const A_NS: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
    const PIC_NS: &str = "http://schemas.openxmlformats.org/drawingml/2006/picture";
    const R_NS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

    // ── Format and pixel size ─────────────────────────────────────────────────

    #[test]
    fn png_and_jpeg_files_give_their_format_and_size() {
        let png = image(PNG_3X2);
        assert_eq!((png.format, pixels(&png)), (Format::Png, (3, 2)));
        assert_eq!((png.extension(), png.content_type()), ("png", "image/png"));
        let jpeg = image(JPEG_3X2);
        assert_eq!((jpeg.format, pixels(&jpeg)), (Format::Jpeg, (3, 2)));
        assert_eq!(
            (jpeg.extension(), jpeg.content_type()),
            ("jpeg", "image/jpeg")
        );
        // The file is kept as given, and shared with the input.
        assert_eq!(png.data(), PNG_3X2);
        assert_eq!(png.data().as_ptr(), PNG_3X2.as_ptr());
    }

    #[test]
    fn gif_webp_and_other_files_are_unsupported() {
        let gif = b"GIF89a\x03\x00\x02\x00\x80\x00\x00\xff\x00\x00\x00\x00\xff\x3b";
        // A RIFF file of type WEBP with a lossless image chunk.
        let mut webp = b"RIFF\x14\x00\x00\x00WEBPVP8L\x08\x00\x00\x00".to_vec();
        webp.extend_from_slice(&[0x2f, 0x02, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00]);
        for data in [
            gif.to_vec(),
            webp,
            b"BM\x3a\x00\x00\x00".to_vec(),
            b"II*\x00".to_vec(),
            b"<svg/>".to_vec(),
            Vec::new(),
            // Too short to recognize.
            PNG_3X2[..4].to_vec(),
            JPEG_3X2[..3].to_vec(),
        ] {
            assert!(
                matches!(rejection(data.clone()), InvalidImage::UnsupportedFormat),
                "{data:02x?}"
            );
        }
    }

    #[test]
    fn files_without_a_size_are_malformed() {
        let no_ihdr = png(&[(b"IEND", b"")]);
        let short_ihdr = png(&[(b"IHDR", &[0, 0, 0, 3, 0, 0]), (b"IEND", b"")]);
        let no_frame_header = jpeg(&[&[0xff, 0xe0, 0x00, 0x04, 0x4a, 0x46]]);
        let short_frame_header = jpeg(&[&[0xff, 0xc0, 0x00, 0x05, 0x08, 0x00, 0x02]]);
        for data in [no_ihdr, short_ihdr, no_frame_header, short_frame_header] {
            assert!(
                matches!(rejection(data.clone()), InvalidImage::Malformed),
                "{data:02x?}"
            );
        }
    }

    #[test]
    fn zero_width_or_height_is_malformed() {
        for (width, height) in [(0_u16, 2_u16), (3, 0)] {
            let header = ihdr(width.into(), height.into());
            let zero_png = png(&[(b"IHDR", &header), (b"IEND", b"")]);
            assert!(matches!(rejection(zero_png), InvalidImage::Malformed));
            let zero_jpeg = jpeg(&[&frame_header(width, height)]);
            assert!(matches!(rejection(zero_jpeg), InvalidImage::Malformed));
        }
        // The smallest and largest sizes are accepted.
        let tiny = png(&[(b"IHDR", &ihdr(1, 1)), (b"IEND", b"")]);
        assert_eq!(pixels(&Image::from_bytes(tiny).unwrap()), (1, 1));
        let huge = png(&[(b"IHDR", &ihdr(u32::MAX, u32::MAX)), (b"IEND", b"")]);
        assert_eq!(
            pixels(&Image::from_bytes(huge).unwrap()),
            (u32::MAX, u32::MAX)
        );
    }

    #[test]
    fn corrupt_files_are_malformed() {
        // A wrong CRC on the IHDR chunk (bytes 29 to 32), and a truncated
        // segment.
        let mut bad_crc = PNG_3X2.to_vec();
        bad_crc[29] ^= 0xff;
        assert!(matches!(rejection(bad_crc), InvalidImage::Malformed));
        let truncated = JPEG_3X2[..30].to_vec();
        assert!(matches!(rejection(truncated), InvalidImage::Malformed));
        // Unlike an intact WebP file, a damaged one is malformed: this RIFF
        // chunk announces 20 bytes and holds 8.
        let truncated_webp = b"RIFF\x14\x00\x00\x00WEBPVP8L".to_vec();
        assert!(matches!(rejection(truncated_webp), InvalidImage::Malformed));
    }

    #[test]
    fn truncated_images_never_panic() {
        for data in [PNG_3X2, JPEG_3X2] {
            for len in 0..=data.len() {
                if let Ok(image) = Image::from_bytes(data[..len].to_vec()) {
                    assert_eq!(pixels(&image), (3, 2), "length {len}");
                }
            }
        }
    }

    #[test]
    fn corrupted_images_never_panic() {
        for data in [PNG_3X2, JPEG_3X2] {
            for index in 0..data.len() {
                for mask in [0x01, 0x10, 0x80, 0xff] {
                    let mut corrupted = data.to_vec();
                    corrupted[index] ^= mask;
                    if let Ok(image) = Image::from_bytes(corrupted) {
                        let (cx, cy) = image.size_emu();
                        assert!(cx > 0 && cy > 0);
                        snippet::parse(&image.drawing_xml("rId1", 1));
                    }
                }
            }
        }
    }

    // ── Display size ──────────────────────────────────────────────────────────

    #[test]
    fn default_display_size_is_96_dpi() {
        assert_eq!(image(PNG_3X2).size_emu(), (3 * 9_525, 2 * 9_525));
        // A large PNG fits below the extent cap without scaling.
        assert_eq!(
            sized(0x7fff_ffff, 1).size_emu(),
            (0x7fff_ffff * 9_525, 9_525)
        );
        // The largest one doesn't.
        assert_eq!(sized(u32::MAX, 1).size_emu(), (MAX_EXTENT_EMU, 9_525));
    }

    #[test]
    fn width_alone_keeps_the_aspect_ratio() {
        let png = image(PNG_3X2).width(Length::from_cm(3.0));
        assert_eq!(png.size_emu(), (1_080_000, 720_000));
    }

    #[test]
    fn height_alone_keeps_the_aspect_ratio() {
        let png = image(PNG_3X2).height(Length::from_cm(2.0));
        assert_eq!(png.size_emu(), (1_080_000, 720_000));
    }

    #[test]
    fn width_and_height_together_stretch_the_image() {
        let png = image(PNG_3X2)
            .height(Length::from_cm(5.0))
            .width(Length::from_cm(1.0));
        assert_eq!(png.size_emu(), (360_000, 1_800_000));
    }

    #[test]
    fn scaled_dimension_rounds_to_the_nearest_emu() {
        // Height = width * 2/3.
        for (width, height) in [(1, 1), (2, 1), (4, 3), (5, 3), (3, 2), (0, 0)] {
            let png = image(PNG_3X2).width(Length::from_emu(width));
            assert_eq!(png.size_emu(), (width, height), "width {width}");
        }
        // Width = height * 3/2: 1.5 rounds up.
        let png = image(PNG_3X2).height(Length::from_emu(1));
        assert_eq!(png.size_emu(), (2, 1));
    }

    #[test]
    fn extreme_aspect_ratios_neither_overflow_nor_exceed_the_extent_cap() {
        let tall = sized(1, 100_000);
        assert_eq!(tall.size_emu(), (9_525, 952_500_000));
        assert_eq!(
            tall.clone().width(Length::from_emu(1)).size_emu(),
            (1, 100_000)
        );
        // 1/100,000 EMU rounds to nothing.
        assert_eq!(tall.clone().height(Length::from_emu(1)).size_emu(), (0, 1));
        // u64::MAX * 100,000 doesn't fit in u64: both dimensions hit the cap.
        let huge = Length::from_emu(u64::MAX);
        assert_eq!(
            tall.clone().width(huge).size_emu(),
            (MAX_EXTENT_EMU, MAX_EXTENT_EMU)
        );
        // u64::MAX / 100,000 fits, but is still above the cap.
        assert_eq!(
            tall.clone().height(huge).size_emu(),
            (MAX_EXTENT_EMU, MAX_EXTENT_EMU)
        );
        assert_eq!(
            tall.width(huge).height(huge).size_emu(),
            (MAX_EXTENT_EMU, MAX_EXTENT_EMU)
        );

        let wide = sized(0x7fff_ffff, 1);
        assert_eq!(
            wide.clone().height(huge).size_emu(),
            (MAX_EXTENT_EMU, MAX_EXTENT_EMU)
        );
        assert_eq!(
            wide.width(Length::from_emu(MAX_EXTENT_EMU)).size_emu(),
            (MAX_EXTENT_EMU, 12_700)
        );
    }

    // ── Drawing snippet ───────────────────────────────────────────────────────

    #[test]
    fn drawing_declares_its_namespaces_on_the_root() {
        let drawing = snippet::parse(&image(PNG_3X2).drawing_xml("rId1", 1));
        let declared: Vec<(&str, &str)> = drawing
            .declarations
            .iter()
            .map(|(prefix, uri)| (prefix.as_str(), uri.as_str()))
            .collect();
        assert_eq!(
            declared,
            [
                ("w", W_NS),
                ("wp", WP_NS),
                ("a", A_NS),
                ("pic", PIC_NS),
                ("r", R_NS)
            ]
        );
        for (index, element) in drawing.descendants().into_iter().enumerate() {
            let prefix = element
                .name
                .split_once(':')
                .map_or("", |(prefix, _)| prefix);
            let (_, expected) = declared.iter().find(|(p, _)| *p == prefix).unwrap();
            assert_eq!(element.namespace, *expected, "{}", element.name);
            assert!(element.text.is_empty(), "text in {}", element.name);
            if index > 0 {
                assert!(
                    element.declarations.is_empty(),
                    "declarations on {}",
                    element.name
                );
            }
        }
    }

    #[test]
    fn drawing_elements_follow_the_schema_order() {
        let drawing = snippet::parse(&image(PNG_3X2).drawing_xml("rId1", 1));
        assert_eq!(
            drawing.outline(),
            concat!(
                "w:drawing[wp:inline[",
                "wp:extent,wp:effectExtent,wp:docPr,wp:cNvGraphicFramePr[a:graphicFrameLocks],",
                "a:graphic[a:graphicData[pic:pic[",
                "pic:nvPicPr[pic:cNvPr,pic:cNvPicPr],",
                "pic:blipFill[a:blip,a:stretch[a:fillRect]],",
                "pic:spPr[a:xfrm[a:off,a:ext],a:prstGeom[a:avLst]]",
                "]]]]]",
            )
        );
    }

    #[test]
    fn drawing_carries_the_size_ids_and_relationship() {
        let png = image(PNG_3X2).width(Length::from_cm(3.0));
        let xml = png.drawing_xml("rId42", 7);
        assert!(!xml.contains(['\n', '\r', '\t']), "{xml}");
        let drawing = snippet::parse(&xml);
        let inline = drawing.child("wp:inline");
        for name in ["distT", "distB", "distL", "distR"] {
            assert_eq!(inline.attr(name), Some("0"));
        }
        let extent = inline.child("wp:extent");
        assert_eq!(extent.attr("cx"), Some("1080000"));
        assert_eq!(extent.attr("cy"), Some("720000"));
        let doc_pr = inline.child("wp:docPr");
        assert_eq!(
            doc_pr.attributes,
            [
                ("id".to_owned(), "7".to_owned()),
                ("name".to_owned(), "Picture 7".to_owned())
            ]
        );

        let graphic_data = inline.child("a:graphic").child("a:graphicData");
        assert_eq!(graphic_data.attr("uri"), Some(PIC_NS));
        let pic = graphic_data.child("pic:pic");
        let c_nv_pr = pic.child("pic:nvPicPr").child("pic:cNvPr");
        assert_eq!(c_nv_pr.attr("id"), Some("0"));
        assert_eq!(c_nv_pr.attr("name"), Some("Picture 7"));
        let blip = pic.child("pic:blipFill").child("a:blip");
        assert_eq!(blip.attr("r:embed"), Some("rId42"));
        let ext = pic.child("pic:spPr").child("a:xfrm").child("a:ext");
        assert_eq!(ext.attributes, extent.attributes);
    }

    #[test]
    fn drawing_writes_descr_only_with_alt_text() {
        for png in [image(PNG_3X2), image(PNG_3X2).alt_text("")] {
            let drawing = snippet::parse(&png.drawing_xml("rId1", 1));
            let doc_pr = drawing.child("wp:inline").child("wp:docPr");
            assert_eq!(doc_pr.attr("descr"), None);
        }
        let png = image(PNG_3X2).alt_text("A red square");
        let drawing = snippet::parse(&png.drawing_xml("rId1", 1));
        let doc_pr = drawing.child("wp:inline").child("wp:docPr");
        assert_eq!(doc_pr.attr("descr"), Some("A red square"));
    }

    #[test]
    fn alt_text_is_escaped_and_illegal_characters_are_dropped() {
        let alt = "<b> & \"quotes\" 'too'\ttab\nnew line\r\n\u{0}\u{8}\u{b}\u{1f}\u{fffe}end";
        let xml = image(PNG_3X2).alt_text(alt).drawing_xml("rId1", 1);
        assert!(
            !xml.contains(|c: char| {
                matches!(c, '\u{0}'..='\u{8}' | '\u{b}' | '\u{c}' | '\u{e}'..='\u{1f}')
                    || matches!(c, '\u{fffe}' | '\u{ffff}')
            }),
            "{xml:?}"
        );
        let drawing = snippet::parse(&xml);
        let doc_pr = drawing.child("wp:inline").child("wp:docPr");
        // Tabs and line breaks survive attribute-value normalization.
        assert_eq!(
            doc_pr.attr("descr"),
            Some("<b> & \"quotes\" 'too'\ttab\nnew line\r\nend")
        );
    }

    #[test]
    fn relationship_id_is_escaped() {
        let drawing = snippet::parse(&image(PNG_3X2).drawing_xml("r\"<&>'", u32::MAX));
        let inline = drawing.child("wp:inline");
        assert_eq!(inline.child("wp:docPr").attr("id"), Some("4294967295"));
        let blip = inline
            .child("a:graphic")
            .child("a:graphicData")
            .child("pic:pic")
            .child("pic:blipFill")
            .child("a:blip");
        assert_eq!(blip.attr("r:embed"), Some("r\"<&>'"));
    }

    // ── Traits ────────────────────────────────────────────────────────────────

    #[test]
    fn image_debug_shows_the_data_length_not_the_bytes() {
        let debug = format!("{:?}", image(PNG_3X2).alt_text("logo"));
        assert!(debug.contains("data: <78 bytes>"), "{debug}");
        assert!(debug.contains("\"logo\""), "{debug}");
        assert!(!debug.contains("137"), "{debug}");
    }

    #[test]
    fn clones_share_the_file() {
        let png = image(PNG_3X2).width(Length::from_cm(1.0));
        assert_eq!(png.clone().data().as_ptr(), png.data().as_ptr());
    }

    #[test]
    fn errors_have_lowercase_messages() {
        assert_eq!(
            InvalidImage::UnsupportedFormat.to_string(),
            "unsupported image format"
        );
        assert_eq!(InvalidImage::Malformed.to_string(), "malformed image");
    }
}
