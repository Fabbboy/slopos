//! Minimal TrueType font parser.
//!
//! Parses the subset of TTF tables needed for basic Latin text rendering:
//! `head`, `maxp`, `cmap`, `hhea`, `hmtx`, `loca`, `glyf`.

use slopos_ostd::KVec;

pub struct TtfFont<'a> {
    data: &'a [u8],
    head: HeadTable,
    maxp: MaxpTable,
    cmap_offset: usize,
    hhea: HheaTable,
    hmtx_offset: usize,
    loca_offset: usize,
    glyf_offset: usize,
}

#[derive(Clone, Copy)]
struct HeadTable {
    units_per_em: u16,
    index_to_loc_format: i16, // 0 = short, 1 = long
}

#[derive(Clone, Copy)]
struct MaxpTable {
    num_glyphs: u16,
}

#[derive(Clone, Copy)]
pub struct HheaTable {
    pub ascender: i16,
    pub descender: i16,
    pub line_gap: i16,
    pub number_of_h_metrics: u16,
}

#[derive(Clone, Copy, Debug)]
pub struct OutlinePoint {
    pub x: i16,
    pub y: i16,
    pub on_curve: bool,
}

/// A closed loop of outline points.
#[derive(Clone, Debug)]
pub struct Contour {
    pub points: KVec<OutlinePoint>,
}

#[derive(Clone, Debug)]
pub struct GlyphOutline {
    pub contours: KVec<Contour>,
    pub x_min: i16,
    pub y_min: i16,
    pub x_max: i16,
    pub y_max: i16,
}

#[derive(Clone, Copy, Debug)]
pub struct HMetrics {
    pub advance_width: u16,
    pub left_side_bearing: i16,
}

fn read_u8(data: &[u8], offset: usize) -> Option<u8> {
    data.get(offset).copied()
}

fn read_u16(data: &[u8], offset: usize) -> Option<u16> {
    if offset + 2 > data.len() {
        return None;
    }
    Some(u16::from_be_bytes([data[offset], data[offset + 1]]))
}

fn read_i16(data: &[u8], offset: usize) -> Option<i16> {
    read_u16(data, offset).map(|v| v as i16)
}

fn read_u32(data: &[u8], offset: usize) -> Option<u32> {
    if offset + 4 > data.len() {
        return None;
    }
    Some(u32::from_be_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ]))
}

/// Find a table in the TrueType offset table; returns `(offset, length)`.
fn find_table(data: &[u8], tag: &[u8; 4]) -> Option<(usize, usize)> {
    let num_tables = read_u16(data, 4)? as usize;
    for i in 0..num_tables {
        let record_offset = 12 + i * 16;
        if record_offset + 16 > data.len() {
            break;
        }
        if &data[record_offset..record_offset + 4] == tag {
            let offset = read_u32(data, record_offset + 8)? as usize;
            let length = read_u32(data, record_offset + 12)? as usize;
            return Some((offset, length));
        }
    }
    None
}

impl<'a> TtfFont<'a> {
    pub fn parse(data: &'a [u8]) -> Option<Self> {
        if data.len() < 12 {
            return None;
        }

        // sfVersion: 0x00010000 (TrueType) or 'true'.
        let sf_version = read_u32(data, 0)?;
        if sf_version != 0x00010000 && sf_version != 0x74727565 {
            return None;
        }

        let (head_off, _) = find_table(data, b"head")?;
        let head = HeadTable {
            units_per_em: read_u16(data, head_off + 18)?,
            index_to_loc_format: read_i16(data, head_off + 50)?,
        };
        if head.units_per_em == 0 {
            return None;
        }

        let (maxp_off, _) = find_table(data, b"maxp")?;
        let maxp = MaxpTable {
            num_glyphs: read_u16(data, maxp_off + 4)?,
        };

        let (cmap_offset, _) = find_table(data, b"cmap")?;

        let (hhea_off, _) = find_table(data, b"hhea")?;
        let hhea = HheaTable {
            ascender: read_i16(data, hhea_off + 4)?,
            descender: read_i16(data, hhea_off + 6)?,
            line_gap: read_i16(data, hhea_off + 8)?,
            number_of_h_metrics: read_u16(data, hhea_off + 34)?,
        };

        let (hmtx_offset, _) = find_table(data, b"hmtx")?;
        let (loca_offset, _) = find_table(data, b"loca")?;
        let (glyf_offset, _) = find_table(data, b"glyf")?;

        Some(Self {
            data,
            head,
            maxp,
            cmap_offset,
            hhea,
            hmtx_offset,
            loca_offset,
            glyf_offset,
        })
    }

