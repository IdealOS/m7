//! What a file is, by its name, and what can be read from an image's first bytes.

use protocol::import::Kind;

/// The kind of file `name` is, by its extension.
pub fn kind_of(name: &str) -> Kind {
    match extension(name).as_str() {
        "txt" | "text" | "md" | "markdown" => Kind::Text,
        "png" | "jpg" | "jpeg" | "gif" | "heic" | "heif" | "webp" => Kind::Image,
        "mp3" | "flac" | "m4a" | "aac" | "ogg" | "oga" | "opus" | "wav" | "aiff" | "aif" | "alac" => {
            Kind::Audio
        }
        _ => Kind::Other,
    }
}

/// A text file's format: Markdown for `.md`, plain otherwise.
pub fn text_format(name: &str) -> &'static str {
    if matches!(extension(name).as_str(), "md" | "markdown") { "markdown" } else { "plain" }
}

/// An image's MIME type, by its extension.
pub fn image_type(name: &str) -> Option<&'static str> {
    Some(match extension(name).as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "heic" | "heif" => "image/heic",
        "webp" => "image/webp",
        _ => return None,
    })
}

/// The file's name without its extension: a title.
pub fn stem(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((s, _)) if !s.is_empty() => s,
        _ => name,
    }
}

fn extension(name: &str) -> String {
    name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default()
}

/// How many of an image's first bytes [`image_size`] wants (a JPEG's size can come
/// after its EXIF data and thumbnail).
pub const HEAD_BYTES: usize = 256 * 1024;

/// An image's width and height, from its first bytes: PNG, GIF, JPEG and WebP.
pub fn image_size(head: &[u8]) -> Option<(u32, u32)> {
    let be16 = |i: usize| head.get(i..i + 2).map(|b| u16::from_be_bytes([b[0], b[1]]) as u32);
    let be32 = |i: usize| head.get(i..i + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
    let le16 = |i: usize| head.get(i..i + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as u32);
    let le24 = |i: usize| head.get(i..i + 3).map(|b| b[0] as u32 | (b[1] as u32) << 8 | (b[2] as u32) << 16);
    if head.starts_with(b"\x89PNG\r\n\x1a\n") && head.get(12..16) == Some(b"IHDR") {
        return Some((be32(16)?, be32(20)?));
    }
    if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        return Some((le16(6)?, le16(8)?));
    }
    if head.starts_with(b"RIFF") && head.get(8..12) == Some(b"WEBP") {
        return match head.get(12..16)? {
            b"VP8X" => Some((le24(24)? + 1, le24(27)? + 1)),
            b"VP8 " => Some((le16(26)? & 0x3fff, le16(28)? & 0x3fff)),
            b"VP8L" => {
                let b = head.get(21..25)?;
                let bits = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1))
            }
            _ => None,
        };
    }
    if head.starts_with(&[0xff, 0xd8]) {
        // Walk the segments to a start-of-frame marker, which holds the size.
        let mut i = 2;
        while i + 4 <= head.len() {
            if head[i] != 0xff {
                return None;
            }
            let marker = head[i + 1];
            if marker == 0xff {
                i += 1; // padding
                continue;
            }
            let len = be16(i + 2)? as usize;
            let sof = matches!(marker, 0xc0..=0xcf) && !matches!(marker, 0xc4 | 0xc8 | 0xcc);
            if sof {
                return Some((be16(i + 7)?, be16(i + 5)?));
            }
            i += 2 + len;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_are_known_by_their_extension() {
        assert_eq!(kind_of("Notes.MD"), Kind::Text);
        assert_eq!(kind_of("a.txt"), Kind::Text);
        assert_eq!(kind_of("IMG_0001.JPG"), Kind::Image);
        assert_eq!(kind_of("song.flac"), Kind::Audio);
        assert_eq!(kind_of("archive.zip"), Kind::Other);
        assert_eq!(kind_of("README"), Kind::Other);
        assert_eq!(text_format("x.markdown"), "markdown");
        assert_eq!(text_format("x.txt"), "plain");
        assert_eq!(image_type("x.jpeg"), Some("image/jpeg"));
        assert_eq!(stem("holiday.photo.png"), "holiday.photo");
        assert_eq!(stem(".hidden"), ".hidden");
    }

    #[test]
    fn image_sizes_come_from_their_headers() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend(640u32.to_be_bytes());
        png.extend(480u32.to_be_bytes());
        assert_eq!(image_size(&png), Some((640, 480)));
        assert_eq!(image_size(b"GIF89a\x20\x03\x58\x02"), Some((800, 600)));
        // A JPEG: SOI, an APP0 segment, then SOF0 (precision, height 300, width 400).
        let jpeg =
            [0xff, 0xd8, 0xff, 0xe0, 0, 4, 0, 0, 0xff, 0xc0, 0, 11, 8, 0x01, 0x2c, 0x01, 0x90, 3, 0, 0, 0];
        assert_eq!(image_size(&jpeg), Some((400, 300)));
        let mut webp = b"RIFF\0\0\0\0WEBPVP8X\0\0\0\0\0\0\0\0".to_vec();
        webp.extend([0x1f, 0x03, 0x00, 0x57, 0x02, 0x00]); // 800 × 600, less one each
        assert_eq!(image_size(&webp), Some((800, 600)));
        assert_eq!(image_size(b"not an image"), None);
        assert_eq!(image_size(&[0xff, 0xd8, 0xff]), None, "cut short");
    }
}
