//! Unicode character width classification.

/// Returns `true` for CJK and fullwidth codepoints that occupy two cells
/// in a fixed-width terminal grid.
#[inline]
pub const fn is_double_width(cp: u32) -> bool {
    matches!(cp,
        0x1100..=0x115F   // Hangul Jamo
        | 0x2E80..=0x303E // CJK Radicals Supplement .. CJK Symbols
        | 0x3041..=0x33BF // Hiragana .. CJK Compatibility
        | 0x3400..=0x4DBF // CJK Unified Ideographs Extension A
        | 0x4E00..=0x9FFF // CJK Unified Ideographs
        | 0xA000..=0xA4CF // Yi Syllables + Yi Radicals
        | 0xAC00..=0xD7AF // Hangul Syllables
        | 0xF900..=0xFAFF // CJK Compatibility Ideographs
        | 0xFE30..=0xFE6F // CJK Compatibility Forms
        | 0xFF01..=0xFF60 // Fullwidth Forms
        | 0xFFE0..=0xFFE6 // Fullwidth Signs
        | 0x20000..=0x2FFFF // CJK Unified Ideographs Extension B+
        | 0x30000..=0x3FFFF // CJK Extension G+
    )
}