    pub fn units_per_em(&self) -> u16 {
        self.head.units_per_em
    }

    pub fn hhea(&self) -> &HheaTable {
        &self.hhea
    }

    /// Only Format 4 (BMP) cmap subtables are supported.
    pub fn glyph_index(&self, codepoint: u32) -> Option<u16> {
        if codepoint > 0xFFFF {
            return None;
        }
        let cp = codepoint as u16;
        let data = self.data;
        let cmap_off = self.cmap_offset;

        let num_subtables = read_u16(data, cmap_off + 2)? as usize;

        // platform 0 = Unicode; platform 3 / encoding 1 or 10 = Windows Unicode.
        for i in 0..num_subtables {
            let record = cmap_off + 4 + i * 8;
            let platform_id = read_u16(data, record)?;
            let encoding_id = read_u16(data, record + 2)?;
            let subtable_offset = read_u32(data, record + 4)? as usize;

            let is_unicode =
                platform_id == 0 || (platform_id == 3 && (encoding_id == 1 || encoding_id == 10));

            if !is_unicode {
                continue;
            }

            let sub = cmap_off + subtable_offset;
            let format = read_u16(data, sub)?;

            if format == 4 {
                return self.cmap_format4_lookup(sub, cp);
            }
        }

        None
    }

    fn cmap_format4_lookup(&self, sub: usize, cp: u16) -> Option<u16> {
        let data = self.data;
        let seg_count = (read_u16(data, sub + 6)? / 2) as usize;

        let end_code_base = sub + 14;
        let start_code_base = end_code_base + seg_count * 2 + 2; // +2 for reservedPad
        let id_delta_base = start_code_base + seg_count * 2;
        let id_range_offset_base = id_delta_base + seg_count * 2;

        for seg in 0..seg_count {
            let end_code = read_u16(data, end_code_base + seg * 2)?;
            if cp > end_code {
                continue;
            }

            let start_code = read_u16(data, start_code_base + seg * 2)?;
            if cp < start_code {
                return Some(0); // .notdef
            }

            let id_delta = read_i16(data, id_delta_base + seg * 2)? as i32;
            let id_range_offset = read_u16(data, id_range_offset_base + seg * 2)?;

            let glyph_id = if id_range_offset == 0 {
                ((cp as i32 + id_delta) & 0xFFFF) as u16
            } else {
                let glyph_id_offset = id_range_offset_base
                    + seg * 2
                    + id_range_offset as usize
                    + (cp as usize - start_code as usize) * 2;
                let gid = read_u16(data, glyph_id_offset)?;
                if gid == 0 {
                    0
                } else {
                    ((gid as i32 + id_delta) & 0xFFFF) as u16
                }
            };

            return Some(glyph_id);
        }

        Some(0)
    }

