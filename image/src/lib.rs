#![forbid(unsafe_code)]
#![feature(restricted_std)]

use slopos_abi::draw::Color32;
use std::fmt;
use std::fs;
use std::path::Path;

const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1A\n";
const ADAM7: [(u32, u32, u32, u32); 7] = [
    (0, 0, 8, 8),
    (4, 0, 8, 8),
    (0, 4, 4, 8),
    (2, 0, 4, 4),
    (0, 2, 2, 4),
    (1, 0, 2, 2),
    (0, 1, 1, 2),
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    /// Straight-alpha sRGB pixels in 0xAARRGGBB order.
    pub pixels: Vec<Color32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodeOptions {
    pub max_pixels: usize,
    pub validate_crc: bool,
}

impl Default for DecodeOptions {
    fn default() -> Self {
        Self {
            max_pixels: 16_777_216,
            validate_crc: true,
        }
    }
}

#[derive(Debug)]
pub enum ImageError {
    Png(PngError),
    Io(std::io::Error),
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Png(e) => write!(f, "png decode failed: {e}"),
            Self::Io(e) => write!(f, "image I/O failed: {e}"),
        }
    }
}

impl std::error::Error for ImageError {}

impl From<PngError> for ImageError {
    fn from(value: PngError) -> Self {
        Self::Png(value)
    }
}

impl From<std::io::Error> for ImageError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PngError {
    BadSignature,
    Truncated,
    InvalidChunk(&'static str),
    Unsupported(&'static str),
    CrcMismatch {
        chunk: [u8; 4],
        expected: u32,
        actual: u32,
    },
    Inflate(InflateError),
    BadDimensions,
    OutputOverflow,
    ResourceLimitExceeded,
    InvalidFilter(u8),
    IdatLengthMismatch {
        expected: usize,
        actual: usize,
    },
}

impl fmt::Display for PngError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadSignature => write!(f, "bad PNG signature"),
            Self::Truncated => write!(f, "truncated PNG stream"),
            Self::InvalidChunk(msg) => write!(f, "invalid PNG chunk: {msg}"),
            Self::Unsupported(msg) => write!(f, "unsupported PNG feature: {msg}"),
            Self::CrcMismatch {
                chunk,
                expected,
                actual,
            } => write!(
                f,
                "CRC mismatch in {}: expected {expected:08x}, got {actual:08x}",
                chunk_name(*chunk),
            ),
            Self::Inflate(e) => write!(f, "inflate failed: {e}"),
            Self::BadDimensions => write!(f, "bad PNG dimensions"),
            Self::OutputOverflow => write!(f, "PNG output size overflow"),
            Self::ResourceLimitExceeded => write!(f, "PNG resource limit exceeded"),
            Self::InvalidFilter(filter) => write!(f, "invalid PNG filter {filter}"),
            Self::IdatLengthMismatch { expected, actual } => {
                write!(
                    f,
                    "IDAT output length mismatch: expected {expected}, got {actual}"
                )
            }
        }
    }
}

impl std::error::Error for PngError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InflateError {
    Truncated,
    InvalidZlibHeader,
    PresetDictionary,
    UnsupportedBlockType,
    InvalidStoredBlock,
    InvalidHuffmanCode,
    InvalidCodeLength,
    InvalidDistance,
    MissingEndBlock,
    AdlerMismatch { expected: u32, actual: u32 },
    OutputLimitExceeded { limit: usize, requested: usize },
}

impl fmt::Display for InflateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => write!(f, "truncated deflate stream"),
            Self::InvalidZlibHeader => write!(f, "invalid zlib header"),
            Self::PresetDictionary => write!(f, "zlib preset dictionary is unsupported"),
            Self::UnsupportedBlockType => write!(f, "unsupported deflate block type"),
            Self::InvalidStoredBlock => write!(f, "invalid stored deflate block"),
            Self::InvalidHuffmanCode => write!(f, "invalid deflate Huffman code"),
            Self::InvalidCodeLength => write!(f, "invalid deflate code length stream"),
            Self::InvalidDistance => write!(f, "invalid deflate back-reference distance"),
            Self::MissingEndBlock => write!(f, "deflate stream ended before end block"),
            Self::AdlerMismatch { expected, actual } => {
                write!(
                    f,
                    "adler32 mismatch: expected {expected:08x}, got {actual:08x}"
                )
            }
            Self::OutputLimitExceeded { limit, requested } => {
                write!(
                    f,
                    "inflated output limit exceeded: limit {limit}, requested {requested}"
                )
            }
        }
    }
}

impl std::error::Error for InflateError {}

pub fn decode(bytes: &[u8], options: DecodeOptions) -> Result<Image, ImageError> {
    decode_png(bytes, options).map_err(ImageError::Png)
}

pub fn decode_png(bytes: &[u8], options: DecodeOptions) -> Result<Image, PngError> {
    PngDecoder::new(bytes, options).decode()
}

pub fn load_path(path: impl AsRef<Path>, options: DecodeOptions) -> Result<Image, ImageError> {
    let bytes = fs::read(path)?;
    decode(&bytes, options)
}

#[derive(Clone, Copy)]
struct Ihdr {
    width: u32,
    height: u32,
    bit_depth: u8,
    color_type: u8,
    interlace: u8,
}

struct PngDecoder<'a> {
    bytes: &'a [u8],
    options: DecodeOptions,
}

impl<'a> PngDecoder<'a> {
    fn new(bytes: &'a [u8], options: DecodeOptions) -> Self {
        Self { bytes, options }
    }

