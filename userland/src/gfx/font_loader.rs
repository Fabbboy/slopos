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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_map_contains_mono_and_sans() {
        assert!(FONT_MAP.iter().any(|(k, _)| *k == "mono"));
        assert!(FONT_MAP.iter().any(|(k, _)| *k == "sans"));
    }
}