    pub fn h_metrics(&self, glyph_id: u16) -> Option<HMetrics> {
        let data = self.data;
        let num_h_metrics = self.hhea.number_of_h_metrics as usize;
        let hmtx = self.hmtx_offset;

        if (glyph_id as usize) < num_h_metrics {
            let off = hmtx + (glyph_id as usize) * 4;
            Some(HMetrics {
                advance_width: read_u16(data, off)?,
                left_side_bearing: read_i16(data, off + 2)?,
            })
        } else {
            // Past numberOfHMetrics hmtx stores lsb only; the advance width is
            // the last recorded one.
            let last_aw_off = hmtx + (num_h_metrics - 1) * 4;
            let advance_width = read_u16(data, last_aw_off)?;
            let lsb_index = (glyph_id as usize) - num_h_metrics;
            let lsb_off = hmtx + num_h_metrics * 4 + lsb_index * 2;
            let left_side_bearing = read_i16(data, lsb_off)?;
            Some(HMetrics {
                advance_width,
                left_side_bearing,
            })
        }
    }

    /// `(offset, length)` of a glyph within the glyf table.
    fn glyph_offset(&self, glyph_id: u16) -> Option<(usize, usize)> {
        let data = self.data;
        let loca = self.loca_offset;
        let gid = glyph_id as usize;

        if gid >= self.maxp.num_glyphs as usize {
            return None;
        }

        let (off0, off1) = if self.head.index_to_loc_format == 0 {
            // Short loca stores offsets halved.
            let o0 = read_u16(data, loca + gid * 2)? as usize * 2;
            let o1 = read_u16(data, loca + (gid + 1) * 2)? as usize * 2;
            (o0, o1)
        } else {
            let o0 = read_u32(data, loca + gid * 4)? as usize;
            let o1 = read_u32(data, loca + (gid + 1) * 4)? as usize;
            (o0, o1)
        };

        if off0 == off1 {
            return None; // Empty glyph (e.g., space)
        }

        Some((self.glyf_offset + off0, off1 - off0))
    }

    pub fn glyph_outline(&self, glyph_id: u16) -> Option<GlyphOutline> {
        let (glyph_off, _glyph_len) = self.glyph_offset(glyph_id)?;
        let data = self.data;

        let num_contours = read_i16(data, glyph_off)?;
        let x_min = read_i16(data, glyph_off + 2)?;
        let y_min = read_i16(data, glyph_off + 4)?;
        let x_max = read_i16(data, glyph_off + 6)?;
        let y_max = read_i16(data, glyph_off + 8)?;

        if num_contours < 0 {
            // Compound glyph
            return self.parse_compound_glyph(glyph_off, x_min, y_min, x_max, y_max);
        }

        let nc = num_contours as usize;
        if nc == 0 {
            return Some(GlyphOutline {
                contours: KVec::new(),
                x_min,
                y_min,
                x_max,
                y_max,
            });
        }

        let mut end_pts: KVec<u16> = KVec::with_capacity(nc).ok()?;
        for i in 0..nc {
            end_pts.push(read_u16(data, glyph_off + 10 + i * 2)?).ok()?;
        }

        let last_point = *end_pts.last()? as usize;
        let num_points = last_point + 1;

        let instr_len_off = glyph_off + 10 + nc * 2;
        let instr_len = read_u16(data, instr_len_off)? as usize;
        let flags_off = instr_len_off + 2 + instr_len;

        let mut flags: KVec<u8> = KVec::with_capacity(num_points).ok()?;
        let mut pos = flags_off;
        while flags.len() < num_points {
            let flag = read_u8(data, pos)?;
            pos += 1;
            flags.push(flag).ok()?;
            if flag & 0x08 != 0 {
                // REPEAT
                let repeat = read_u8(data, pos)? as usize;
                pos += 1;
                for _ in 0..repeat {
                    flags.push(flag).ok()?;
                    if flags.len() >= num_points {
                        break;
                    }
                }
            }
        }

        let mut x_coords: KVec<i16> = KVec::with_capacity(num_points).ok()?;
        let mut x: i16 = 0;
        for &flag in &flags[..num_points] {
            let x_short = flag & 0x02 != 0;
            let x_same_or_positive = flag & 0x10 != 0;

            if x_short {
                let dx = read_u8(data, pos)? as i16;
                pos += 1;
                x += if x_same_or_positive { dx } else { -dx };
            } else if !x_same_or_positive {
                let dx = read_i16(data, pos)?;
                pos += 2;
                x += dx;
            }
            // else: x_same_or_positive && !x_short => same as previous
            x_coords.push(x).ok()?;
        }

        let mut y_coords: KVec<i16> = KVec::with_capacity(num_points).ok()?;
        let mut y: i16 = 0;
        for &flag in &flags[..num_points] {
            let y_short = flag & 0x04 != 0;
            let y_same_or_positive = flag & 0x20 != 0;

            if y_short {
                let dy = read_u8(data, pos)? as i16;
                pos += 1;
                y += if y_same_or_positive { dy } else { -dy };
            } else if !y_same_or_positive {
                let dy = read_i16(data, pos)?;
                pos += 2;
                y += dy;
            }
            y_coords.push(y).ok()?;
        }

        let mut contours: KVec<Contour> = KVec::with_capacity(nc).ok()?;
        let mut start = 0usize;
        for &end in &end_pts {
            let end = end as usize;
            if end >= num_points {
                break;
            }
            let mut points: KVec<OutlinePoint> = KVec::with_capacity(end - start + 1).ok()?;
            for i in start..=end {
                points
                    .push(OutlinePoint {
                        x: x_coords[i],
                        y: y_coords[i],
                        on_curve: flags[i] & 0x01 != 0,
                    })
                    .ok()?;
            }
            contours.push(Contour { points }).ok()?;
            start = end + 1;
        }

        Some(GlyphOutline {
            contours,
            x_min,
            y_min,
            x_max,
            y_max,
        })
    }