    fn decode(&self) -> Result<Image, PngError> {
        if self.bytes.len() < PNG_SIGNATURE.len() || &self.bytes[..8] != PNG_SIGNATURE {
            return Err(PngError::BadSignature);
        }

        let mut offset = 8usize;
        let mut ihdr: Option<Ihdr> = None;
        let mut palette: Vec<[u8; 3]> = Vec::new();
        let mut trns: Vec<u8> = Vec::new();
        let mut idat: Vec<u8> = Vec::new();
        let mut seen_idat = false;
        let mut seen_iend = false;

        while offset < self.bytes.len() {
            if self.bytes.len().saturating_sub(offset) < 12 {
                return Err(PngError::Truncated);
            }
            let len = read_be_u32(&self.bytes[offset..offset + 4]) as usize;
            let chunk_type: [u8; 4] = self.bytes[offset + 4..offset + 8]
                .try_into()
                .map_err(|_| PngError::Truncated)?;
            let data_start = offset + 8;
            let data_end = data_start
                .checked_add(len)
                .ok_or(PngError::OutputOverflow)?;
            let crc_end = data_end.checked_add(4).ok_or(PngError::OutputOverflow)?;
            if crc_end > self.bytes.len() {
                return Err(PngError::Truncated);
            }
            let data = &self.bytes[data_start..data_end];
            let expected_crc = read_be_u32(&self.bytes[data_end..crc_end]);
            if self.options.validate_crc {
                let actual_crc = crc32_chunk(chunk_type, data);
                if actual_crc != expected_crc {
                    return Err(PngError::CrcMismatch {
                        chunk: chunk_type,
                        expected: expected_crc,
                        actual: actual_crc,
                    });
                }
            }

            match &chunk_type {
                b"IHDR" => {
                    if ihdr.is_some() || data.len() != 13 {
                        return Err(PngError::InvalidChunk("IHDR"));
                    }
                    let parsed = parse_ihdr(data)?;
                    validate_ihdr(parsed, self.options.max_pixels)?;
                    ihdr = Some(parsed);
                }
                b"PLTE" => {
                    if ihdr.is_none() || seen_idat || data.is_empty() || data.len() % 3 != 0 {
                        return Err(PngError::InvalidChunk("PLTE"));
                    }
                    if data.len() / 3 > 256 {
                        return Err(PngError::InvalidChunk("PLTE too large"));
                    }
                    palette.clear();
                    for rgb in data.chunks_exact(3) {
                        palette.push([rgb[0], rgb[1], rgb[2]]);
                    }
                }
                b"tRNS" => {
                    if ihdr.is_none() || seen_idat {
                        return Err(PngError::InvalidChunk("tRNS"));
                    }
                    trns.clear();
                    trns.extend_from_slice(data);
                }
                b"IDAT" => {
                    if ihdr.is_none() {
                        return Err(PngError::InvalidChunk("IDAT before IHDR"));
                    }
                    seen_idat = true;
                    extend_vec_from_slice(&mut idat, data);
                }
                b"IEND" => {
                    if data.is_empty() {
                        seen_iend = true;
                        offset = crc_end;
                        break;
                    }
                    return Err(PngError::InvalidChunk("IEND"));
                }
                _ => {
                    if is_critical(chunk_type) {
                        return Err(PngError::Unsupported("unknown critical chunk"));
                    }
                }
            }

            offset = crc_end;
        }

        if !seen_iend || offset > self.bytes.len() {
            return Err(PngError::Truncated);
        }
        let ihdr = ihdr.ok_or(PngError::InvalidChunk("missing IHDR"))?;
        if idat.is_empty() {
            return Err(PngError::InvalidChunk("missing IDAT"));
        }
        validate_png_state(ihdr, &palette, &trns)?;

        let expected = expected_image_data_len(ihdr)?;
        let inflated = inflate_zlib(&idat, expected).map_err(PngError::Inflate)?;
        if inflated.len() != expected {
            return Err(PngError::IdatLengthMismatch {
                expected,
                actual: inflated.len(),
            });
        }

        let pixel_count = checked_pixel_count(ihdr.width, ihdr.height, self.options.max_pixels)?;
        let mut pixels = vec![Color32::TRANSPARENT; pixel_count];
        if ihdr.interlace == 0 {
            decode_non_interlaced(ihdr, &palette, &trns, &inflated, &mut pixels)?;
        } else {
            decode_adam7(ihdr, &palette, &trns, &inflated, &mut pixels)?;
        }
        Ok(Image {
            width: ihdr.width,
            height: ihdr.height,
            pixels,
        })
    }
}

fn parse_ihdr(data: &[u8]) -> Result<Ihdr, PngError> {
    if data.len() != 13 {
        return Err(PngError::InvalidChunk("IHDR"));
    }
    if data[10] != 0 {
        return Err(PngError::Unsupported("non-deflate PNG compression"));
    }
    if data[11] != 0 {
        return Err(PngError::Unsupported("non-adaptive PNG filter method"));
    }
    if data[12] > 1 {
        return Err(PngError::Unsupported("unknown PNG interlace method"));
    }
    Ok(Ihdr {
        width: read_be_u32(&data[0..4]),
        height: read_be_u32(&data[4..8]),
        bit_depth: data[8],
        color_type: data[9],
        interlace: data[12],
    })
}

fn validate_ihdr(ihdr: Ihdr, max_pixels: usize) -> Result<(), PngError> {
    checked_pixel_count(ihdr.width, ihdr.height, max_pixels)?;
    match (ihdr.color_type, ihdr.bit_depth) {
        (0, 1 | 2 | 4 | 8 | 16) | (2, 8 | 16) | (3, 1 | 2 | 4 | 8) | (4, 8 | 16) | (6, 8 | 16) => {
            Ok(())
        }
        (0 | 2 | 3 | 4 | 6, _) => Err(PngError::Unsupported("illegal PNG bit depth")),
        _ => Err(PngError::Unsupported("unknown PNG color type")),
    }
}

