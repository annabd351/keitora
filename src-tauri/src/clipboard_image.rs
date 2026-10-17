// Temp-write helper for pasted clipboard images.
//
// Clipboard image paste (Windows-first) and drag-drop both converge on injecting a
// quoted path into the PTY via `ptyWrite`; for a pasted image there is no path yet,
// so the frontend reads the clipboard bytes and this module writes them to a
// uniquely-named file in the OS temp dir. Path quoting lives in the TypeScript
// frontend — this module only writes bytes.

use std::io::Write;
use std::path::PathBuf;

/// Write `bytes` to a uniquely-named file in the OS temp dir with the given
/// extension (no leading dot), returning the absolute path.
///
/// `ext` is sanitized to `[A-Za-z0-9]+`; empty or fully-invalid input falls back to
/// `png`. The filename embeds a random suffix so concurrent/successive pastes never
/// collide.
pub fn save_image_to_temp(bytes: &[u8], ext: &str) -> std::io::Result<PathBuf> {
    let ext = sanitize_ext(ext);
    let suffix: u64 = rand::random();
    let name = format!("keitora-paste-{suffix:016x}.{ext}");
    let path = std::env::temp_dir().join(name);
    let mut file = std::fs::File::create(&path)?;
    file.write_all(bytes)?;
    Ok(path)
}

/// Validate `ext` against `[A-Za-z0-9]+`; fall back to `png` when it is empty or
/// contains anything outside that set (e.g. path separators or `.`), rather than
/// silently stripping bad characters.
fn sanitize_ext(ext: &str) -> String {
    if !ext.is_empty() && ext.chars().all(|c| c.is_ascii_alphanumeric()) {
        ext.to_string()
    } else {
        "png".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_exact_bytes_and_file_exists() {
        let bytes = b"not actually a png, just test bytes";
        let path = save_image_to_temp(bytes, "png").unwrap();
        assert!(path.exists());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn successive_calls_yield_different_paths() {
        let a = save_image_to_temp(b"one", "png").unwrap();
        let b = save_image_to_temp(b"two", "png").unwrap();
        assert_ne!(a, b);
        std::fs::remove_file(&a).ok();
        std::fs::remove_file(&b).ok();
    }

    #[test]
    fn extension_is_applied() {
        let path = save_image_to_temp(b"data", "jpg").unwrap();
        assert_eq!(path.extension().unwrap(), "jpg");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn empty_ext_falls_back_to_png() {
        let path = save_image_to_temp(b"data", "").unwrap();
        assert_eq!(path.extension().unwrap(), "png");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn invalid_ext_falls_back_to_png() {
        let path = save_image_to_temp(b"data", "../../etc").unwrap();
        assert_eq!(path.extension().unwrap(), "png");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn path_is_absolute_and_under_temp_dir() {
        let path = save_image_to_temp(b"data", "png").unwrap();
        assert!(path.is_absolute());
        assert!(path.starts_with(std::env::temp_dir()));
        std::fs::remove_file(&path).ok();
    }
}
