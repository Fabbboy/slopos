const FONT_DIR: &str = "/usr/share/fonts";

const FONT_MAP: &[(&str, &str)] = &[
    ("mono", "JetBrainsMono-Regular.ttf"),
    ("sans", "Inter-Regular.ttf"),
];

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
    // Intentional leak: font data must be 'static and is loaded once per session.
    Some(std::boxed::Box::leak(data.into_boxed_slice()))
}

pub fn load_font_or_embedded(name: &str, embedded: &'static [u8]) -> (&'static [u8], bool) {
    match load_font(name) {
        Some(data) => (data, true),
        None => (embedded, false),
    }
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
        let embedded = b"fake font data";
        let (result, from_fs) = load_font_or_embedded("mono", embedded);
        if !from_fs {
            assert_eq!(result, embedded.as_slice());
        }
    }
}