fn validate_png_state(ihdr: Ihdr, palette: &[[u8; 3]], trns: &[u8]) -> Result<(), PngError> {
    match ihdr.color_type {
        0 if !trns.is_empty() && trns.len() != 2 => Err(PngError::InvalidChunk("tRNS grayscale")),
        2 if !trns.is_empty() && trns.len() != 6 => Err(PngError::InvalidChunk("tRNS RGB")),
        3 => {
            if palette.is_empty() {
                Err(PngError::InvalidChunk("indexed PNG without PLTE"))
            } else if trns.len() > palette.len() {
                Err(PngError::InvalidChunk("tRNS longer than PLTE"))
            } else {
                Ok(())
            }
        }
        4 | 6 if !trns.is_empty() => Err(PngError::InvalidChunk("tRNS with alpha color type")),
        _ => Ok(()),
    }
}

fn checked_pixel_count(width: u32, height: u32, max_pixels: usize) -> Result<usize, PngError> {
    if width == 0 || height == 0 {
        return Err(PngError::BadDimensions);
    }
    let count = (width as usize)
        .checked_mul(height as usize)
        .ok_or(PngError::OutputOverflow)?;
    if count > max_pixels {
        return Err(PngError::ResourceLimitExceeded);
    }
    Ok(count)
}

fn expected_image_data_len(ihdr: Ihdr) -> Result<usize, PngError> {
    if ihdr.interlace == 0 {
        let row = row_bytes(ihdr.width, ihdr)?;
        return row
            .checked_add(1)
            .and_then(|v| v.checked_mul(ihdr.height as usize))
            .ok_or(PngError::OutputOverflow);
    }

    let mut total = 0usize;
    for (sx, sy, dx, dy) in ADAM7 {
        let pw = pass_len(ihdr.width, sx, dx);
        let ph = pass_len(ihdr.height, sy, dy);
        if pw == 0 || ph == 0 {
            continue;
        }
        let row = row_bytes(pw, ihdr)?;
        total = total
            .checked_add(
                row.checked_add(1)
                    .and_then(|v| v.checked_mul(ph as usize))
                    .ok_or(PngError::OutputOverflow)?,
            )
            .ok_or(PngError::OutputOverflow)?;
    }
    Ok(total)
}

fn decode_non_interlaced(
    ihdr: Ihdr,
    palette: &[[u8; 3]],
    trns: &[u8],
    data: &[u8],
    pixels: &mut [Color32],
) -> Result<(), PngError> {
    let row_len = row_bytes(ihdr.width, ihdr)?;
    let filter_bpp = filter_bytes_per_pixel(ihdr);
    let mut prev = vec![0u8; row_len];
    let mut curr = vec![0u8; row_len];
    let mut offset = 0usize;

    for y in 0..ihdr.height {
        let filter = *data.get(offset).ok_or(PngError::Truncated)?;
        offset += 1;
        let end = offset
            .checked_add(row_len)
            .ok_or(PngError::OutputOverflow)?;
        if end > data.len() {
            return Err(PngError::Truncated);
        }
        curr.copy_from_slice(&data[offset..end]);
        offset = end;
        unfilter(filter, filter_bpp, &prev, &mut curr)?;
        expand_row(
            ihdr,
            palette,
            trns,
            &curr,
            &mut pixels[y as usize * ihdr.width as usize..],
        )?;
        std::mem::swap(&mut prev, &mut curr);
    }
    Ok(())
}

fn decode_adam7(
    ihdr: Ihdr,
    palette: &[[u8; 3]],
    trns: &[u8],
    data: &[u8],
    pixels: &mut [Color32],
) -> Result<(), PngError> {
    let mut offset = 0usize;
    for (sx, sy, dx, dy) in ADAM7 {
        let pw = pass_len(ihdr.width, sx, dx);
        let ph = pass_len(ihdr.height, sy, dy);
        if pw == 0 || ph == 0 {
            continue;
        }
        let row_len = row_bytes(pw, ihdr)?;
        let filter_bpp = filter_bytes_per_pixel(ihdr);
        let mut prev = vec![0u8; row_len];
        let mut curr = vec![0u8; row_len];
        let mut expanded = vec![Color32::TRANSPARENT; pw as usize];
        for row in 0..ph {
            let filter = *data.get(offset).ok_or(PngError::Truncated)?;
            offset += 1;
            let end = offset
                .checked_add(row_len)
                .ok_or(PngError::OutputOverflow)?;
            if end > data.len() {
                return Err(PngError::Truncated);
            }
            curr.copy_from_slice(&data[offset..end]);
            offset = end;
            unfilter(filter, filter_bpp, &prev, &mut curr)?;
            expand_row(ihdr, palette, trns, &curr, &mut expanded)?;
            let dst_y = sy + row * dy;
            let row_base = dst_y as usize * ihdr.width as usize;
            for x in 0..pw {
                let dst_x = sx + x * dx;
                pixels[row_base + dst_x as usize] = expanded[x as usize];
            }
            std::mem::swap(&mut prev, &mut curr);
        }
    }
    if offset != data.len() {
        return Err(PngError::IdatLengthMismatch {
            expected: offset,
            actual: data.len(),
        });
    }
    Ok(())
}

fn unfilter(filter: u8, bpp: usize, prev: &[u8], row: &mut [u8]) -> Result<(), PngError> {
    match filter {
        0 => {}
        1 => {
            for i in 0..row.len() {
                let left = if i >= bpp { row[i - bpp] } else { 0 };
                row[i] = row[i].wrapping_add(left);
            }
        }
        2 => {
            for i in 0..row.len() {
                row[i] = row[i].wrapping_add(prev[i]);
            }
        }
        3 => {
            for i in 0..row.len() {
                let left = if i >= bpp { row[i - bpp] } else { 0 };
                let up = prev[i];
                row[i] = row[i].wrapping_add(((left as u16 + up as u16) / 2) as u8);
            }
        }
        4 => {
            for i in 0..row.len() {
                let left = if i >= bpp { row[i - bpp] } else { 0 };
                let up = prev[i];
                let up_left = if i >= bpp { prev[i - bpp] } else { 0 };
                row[i] = row[i].wrapping_add(paeth(left, up, up_left));
            }
        }
        other => return Err(PngError::InvalidFilter(other)),
    }
    Ok(())
}