    /// A compound glyph is built from references to other glyphs.
    fn parse_compound_glyph(
        &self,
        glyph_off: usize,
        x_min: i16,
        y_min: i16,
        x_max: i16,
        y_max: i16,
    ) -> Option<GlyphOutline> {
        let data = self.data;
        let mut pos = glyph_off + 10; // skip header
        let mut all_contours: KVec<Contour> = KVec::new();

        loop {
            let flags = read_u16(data, pos)?;
            let component_glyph_id = read_u16(data, pos + 2)?;
            pos += 4;

            let (dx, dy) = if flags & 0x0001 != 0 {
                // ARG_1_AND_2_ARE_WORDS
                let dx = read_i16(data, pos)?;
                let dy = read_i16(data, pos + 2)?;
                pos += 4;
                (dx as i32, dy as i32)
            } else {
                let dx = read_u8(data, pos)? as i8 as i32;
                let dy = read_u8(data, pos + 1)? as i8 as i32;
                pos += 2;
                (dx, dy)
            };

            if flags & 0x0008 != 0 {
                // WE_HAVE_A_SCALE
                pos += 2;
            } else if flags & 0x0040 != 0 {
                // WE_HAVE_AN_X_AND_Y_SCALE
                pos += 4;
            } else if flags & 0x0080 != 0 {
                // WE_HAVE_A_TWO_BY_TWO
                pos += 8;
            }

            if let Some(component) = self.glyph_outline(component_glyph_id) {
                for contour in component.contours {
                    let translated_points: KVec<OutlinePoint> =
                        KVec::from_iter_fallible(contour.points.iter().map(|p| OutlinePoint {
                            x: (p.x as i32 + dx) as i16,
                            y: (p.y as i32 + dy) as i16,
                            on_curve: p.on_curve,
                        }))
                        .ok()?;
                    all_contours
                        .push(Contour {
                            points: translated_points,
                        })
                        .ok()?;
                }
            }

            if flags & 0x0020 == 0 {
                // MORE_COMPONENTS
                break;
            }
        }

        Some(GlyphOutline {
            contours: all_contours,
            x_min,
            y_min,
            x_max,
            y_max,
        })
    }

    pub fn num_glyphs(&self) -> u16 {
        self.maxp.num_glyphs
    }
}
