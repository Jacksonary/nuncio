//! Attachment safety checks and MIME type inference.

/// Executable / installer / script extensions that must never be launched
/// directly from the mail client.
const DANGEROUS_EXTENSIONS: &[&str] = &[
    // Windows executables, scripts and shortcuts
    "exe", "com", "bat", "cmd", "ps1", "psm1", "vbs", "vbe", "js", "jse", "wsf", "wsh", "msi",
    "msp", "scr", "pif", "lnk", "reg", "hta", "cpl", "jar", "app", "command", "sh",
    // Disk images that can auto-mount / auto-run
    "iso", "img", "vhd", "vhdx", // Packaged applications
    "appx", "msix", // Macro-enabled Office documents
    "docm", "xlsm", "pptm",
];

/// True when `filename` has an extension considered dangerous to open directly.
///
/// Trailing dots and spaces are ignored (Windows strips them), and only the
/// final extension is considered.
pub fn is_dangerous(filename: &str) -> bool {
    let name = filename
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(filename)
        .trim()
        .trim_end_matches(['.', ' ']);
    let extension = match name.rsplit_once('.') {
        Some((_, extension)) => extension,
        None => return false,
    };
    let extension = extension.trim().to_ascii_lowercase();
    !extension.is_empty()
        && DANGEROUS_EXTENSIONS
            .iter()
            .any(|danger| *danger == extension)
}

/// Guess a MIME type from the file name, falling back to
/// `application/octet-stream`.
pub fn mime_for_filename(filename: &str) -> String {
    mime_guess::from_path(filename)
        .first_or_octet_stream()
        .essence_str()
        .to_string()
}

/// A reasonable fallback file name for an attachment without one.
pub fn fallback_filename(index: usize, mime_type: &str) -> String {
    let extension = mime_guess::get_mime_extensions_str(mime_type)
        .and_then(|extensions| extensions.first().copied())
        .unwrap_or("bin");
    format!("attachment-{index}.{extension}")
}

/// Build a `Content-ID` value suitable for using as a `cid:` URL.
pub fn content_id_for(filename: &str) -> String {
    let stem: String = filename
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect();
    let stem = stem.trim_matches(['.', '_', '-']);
    let stem = if stem.is_empty() { "image" } else { stem };
    format!("{stem}-{}@nuncio", uuid::Uuid::new_v4())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_dangerous_extensions() {
        for name in [
            "setup.exe",
            "SETUP.EXE",
            "run.bat ",
            "script.PS1",
            "macro.docm",
            "disk.iso",
            "C:\\Temp\\thing.lnk.",
        ] {
            assert!(is_dangerous(name), "{name} should be dangerous");
        }
    }

    #[test]
    fn allows_normal_attachments() {
        for name in [
            "photo.jpg",
            "report.pdf",
            "notes.txt",
            "archive.zip",
            "no-extension",
        ] {
            assert!(!is_dangerous(name), "{name} should be safe");
        }
    }

    #[test]
    fn guesses_mime_types() {
        assert_eq!(mime_for_filename("photo.PNG"), "image/png");
        assert_eq!(mime_for_filename("unknown.zzz"), "application/octet-stream");
    }

    #[test]
    fn creates_valid_content_ids() {
        let content_id = content_id_for("我的 图片.png");
        assert!(content_id.contains('@'));
        assert!(content_id.is_ascii());
    }
}