fn expand_row(
    ihdr: Ihdr,
    palette: &[[u8; 3]],
    trns: &[u8],
    row: &[u8],
    out: &mut [Color32],
) -> Result<(), PngError> {
    let width = out.len().min(ihdr.width as usize);
    match ihdr.color_type {
        0 => {
            let transparent = (trns.len() == 2).then(|| read_be_u16(trns));
            for (x, dst) in out.iter_mut().take(width).enumerate() {
                let raw = sample_bits(row, x, ihdr.bit_depth) as u16;
                let gray = scale_sample_to_u8(raw, ihdr.bit_depth);
                let alpha = if transparent == Some(raw) { 0 } else { 255 };
                *dst = Color32::new(gray, gray, gray, alpha);
            }
        }
        2 => {
            for (x, dst) in out.iter_mut().take(width).enumerate() {
                let (r_raw, g_raw, b_raw, r, g, b) = if ihdr.bit_depth == 8 {
                    let i = x * 3;
                    (
                        row[i] as u16,
                        row[i + 1] as u16,
                        row[i + 2] as u16,
                        row[i],
                        row[i + 1],
                        row[i + 2],
                    )
                } else {
                    let i = x * 6;
                    let rr = read_be_u16(&row[i..i + 2]);
                    let gg = read_be_u16(&row[i + 2..i + 4]);
                    let bb = read_be_u16(&row[i + 4..i + 6]);
                    (rr, gg, bb, row[i], row[i + 2], row[i + 4])
                };
                let alpha = if trns.len() == 6
                    && r_raw == read_be_u16(&trns[0..2])
                    && g_raw == read_be_u16(&trns[2..4])
                    && b_raw == read_be_u16(&trns[4..6])
                {
                    0
                } else {
                    255
                };
                *dst = Color32::new(r, g, b, alpha);
            }
        }
        3 => {
            for (x, dst) in out.iter_mut().take(width).enumerate() {
                let idx = sample_bits(row, x, ihdr.bit_depth) as usize;
                let rgb = palette
                    .get(idx)
                    .ok_or(PngError::InvalidChunk("palette index out of range"))?;
                let alpha = trns.get(idx).copied().unwrap_or(255);
                *dst = Color32::new(rgb[0], rgb[1], rgb[2], alpha);
            }
        }
        4 => {
            for (x, dst) in out.iter_mut().take(width).enumerate() {
                if ihdr.bit_depth == 8 {
                    let i = x * 2;
                    let gray = row[i];
                    let alpha = row[i + 1];
                    *dst = Color32::new(gray, gray, gray, alpha);
                } else {
                    let i = x * 4;
                    let gray = row[i];
                    let alpha = row[i + 2];
                    *dst = Color32::new(gray, gray, gray, alpha);
                }
            }
        }
        6 => {
            for (x, dst) in out.iter_mut().take(width).enumerate() {
                if ihdr.bit_depth == 8 {
                    let i = x * 4;
                    *dst = Color32::new(row[i], row[i + 1], row[i + 2], row[i + 3]);
                } else {
                    let i = x * 8;
                    *dst = Color32::new(row[i], row[i + 2], row[i + 4], row[i + 6]);
                }
            }
        }
        _ => return Err(PngError::Unsupported("unknown PNG color type")),
    }
    Ok(())
}

fn row_bytes(width: u32, ihdr: Ihdr) -> Result<usize, PngError> {
    let bits = bits_per_pixel(ihdr) as usize;
    (width as usize)
        .checked_mul(bits)
        .and_then(|v| v.checked_add(7))
        .map(|v| v / 8)
        .ok_or(PngError::OutputOverflow)
}

fn bits_per_pixel(ihdr: Ihdr) -> u8 {
    match ihdr.color_type {
        0 | 3 => ihdr.bit_depth,
        2 => ihdr.bit_depth * 3,
        4 => ihdr.bit_depth * 2,
        6 => ihdr.bit_depth * 4,
        _ => 0,
    }
}

fn filter_bytes_per_pixel(ihdr: Ihdr) -> usize {
    usize::from(bits_per_pixel(ihdr)).div_ceil(8).max(1)
}

fn pass_len(size: u32, start: u32, step: u32) -> u32 {
    if size <= start {
        0
    } else {
        (size - start).div_ceil(step)
    }
}

fn sample_bits(row: &[u8], x: usize, bit_depth: u8) -> u8 {
    if bit_depth == 8 {
        return row[x];
    }
    let bit_index = x * bit_depth as usize;
    let byte = row[bit_index / 8];
    let shift = 8usize - bit_depth as usize - (bit_index % 8);
    let mask = (1u16 << bit_depth) - 1;
    ((byte as u16 >> shift) & mask) as u8
}

fn scale_sample_to_u8(value: u16, bit_depth: u8) -> u8 {
    if bit_depth == 8 {
        value as u8
    } else if bit_depth == 16 {
        (value >> 8) as u8
    } else {
        let max = (1u32 << bit_depth) - 1;
        ((value as u32 * 255 + max / 2) / max) as u8
    }
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let a = a as i32;
    let b = b as i32;
    let c = c as i32;
    let p = a + b - c;
    let pa = (p - a).abs();
    let pb = (p - b).abs();
    let pc = (p - c).abs();
    if pa <= pb && pa <= pc {
        a as u8
    } else if pb <= pc {
        b as u8
    } else {
        c as u8
    }
}

