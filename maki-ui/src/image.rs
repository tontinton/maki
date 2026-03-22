use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine;
use image::{ImageBuffer, RgbaImage};
use maki_agent::{ImageMediaType, ImageSource};

const MAX_IMAGE_PIXELS: usize = 8_000_000;
const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

const IMAGE_EXTENSIONS: &[(&str, ImageMediaType)] = &[
    ("png", ImageMediaType::Png),
    ("jpg", ImageMediaType::Jpeg),
    ("jpeg", ImageMediaType::Jpeg),
    ("gif", ImageMediaType::Gif),
    ("webp", ImageMediaType::Webp),
];

pub(crate) fn try_parse_image_path(text: &str) -> Option<(PathBuf, ImageMediaType)> {
    let trimmed = text.trim().trim_matches('\'');
    let (path_str, was_file_uri) = match trimmed.strip_prefix("file://") {
        Some(rest) => (rest.replace("\\ ", " "), true),
        None => (trimmed.replace("\\ ", " "), false),
    };
    if path_str.contains("://") {
        return None;
    }
    let is_absolute =
        path_str.starts_with('/') || (cfg!(windows) && path_str.get(1..3) == Some(":\\"));
    if !was_file_uri && !is_absolute && !path_str.starts_with("~/") {
        return None;
    }
    let path = if let Some(rest) = path_str.strip_prefix("~/") {
        dirs::home_dir()?.join(rest)
    } else {
        PathBuf::from(&path_str)
    };
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let media_type = IMAGE_EXTENSIONS
        .iter()
        .find(|(e, _)| *e == ext)
        .map(|(_, mt)| *mt)?;
    Some((path, media_type))
}

pub(crate) fn load_file_image(
    path: &Path,
    media_type: ImageMediaType,
) -> Result<ImageSource, String> {
    let bytes = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err("Image file exceeds 20MB limit".into());
    }
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(ImageSource::new(media_type, Arc::from(b64)))
}

pub(crate) fn load_clipboard_image() -> Result<ImageSource, String> {
    let mut cb = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    let img = cb.get_image().map_err(|e| e.to_string())?;
    let pixels = img
        .width
        .checked_mul(img.height)
        .ok_or_else(|| format!("Image dimensions overflow ({}x{})", img.width, img.height))?;
    if pixels > MAX_IMAGE_PIXELS {
        return Err(format!("Image too large ({}x{})", img.width, img.height));
    }
    let png_bytes = encode_rgba_to_png(img.width as u32, img.height as u32, &img.bytes)?;
    if png_bytes.len() > MAX_IMAGE_BYTES {
        return Err("Encoded image exceeds 20MB limit".into());
    }
    let b64 = base64::engine::general_purpose::STANDARD.encode(&png_bytes);
    Ok(ImageSource::new(ImageMediaType::Png, Arc::from(b64)))
}

fn encode_rgba_to_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    let img: RgbaImage =
        ImageBuffer::from_raw(width, height, rgba.to_vec()).ok_or("Invalid image dimensions")?;
    let mut buf = Vec::new();
    img.write_to(&mut io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case("file:///home/user/photo.png",      "/home/user/photo.png",  ImageMediaType::Png  ; "file_uri_png")]
    #[test_case("file:///home/user/photo.jpg",      "/home/user/photo.jpg",  ImageMediaType::Jpeg ; "file_uri_jpg")]
    #[test_case("file:///home/user/photo.jpeg",     "/home/user/photo.jpeg", ImageMediaType::Jpeg ; "file_uri_jpeg")]
    #[test_case("file:///home/user/photo.gif",       "/home/user/photo.gif",  ImageMediaType::Gif  ; "file_uri_gif")]
    #[test_case("file:///home/user/photo.webp",      "/home/user/photo.webp", ImageMediaType::Webp ; "file_uri_webp")]
    #[test_case("/home/user/photo.png",              "/home/user/photo.png",  ImageMediaType::Png  ; "absolute_path")]
    #[test_case("  /home/user/photo.jpg\n",          "/home/user/photo.jpg",  ImageMediaType::Jpeg ; "trimmed_whitespace")]
    #[test_case("'/home/user/photo.png'",            "/home/user/photo.png",  ImageMediaType::Png  ; "single_quoted")]
    #[test_case("/home/user/my\\ photo.png",         "/home/user/my photo.png", ImageMediaType::Png ; "escaped_space")]
    fn try_parse_image_path_valid(
        input: &str,
        expected_path: &str,
        expected_media: ImageMediaType,
    ) {
        let (path, media) = try_parse_image_path(input).expect("should parse");
        assert_eq!(path.to_str().unwrap(), expected_path);
        assert_eq!(media, expected_media);
    }

    #[test]
    fn try_parse_image_path_tilde() {
        let (path, media) = try_parse_image_path("~/Pictures/photo.jpg").expect("should parse");
        let home = dirs::home_dir().unwrap();
        assert_eq!(path, home.join("Pictures/photo.jpg"));
        assert_eq!(media, ImageMediaType::Jpeg);
    }

    #[test_case("hello world"            ; "plain_text")]
    #[test_case("/home/user/readme.txt"  ; "non_image_ext")]
    #[test_case("/home/user/noext"       ; "no_extension")]
    #[test_case("https://example.com/a.png" ; "https_url")]
    #[test_case("relative.png"           ; "relative_path")]
    #[test_case("look at /home/user/photo.png" ; "embedded_path")]
    fn try_parse_image_path_none(input: &str) {
        assert!(try_parse_image_path(input).is_none());
    }

    #[test]
    fn load_file_image_nonexistent() {
        let err = load_file_image(Path::new("/nonexistent/image.png"), ImageMediaType::Png);
        assert!(err.is_err());
    }

    #[test]
    fn load_file_image_valid() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("test.png");
        fs::write(&path, b"fake png data").unwrap();
        let result = load_file_image(&path, ImageMediaType::Png);
        assert!(result.is_ok());
        let source = result.unwrap();
        assert_eq!(source.media_type, ImageMediaType::Png);
        assert!(!source.data.is_empty());
    }
}
