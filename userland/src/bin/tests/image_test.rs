#![feature(restricted_std)]

use slopos_abi::draw::Color32;
use slopos_image::{DecodeOptions, PngError};
use slopos_userland as _;

const RGBA_1X1: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0,
    0, 0, 31, 21, 196, 137, 0, 0, 0, 16, 73, 68, 65, 84, 120, 1, 1, 5, 0, 250, 255, 0, 255, 0, 0,
    255, 5, 0, 1, 255, 250, 92, 136, 209, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];
const WALLPAPER_PATH: &str = "/usr/share/slopos/wallpapers/default.png";

fn decode_embedded_rgba() -> Result<(), String> {
    let image = slopos_image::decode_png(RGBA_1X1, DecodeOptions::default())
        .map_err(|e| format!("decode failed: {e}"))?;
    if image.width == 1 && image.height == 1 && image.pixels == [Color32::rgb(255, 0, 0)] {
        Ok(())
    } else {
        Err(format!(
            "unexpected image: {}x{} {} pixels",
            image.width,
            image.height,
            image.pixels.len()
        ))
    }
}

fn reject_bad_signature() -> Result<(), String> {
    if matches!(
        slopos_image::decode_png(b"not a png", DecodeOptions::default()),
        Err(PngError::BadSignature)
    ) {
        Ok(())
    } else {
        Err("bad signature was not rejected".to_string())
    }
}

fn packaged_wallpaper_metadata() -> Result<(), String> {
    let metadata = std::fs::metadata(WALLPAPER_PATH)
        .map_err(|e| format!("metadata({WALLPAPER_PATH}) failed: {e}"))?;
    if metadata.len() == 271 {
        Ok(())
    } else {
        Err(format!("metadata len {}", metadata.len()))
    }
}

fn packaged_wallpaper_read() -> Result<(), String> {
    let bytes =
        std::fs::read(WALLPAPER_PATH).map_err(|e| format!("read({WALLPAPER_PATH}) failed: {e}"))?;
    if bytes.len() == 271 && bytes.starts_with(b"\x89PNG\r\n\x1A\n") {
        Ok(())
    } else {
        Err(format!(
            "read len {} signature={}",
            bytes.len(),
            bytes.starts_with(b"\x89PNG\r\n\x1A\n")
        ))
    }
}

fn packaged_wallpaper_checksum() -> Result<(), String> {
    let bytes =
        std::fs::read(WALLPAPER_PATH).map_err(|e| format!("read({WALLPAPER_PATH}) failed: {e}"))?;
    verify_wallpaper_checksum(&bytes)
}

fn verify_wallpaper_checksum(bytes: &[u8]) -> Result<(), String> {
    let sum = bytes
        .iter()
        .fold(0u32, |acc, byte| acc.wrapping_add(*byte as u32));
    let fnv = bytes.iter().fold(0u32, |acc, byte| {
        acc.wrapping_mul(16_777_619) ^ *byte as u32
    });
    if sum == 27_545 && fnv == 941_773_329 {
        Ok(())
    } else {
        Err(format!("checksum sum={sum} fnv={fnv}"))
    }
}

fn decode_packaged_wallpaper_bytes() -> Result<(), String> {
    let bytes =
        std::fs::read(WALLPAPER_PATH).map_err(|e| format!("read({WALLPAPER_PATH}) failed: {e}"))?;
    verify_wallpaper_checksum(&bytes)?;
    let image =
        slopos_image::decode_png(&bytes, DecodeOptions::default()).map_err(|e| format!("{e}"))?;
    if image.width == 73
        && image.height == 18
        && image.pixels.len() == image.width as usize * image.height as usize
    {
        Ok(())
    } else {
        Err(format!(
            "unexpected image: {}x{} {} pixels",
            image.width,
            image.height,
            image.pixels.len()
        ))
    }
}

fn load_packaged_wallpaper() -> Result<(), String> {
    let image = slopos_image::load_path(WALLPAPER_PATH, DecodeOptions::default())
        .map_err(|e| format!("{e}"))?;
    if image.width > 0
        && image.height > 0
        && image.pixels.len() == image.width as usize * image.height as usize
    {
        Ok(())
    } else {
        Err(format!(
            "unexpected image: {}x{} {} pixels",
            image.width,
            image.height,
            image.pixels.len()
        ))
    }
}

fn run_cases(cases: &[(&'static str, fn() -> Result<(), String>)]) -> ! {
    use slopos_slibc::test_harness::{TestStatus, report};

    let mut failed = 0u32;
    for (name, f) in cases {
        eprintln!("utest-progress: image_test::{name} start");
        let result = f();
        let (status, message) = match result {
            Ok(()) => {
                eprintln!("utest-progress: image_test::{name} pass");
                (TestStatus::Pass, String::new())
            }
            Err(message) => {
                eprintln!("utest-progress: image_test::{name} fail");
                failed = failed.saturating_add(1);
                (TestStatus::Fail, message)
            }
        };
        report(status, name, &message);
    }
    slopos_userland::syscall::core::exit_with_code(failed.min(255) as i32)
}

fn main() {
    run_cases(&[
        ("decode_embedded_rgba", decode_embedded_rgba),
        ("reject_bad_signature", reject_bad_signature),
        ("packaged_wallpaper_metadata", packaged_wallpaper_metadata),
        ("packaged_wallpaper_read", packaged_wallpaper_read),
        ("packaged_wallpaper_checksum", packaged_wallpaper_checksum),
        (
            "decode_packaged_wallpaper_bytes",
            decode_packaged_wallpaper_bytes,
        ),
        ("load_packaged_wallpaper", load_packaged_wallpaper),
    ]);
}