fn inflate_zlib(data: &[u8], max_output: usize) -> Result<Vec<u8>, InflateError> {
    if data.len() < 6 {
        return Err(InflateError::Truncated);
    }
    let cmf = data[0];
    let flg = data[1];
    if cmf & 0x0F != 8 || (cmf >> 4) > 7 || (((cmf as u16) << 8 | flg as u16) % 31) != 0 {
        return Err(InflateError::InvalidZlibHeader);
    }
    if flg & 0x20 != 0 {
        return Err(InflateError::PresetDictionary);
    }
    let expected_adler = read_be_u32(&data[data.len() - 4..]);
    let deflate = &data[2..data.len() - 4];
    let out = inflate_deflate(deflate, max_output)?;
    let actual_adler = adler32(&out);
    if actual_adler != expected_adler {
        return Err(InflateError::AdlerMismatch {
            expected: expected_adler,
            actual: actual_adler,
        });
    }
    Ok(out)
}

fn inflate_deflate(data: &[u8], max_output: usize) -> Result<Vec<u8>, InflateError> {
    let mut bits = BitReader::new(data);
    let mut out = Vec::with_capacity(max_output);
    loop {
        let final_block = bits.read_bits(1)? != 0;
        let block_type = bits.read_bits(2)?;
        match block_type {
            0 => inflate_stored(&mut bits, &mut out, max_output)?,
            1 => {
                let (lit, dist) = fixed_tables()?;
                inflate_huffman_block(&mut bits, &lit, &dist, &mut out, max_output)?;
            }
            2 => {
                let (lit, dist) = dynamic_tables(&mut bits)?;
                inflate_huffman_block(&mut bits, &lit, &dist, &mut out, max_output)?;
            }
            _ => return Err(InflateError::UnsupportedBlockType),
        }
        if final_block {
            return Ok(out);
        }
    }
}

fn inflate_stored(
    bits: &mut BitReader<'_>,
    out: &mut Vec<u8>,
    max_output: usize,
) -> Result<(), InflateError> {
    bits.align_byte();
    let len = bits.read_u16_le()?;
    let nlen = bits.read_u16_le()?;
    if len != !nlen {
        return Err(InflateError::InvalidStoredBlock);
    }
    if out
        .len()
        .checked_add(len as usize)
        .is_none_or(|n| n > max_output)
    {
        return Err(InflateError::OutputLimitExceeded {
            limit: max_output,
            requested: out.len().saturating_add(len as usize),
        });
    }
    let bytes = bits.read_bytes(len as usize)?;
    extend_vec_from_slice(out, bytes);
    Ok(())
}

fn extend_vec_from_slice(out: &mut Vec<u8>, bytes: &[u8]) {
    // TODO(tech-debt): bulk slice copy corrupts in SlopOS userland; drop the byte loop when fixed.
    out.reserve(bytes.len());
    for &byte in bytes {
        out.push(byte);
    }
}

fn inflate_huffman_block(
    bits: &mut BitReader<'_>,
    lit: &Huffman,
    dist: &Huffman,
    out: &mut Vec<u8>,
    max_output: usize,
) -> Result<(), InflateError> {
    loop {
        let sym = lit.decode(bits)?;
        match sym {
            0..=255 => {
                if out.len() >= max_output {
                    return Err(InflateError::OutputLimitExceeded {
                        limit: max_output,
                        requested: out.len().saturating_add(1),
                    });
                }
                out.push(sym as u8);
            }
            256 => return Ok(()),
            257..=285 => {
                let idx = (sym - 257) as usize;
                let mut len = LENGTH_BASE[idx] as usize;
                let extra = LENGTH_EXTRA[idx];
                if extra != 0 {
                    len += bits.read_bits(extra)? as usize;
                }
                let dist_sym = dist.decode(bits)? as usize;
                if dist_sym >= DIST_BASE.len() {
                    return Err(InflateError::InvalidDistance);
                }
                let mut distance = DIST_BASE[dist_sym] as usize;
                let dist_extra = DIST_EXTRA[dist_sym];
                if dist_extra != 0 {
                    distance += bits.read_bits(dist_extra)? as usize;
                }
                if distance == 0 || distance > out.len() {
                    return Err(InflateError::InvalidDistance);
                }
                if out.len().checked_add(len).is_none_or(|n| n > max_output) {
                    return Err(InflateError::OutputLimitExceeded {
                        limit: max_output,
                        requested: out.len().saturating_add(len),
                    });
                }
                for _ in 0..len {
                    let src = out.len() - distance;
                    out.push(out[src]);
                }
            }
            _ => return Err(InflateError::InvalidHuffmanCode),
        }
    }
}

fn fixed_tables() -> Result<(Huffman, Huffman), InflateError> {
    let mut lit_lengths = vec![0u8; 288];
    for v in lit_lengths.iter_mut().take(144) {
        *v = 8;
    }
    for v in lit_lengths.iter_mut().take(256).skip(144) {
        *v = 9;
    }
    for v in lit_lengths.iter_mut().take(280).skip(256) {
        *v = 7;
    }
    for v in lit_lengths.iter_mut().take(288).skip(280) {
        *v = 8;
    }
    let dist_lengths = vec![5u8; 32];
    Ok((
        Huffman::from_lengths(&lit_lengths)?,
        Huffman::from_lengths(&dist_lengths)?,
    ))
}

