//! Materialize images selected on this desktop for a paired host.
use base64::{Engine, engine::general_purpose::STANDARD};
use ox_desk_core::mentions::ImageInput;
use std::{fs::OpenOptions, io::Read, os::unix::fs::OpenOptionsExt, path::PathBuf, sync::Arc};

const MAX_BYTES: usize = 6 * 1024 * 1024;
const LIMIT: &str = "Choose up to four images totalling 6 MiB or less.";

pub(crate) fn check_limits(count: usize, bytes: usize) -> Result<(), String> {
    if count > 4 || bytes > MAX_BYTES {
        Err(LIMIT.into())
    } else {
        Ok(())
    }
}

pub(crate) fn encode(mime: &str, bytes: &[u8]) -> ImageInput {
    ImageInput::Clipboard(
        mime.into(),
        Arc::from(format!("data:{mime};base64,{}", STANDARD.encode(bytes))),
    )
}

pub(crate) fn read_images(paths: &[PathBuf]) -> Result<Vec<ImageInput>, String> {
    check_limits(paths.len(), 0)?;
    let mut images = Vec::with_capacity(paths.len());
    let mut remaining = MAX_BYTES;
    for path in paths {
        let extension = path
            .extension()
            .and_then(|v| v.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let heic = matches!(extension.as_str(), "heic" | "heif");
        let mime = match extension.as_str() {
            "png" | "heic" | "heif" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            _ => return Err("Choose a PNG, JPEG, GIF, WebP or HEIC image.".into()),
        };
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
            .map_err(|e| format!("Cannot open {}: {e}", path.display()))?;
        let meta = file.metadata().map_err(|e| e.to_string())?;
        if !meta.is_file() {
            return Err("Choose an image file.".into());
        }
        let bytes = if heic {
            if meta.len() > 20 * 1024 * 1024 {
                return Err("Choose a HEIC image of 20 MiB or less.".into());
            }
            super::convert_heic_to_png(path)?
        } else {
            if meta.len() > remaining as u64 {
                return Err(LIMIT.into());
            }
            let mut bytes = Vec::new();
            file.take(remaining as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|e| e.to_string())?;
            bytes
        };
        if bytes.is_empty() {
            return Err(format!("{} is empty.", path.display()));
        }
        remaining = remaining.checked_sub(bytes.len()).ok_or(LIMIT)?;
        images.push(encode(mime, &bytes));
    }
    Ok(images)
}

pub(crate) fn check_draft(existing: &[ImageInput], added: &[ImageInput]) -> Result<(), String> {
    let mut total = 0;
    for image in existing.iter().chain(added) {
        let ImageInput::Clipboard(_, data) = image else {
            return Err("Select this image on the desktop again.".into());
        };
        let (_, encoded) = data.split_once(',').ok_or("Invalid image attachment")?;
        if encoded.is_empty() {
            return Err("The selected image is empty.".into());
        }
        let padding = encoded.len() - encoded.trim_end_matches('=').len();
        total += encoded.len() / 4 * 3 - padding;
    }
    check_limits(existing.len() + added.len(), total)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn remote_drop_contains_bytes_and_shares_the_paste_budget() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("selection.PNG");
        std::fs::write(&path, b"abcd").unwrap();
        let images = read_images(&[path]).unwrap();
        assert_eq!(images[0].wire()["url"], "data:image/png;base64,YWJjZA==");
        assert!(check_draft(&images, &[encode("image/png", &vec![0; MAX_BYTES - 4])]).is_ok());
        assert!(check_draft(&images, &[encode("image/png", &vec![0; MAX_BYTES - 3])]).is_err());
        assert!(check_draft(&vec![images[0].clone(); 4], &images).is_err());
    }
}
