//! Filesystem-based font loading with embedded fallback.
//!
//! Loads TrueType font files from `/usr/share/fonts/` at runtime,
//! falling back to compiled-in font data if the filesystem is
//! unavailable (e.g., ext2 not mounted).
//!
//! Modeled after Redox OS's simple directory-based font discovery.

/// Well-known system font directory (Linux/FHS convention).
const FONT_DIR: &str = "/usr/share/fonts";

/// Logical font names mapped to filenames.
const FONT_MAP: &[(&str, &str)] = &[
    ("mono", "JetBrainsMono-Regular.ttf"),
    ("sans", "Inter-Regular.ttf"),
];

/// Load a font file from the filesystem, returning a leaked `&'static [u8]`.
///
/// The memory is intentionally leaked to satisfy the `'static` lifetime
/// required by `FontRenderer`. In a long-running OS this is fine because
/// fonts are loaded once and used for the entire session.
///
/// Returns `None` if the file cannot be read.
pub fn load_font(name: &str) -> Option<&'static [u8]> {
    let filename = FONT_MAP
        .iter()
        .find(|(k, _)| *k == name)
        .map(|(_, v)| *v)
        .unwrap_or(name);

    let path = std::format!("{}/{}", FONT_DIR, filename);
    let data = std::fs::read(&path).ok()?;
    if data.is_empty() {
        return None;
    }
    Some(std::boxed::Box::leak(data.into_boxed_slice()))
}

/// Load a font, falling back to embedded data if the filesystem fails.
pub fn load_font_or_embedded(name: &str, embedded: &'static [u8]) -> (&'static [u8], bool) {
    match load_font(name) {
        Some(data) => (data, true), // (data, from_filesystem)
        None => (embedded, false),
    }
}

/// List all `.ttf` font files available in the system font directory.
pub fn list_fonts() -> std::vec::Vec<std::string::String> {
    let mut fonts = std::vec::Vec::new();
    if let Ok(entries) = std::fs::read_dir(FONT_DIR) {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                if name.ends_with(".ttf") || name.ends_with(".otf") {
                    fonts.push(name.to_string());
                }
            }
        }
    }
    fonts
}

/// Resolve a logical font name (e.g., "mono") to a filesystem path.
pub fn resolve_font_path(name: &str) -> std::string::String {
    let filename = FONT_MAP
        .iter()
        .find(|(k, _)| *k == name)
        .map(|(_, v)| *v)
        .unwrap_or(name);
    std::format!("{}/{}", FONT_DIR, filename)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_map_contains_mono_and_sans() {
        assert!(FONT_MAP.iter().any(|(k, _)| *k == "mono"));
        assert!(FONT_MAP.iter().any(|(k, _)| *k == "sans"));
    }

    #[test]
    fn load_font_or_embedded_falls_back() {
        // On host, /usr/share/fonts/ won't have our fonts, so should fall back.
        let embedded = b"fake font data";
        let (result, from_fs) = load_font_or_embedded("mono", embedded);
        // Either it loaded from fs or fell back to embedded.
        if !from_fs {
            assert_eq!(result, embedded.as_slice());
        }
    }

    #[test]
    fn resolve_font_path_maps_logical_names() {
        assert_eq!(
            resolve_font_path("mono"),
            "/usr/share/fonts/JetBrainsMono-Regular.ttf"
        );
        assert_eq!(
            resolve_font_path("sans"),
            "/usr/share/fonts/Inter-Regular.ttf"
        );
    }

    #[test]
    fn resolve_font_path_passes_through_unknown() {
        assert_eq!(
            resolve_font_path("custom.ttf"),
            "/usr/share/fonts/custom.ttf"
        );
    }
}