fn dynamic_tables(bits: &mut BitReader<'_>) -> Result<(Huffman, Huffman), InflateError> {
    let hlit = bits.read_bits(5)? as usize + 257;
    let hdist = bits.read_bits(5)? as usize + 1;
    let hclen = bits.read_bits(4)? as usize + 4;
    let mut code_len_lengths = [0u8; 19];
    for &symbol in CODE_LENGTH_ORDER.iter().take(hclen) {
        code_len_lengths[symbol] = bits.read_bits(3)? as u8;
    }
    let code_len_huff = Huffman::from_lengths(&code_len_lengths)?;
    let total = hlit
        .checked_add(hdist)
        .ok_or(InflateError::InvalidCodeLength)?;
    let mut lengths = Vec::with_capacity(total);
    while lengths.len() < total {
        let sym = code_len_huff.decode(bits)?;
        match sym {
            0..=15 => lengths.push(sym as u8),
            16 => {
                let prev = *lengths.last().ok_or(InflateError::InvalidCodeLength)?;
                let repeat = bits.read_bits(2)? as usize + 3;
                if lengths.len() + repeat > total {
                    return Err(InflateError::InvalidCodeLength);
                }
                for _ in 0..repeat {
                    lengths.push(prev);
                }
            }
            17 => {
                let repeat = bits.read_bits(3)? as usize + 3;
                if lengths.len() + repeat > total {
                    return Err(InflateError::InvalidCodeLength);
                }
                for _ in 0..repeat {
                    lengths.push(0);
                }
            }
            18 => {
                let repeat = bits.read_bits(7)? as usize + 11;
                if lengths.len() + repeat > total {
                    return Err(InflateError::InvalidCodeLength);
                }
                for _ in 0..repeat {
                    lengths.push(0);
                }
            }
            _ => return Err(InflateError::InvalidCodeLength),
        }
    }
    let lit = Huffman::from_lengths(&lengths[..hlit])?;
    let dist = Huffman::from_lengths_allow_empty(&lengths[hlit..])?;
    Ok((lit, dist))
}

struct BitReader<'a> {
    data: &'a [u8],
    byte_pos: usize,
    bit_pos: u8,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte_pos: 0,
            bit_pos: 0,
        }
    }

    fn read_bits(&mut self, count: u8) -> Result<u32, InflateError> {
        let mut value = 0u32;
        for i in 0..count {
            let byte = *self
                .data
                .get(self.byte_pos)
                .ok_or(InflateError::Truncated)?;
            let bit = (byte >> self.bit_pos) & 1;
            value |= (bit as u32) << i;
            self.bit_pos += 1;
            if self.bit_pos == 8 {
                self.bit_pos = 0;
                self.byte_pos += 1;
            }
        }
        Ok(value)
    }

    fn align_byte(&mut self) {
        if self.bit_pos != 0 {
            self.bit_pos = 0;
            self.byte_pos += 1;
        }
    }

    fn read_u16_le(&mut self) -> Result<u16, InflateError> {
        if self.bit_pos != 0 {
            return Err(InflateError::InvalidStoredBlock);
        }
        let bytes = self.read_bytes(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn read_bytes(&mut self, len: usize) -> Result<&'a [u8], InflateError> {
        if self.bit_pos != 0 {
            return Err(InflateError::InvalidStoredBlock);
        }
        let end = self
            .byte_pos
            .checked_add(len)
            .ok_or(InflateError::Truncated)?;
        if end > self.data.len() {
            return Err(InflateError::Truncated);
        }
        let bytes = &self.data[self.byte_pos..end];
        self.byte_pos = end;
        Ok(bytes)
    }
}

#[derive(Clone)]
struct Huffman {
    tables: Vec<Vec<u16>>,
    max_len: u8,
}

impl Huffman {
    fn from_lengths(lengths: &[u8]) -> Result<Self, InflateError> {
        Self::from_lengths_inner(lengths, false)
    }

    fn from_lengths_allow_empty(lengths: &[u8]) -> Result<Self, InflateError> {
        Self::from_lengths_inner(lengths, true)
    }

    fn from_lengths_inner(lengths: &[u8], allow_empty: bool) -> Result<Self, InflateError> {
        let max_len = lengths.iter().copied().max().unwrap_or(0);
        if max_len == 0 {
            if allow_empty {
                return Ok(Self {
                    tables: Vec::new(),
                    max_len: 0,
                });
            }
            return Err(InflateError::InvalidHuffmanCode);
        }
        if max_len > 15 {
            return Err(InflateError::InvalidHuffmanCode);
        }

        let mut bl_count = [0u16; 16];
        for &len in lengths {
            if len > 15 {
                return Err(InflateError::InvalidHuffmanCode);
            }
            if len != 0 {
                bl_count[len as usize] += 1;
            }
        }
        let mut left = 1i32;
        for &count in bl_count.iter().skip(1) {
            left = (left << 1) - count as i32;
            if left < 0 {
                return Err(InflateError::InvalidHuffmanCode);
            }
        }

        let mut code = 0u16;
        let mut next_code = [0u16; 16];
        for bits in 1..=15 {
            code = (code + bl_count[bits - 1]) << 1;
            next_code[bits] = code;
        }

        let mut tables = Vec::with_capacity(max_len as usize + 1);
        tables.push(Vec::new());
        for len in 1..=max_len {
            tables.push(vec![u16::MAX; 1usize << len]);
        }
        for (symbol, &len) in lengths.iter().enumerate() {
            if len == 0 {
                continue;
            }
            let code_for_symbol = next_code[len as usize];
            next_code[len as usize] = next_code[len as usize].saturating_add(1);
            let reversed = reverse_bits(code_for_symbol, len) as usize;
            tables[len as usize][reversed] = symbol as u16;
        }
        Ok(Self { tables, max_len })
    }

    fn decode(&self, bits: &mut BitReader<'_>) -> Result<u16, InflateError> {
        let mut code = 0u16;
        for len in 1..=self.max_len {
            let bit = bits.read_bits(1)? as u16;
            code |= bit << (len - 1);
            let symbol = self.tables[len as usize][code as usize];
            if symbol != u16::MAX {
                return Ok(symbol);
            }
        }
        Err(InflateError::InvalidHuffmanCode)
    }
}

const CODE_LENGTH_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

fn read_be_u16(bytes: &[u8]) -> u16 {
    u16::from_be_bytes([bytes[0], bytes[1]])
}

fn read_be_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn is_critical(chunk_type: [u8; 4]) -> bool {
    chunk_type[0].is_ascii_uppercase()
}

fn chunk_name(chunk: [u8; 4]) -> String {
    String::from_utf8_lossy(&chunk).into_owned()
}

fn reverse_bits(mut code: u16, len: u8) -> u16 {
    let mut out = 0u16;
    for _ in 0..len {
        out = (out << 1) | (code & 1);
        code >>= 1;
    }
    out
}

fn adler32(bytes: &[u8]) -> u32 {
    const MOD: u32 = 65_521;
    let mut a = 1u32;
    let mut b = 0u32;
    for &byte in bytes {
        a = (a + byte as u32) % MOD;
        b = (b + a) % MOD;
    }
    (b << 16) | a
}

fn crc32_chunk(chunk_type: [u8; 4], data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for byte in chunk_type {
        crc = crc32_update(crc, byte);
    }
    for &byte in data {
        crc = crc32_update(crc, byte);
    }
    !crc
}

fn crc32_update(mut crc: u32, byte: u8) -> u32 {
    crc ^= u32::from(byte);
    for _ in 0..8 {
        if crc & 1 != 0 {
            crc = (crc >> 1) ^ 0xEDB8_8320;
        } else {
            crc >>= 1;
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_rgba_uncompressed() {
        let png = make_png(
            2,
            1,
            8,
            6,
            0,
            &[],
            &[],
            &[0, 255, 0, 0, 255, 0, 0, 255, 128],
        );
        let image = decode_png(&png, DecodeOptions::default()).expect("decode");
        assert_eq!(image.width, 2);
        assert_eq!(image.height, 1);
        assert_eq!(image.pixels[0], Color32::new(255, 0, 0, 255));
        assert_eq!(image.pixels[1], Color32::new(0, 0, 255, 128));
    }

    #[test]
    fn decodes_rgb_fixed_huffman() {
        let raw = [0, 10, 20, 30];
        let png = make_png_with_idat(1, 1, 8, 2, 0, &[], &[], &zlib_fixed(&raw));
        let image = decode_png(&png, DecodeOptions::default()).expect("decode");
        assert_eq!(image.pixels[0], Color32::rgb(10, 20, 30));
    }

    #[test]
    fn decodes_grayscale_low_bit_depth() {
        let png = make_png(4, 1, 2, 0, 0, &[], &[], &[0, 0b0001_1011]);
        let image = decode_png(&png, DecodeOptions::default()).expect("decode");
        assert_eq!(image.pixels[0], Color32::rgb(0, 0, 0));
        assert_eq!(image.pixels[1], Color32::rgb(85, 85, 85));
        assert_eq!(image.pixels[2], Color32::rgb(170, 170, 170));
        assert_eq!(image.pixels[3], Color32::rgb(255, 255, 255));
    }

    #[test]
    fn decodes_palette_with_trns() {
        let palette = [255, 0, 0, 0, 255, 0];
        let trns = [255, 32];
        let png = make_png(2, 1, 8, 3, 0, &palette, &trns, &[0, 0, 1]);
        let image = decode_png(&png, DecodeOptions::default()).expect("decode");
        assert_eq!(image.pixels[0], Color32::new(255, 0, 0, 255));
        assert_eq!(image.pixels[1], Color32::new(0, 255, 0, 32));
    }

    #[test]
    fn decodes_16_bit_rgba() {
        let raw = [0, 0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0];
        let png = make_png(1, 1, 16, 6, 0, &[], &[], &raw);
        let image = decode_png(&png, DecodeOptions::default()).expect("decode");
        assert_eq!(image.pixels[0], Color32::new(0x12, 0x56, 0x9a, 0xde));
    }

    #[test]
    fn decodes_interlaced_single_pixel() {
        let png = make_png(1, 1, 8, 6, 1, &[], &[], &[0, 1, 2, 3, 4]);
        let image = decode_png(&png, DecodeOptions::default()).expect("decode");
        assert_eq!(image.pixels[0], Color32::new(1, 2, 3, 4));
    }

    #[test]
    fn decodes_bundled_palette_wallpaper() {
        let bytes = std::fs::read("../assets/logo.png").expect("asset");
        let image = decode_png(&bytes, DecodeOptions::default()).expect("decode bundled logo");
        assert_eq!(image.width, 73);
        assert_eq!(image.height, 18);
        assert_eq!(image.pixels.len(), 73 * 18);
    }

    #[test]
    fn rejects_bad_signature() {
        let err = decode_png(b"nope", DecodeOptions::default()).unwrap_err();
        assert!(matches!(err, PngError::BadSignature));
    }

    #[test]
    fn rejects_bad_crc() {
        let mut png = make_png(1, 1, 8, 6, 0, &[], &[], &[0, 0, 0, 0, 0]);
        let len = read_be_u32(&png[8..12]) as usize;
        let crc_pos = 8 + 8 + len;
        png[crc_pos] ^= 0x55;
        let err = decode_png(&png, DecodeOptions::default()).unwrap_err();
        assert!(matches!(err, PngError::CrcMismatch { .. }));
    }

    #[test]
    fn rejects_invalid_filter() {
        let png = make_png(1, 1, 8, 6, 0, &[], &[], &[9, 0, 0, 0, 0]);
        let err = decode_png(&png, DecodeOptions::default()).unwrap_err();
        assert!(matches!(err, PngError::InvalidFilter(9)));
    }

    #[test]
    fn rejects_bad_zlib_header() {
        let bad_zlib = [0, 0, 0, 0, 0, 0];
        let png = make_png_with_idat(1, 1, 8, 6, 0, &[], &[], &bad_zlib);
        let err = decode_png(&png, DecodeOptions::default()).unwrap_err();
        assert!(matches!(
            err,
            PngError::Inflate(InflateError::InvalidZlibHeader)
        ));
    }

    #[test]
    fn rejects_invalid_backref() {
        let mut z = vec![0x78, 0x01];
        z.extend_from_slice(&[0x03, 0x00]);
        z.extend_from_slice(&0u32.to_be_bytes());
        let png = make_png_with_idat(1, 1, 8, 6, 0, &[], &[], &z);
        let err = decode_png(&png, DecodeOptions::default()).unwrap_err();
        assert!(matches!(err, PngError::Inflate(_)));
    }

    #[test]
    fn rejects_truncated_chunk() {
        let mut png = make_png(1, 1, 8, 6, 0, &[], &[], &[0, 0, 0, 0, 0]);
        png.truncate(png.len() - 3);
        let err = decode_png(&png, DecodeOptions::default()).unwrap_err();
        assert!(matches!(err, PngError::Truncated));
    }

    #[test]
    fn rejects_resource_limit() {
        let png = make_png(2, 2, 8, 6, 0, &[], &[], &[0; 18]);
        let err = decode_png(
            &png,
            DecodeOptions {
                max_pixels: 3,
                validate_crc: true,
            },
        )
        .unwrap_err();
        assert!(matches!(err, PngError::ResourceLimitExceeded));
    }

    #[test]
    fn rejects_idat_length_mismatch() {
        let png = make_png(1, 1, 8, 6, 0, &[], &[], &[0, 1, 2, 3]);
        let err = decode_png(&png, DecodeOptions::default()).unwrap_err();
        assert!(matches!(err, PngError::IdatLengthMismatch { .. }));
    }

    fn make_png(
        width: u32,
        height: u32,
        bit_depth: u8,
        color_type: u8,
        interlace: u8,
        palette: &[u8],
        trns: &[u8],
        raw: &[u8],
    ) -> Vec<u8> {
        make_png_with_idat(
            width,
            height,
            bit_depth,
            color_type,
            interlace,
            palette,
            trns,
            &zlib_stored(raw),
        )
    }

    fn make_png_with_idat(
        width: u32,
        height: u32,
        bit_depth: u8,
        color_type: u8,
        interlace: u8,
        palette: &[u8],
        trns: &[u8],
        idat: &[u8],
    ) -> Vec<u8> {
        let mut png = Vec::new();
        png.extend_from_slice(PNG_SIGNATURE);
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&width.to_be_bytes());
        ihdr.extend_from_slice(&height.to_be_bytes());
        ihdr.extend_from_slice(&[bit_depth, color_type, 0, 0, interlace]);
        push_chunk(&mut png, *b"IHDR", &ihdr);
        if !palette.is_empty() {
            push_chunk(&mut png, *b"PLTE", palette);
        }
        if !trns.is_empty() {
            push_chunk(&mut png, *b"tRNS", trns);
        }
        push_chunk(&mut png, *b"IDAT", idat);
        push_chunk(&mut png, *b"IEND", &[]);
        png
    }

    fn push_chunk(png: &mut Vec<u8>, kind: [u8; 4], data: &[u8]) {
        png.extend_from_slice(&(data.len() as u32).to_be_bytes());
        png.extend_from_slice(&kind);
        png.extend_from_slice(data);
        png.extend_from_slice(&crc32_chunk(kind, data).to_be_bytes());
    }

    fn zlib_stored(raw: &[u8]) -> Vec<u8> {
        assert!(u16::try_from(raw.len()).is_ok());
        let len = raw.len() as u16;
        let mut out = vec![0x78, 0x01, 0x01];
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(raw);
        out.extend_from_slice(&adler32(raw).to_be_bytes());
        out
    }

    fn zlib_fixed(raw: &[u8]) -> Vec<u8> {
        let mut writer = BitWriter::new();
        writer.write_bits(1, 1);
        writer.write_bits(0b01, 2);
        for &byte in raw {
            let (code, len) = fixed_lit_code(byte as u16);
            writer.write_bits(reverse_bits(code, len) as u32, len);
        }
        let (end, end_len) = fixed_lit_code(256);
        writer.write_bits(reverse_bits(end, end_len) as u32, end_len);
        let mut out = vec![0x78, 0x01];
        out.extend(writer.finish());
        out.extend_from_slice(&adler32(raw).to_be_bytes());
        out
    }

    fn fixed_lit_code(symbol: u16) -> (u16, u8) {
        match symbol {
            0..=143 => (0x30 + symbol, 8),
            144..=255 => (0x190 + symbol - 144, 9),
            256..=279 => (symbol - 256, 7),
            280..=287 => (0xC0 + symbol - 280, 8),
            _ => unreachable!(),
        }
    }

    struct BitWriter {
        bytes: Vec<u8>,
        current: u8,
        bits: u8,
    }

    impl BitWriter {
        fn new() -> Self {
            Self {
                bytes: Vec::new(),
                current: 0,
                bits: 0,
            }
        }

        fn write_bits(&mut self, value: u32, count: u8) {
            for i in 0..count {
                let bit = ((value >> i) & 1) as u8;
                self.current |= bit << self.bits;
                self.bits += 1;
                if self.bits == 8 {
                    self.bytes.push(self.current);
                    self.current = 0;
                    self.bits = 0;
                }
            }
        }

        fn finish(mut self) -> Vec<u8> {
            if self.bits != 0 {
                self.bytes.push(self.current);
            }
            self.bytes
        }
    }
}
