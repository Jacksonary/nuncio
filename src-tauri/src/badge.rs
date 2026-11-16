//! Tiny PNG renderer for the Windows taskbar overlay badge.
//!
//! The Windows overlay icon has to be an `Image`, which is decoded from PNG
//! bytes by `tauri` (`image-png` feature). Instead of pulling in an image
//! encoding crate we emit an uncompressed RGBA PNG (stored deflate blocks):
//! it is a few hundred bytes and keeps the dependency graph small.

/// Badge canvas.
///
/// 32px was too small: Windows scales the overlay icon, and at 32 the two-glyph
/// text ("9+", "12") already needs 33px of horizontal room, so it was clipped
/// on the right and the remaining glyphs landed on the edge of the circle.
/// 64 gives the text room at 100% and enough source pixels that the scaling to
/// the taskbar size stays sharp on high-DPI displays.
const SIZE: u32 = 64;

/// Render a red circular badge with the unread count (`9+` above nine).
///
/// Returns `None` when there is nothing to show. An all-transparent image is
/// NOT equivalent to "no badge": Windows still draws an overlay slot for it,
/// and on some compositor paths the transparent surface leaves artifacts
/// (black banding) over the taskbar icon. The caller must pass `None` to
/// `set_overlay_icon` so the overlay is actually removed.
pub fn overlay_png(count: u32) -> Option<Vec<u8>> {
    if count == 0 {
        return None;
    }
    let mut pixels = vec![0u8; (SIZE * SIZE * 4) as usize];

    let center = (SIZE as f32 - 1.0) / 2.0;
    let radius = center - 1.0;
    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let distance = (dx * dx + dy * dy).sqrt();
            // One pixel of anti-aliasing on the rim.
            let alpha = ((radius + 0.75 - distance) * 255.0).clamp(0.0, 255.0) as u8;
            if alpha == 0 {
                continue;
            }
            let index = ((y * SIZE + x) * 4) as usize;
            pixels[index] = 214; // #D62F2F
            pixels[index + 1] = 47;
            pixels[index + 2] = 47;
            pixels[index + 3] = alpha;
        }
    }

    let text = if count > 9 {
        "9+".to_string()
    } else {
        count.to_string()
    };
    draw_text(&mut pixels, &text);
    Some(encode_png(&pixels))
}

/// 5x7 glyphs for digits and `+`; `#` is an opaque white pixel.
fn glyph(ch: char) -> Option<[&'static str; 7]> {
    Some(match ch {
        '0' => [
            " ### ", "#   #", "#  ##", "# # #", "##  #", "#   #", " ### ",
        ],
        '1' => [
            "  #  ", " ##  ", "  #  ", "  #  ", "  #  ", "  #  ", " ### ",
        ],
        '2' => [
            " ### ", "#   #", "    #", "   # ", "  #  ", " #   ", "#####",
        ],
        '3' => [
            "#####", "   # ", "  #  ", "   # ", "    #", "#   #", " ### ",
        ],
        '4' => [
            "   # ", "  ## ", " # # ", "#  # ", "#####", "   # ", "   # ",
        ],
        '5' => [
            "#####", "#    ", "#### ", "    #", "    #", "#   #", " ### ",
        ],
        '6' => [
            "  ## ", " #   ", "#    ", "#### ", "#   #", "#   #", " ### ",
        ],
        '7' => [
            "#####", "    #", "   # ", "  #  ", " #   ", " #   ", " #   ",
        ],
        '8' => [
            " ### ", "#   #", "#   #", " ### ", "#   #", "#   #", " ### ",
        ],
        '9' => [
            " ### ", "#   #", "#   #", " ####", "    #", "   # ", " ##  ",
        ],
        '+' => [
            "     ", "  #  ", "  #  ", "#####", "  #  ", "  #  ", "     ",
        ],
        _ => return None,
    })
}

fn draw_text(pixels: &mut [u8], text: &str) {
    // Scale by glyph count: the circle is round, so a two-glyph run has to stay
    // inside it diagonally. At SIZE=64 a scale of 5 fits one glyph but a
    // two-glyph run ("9+") reaches 32.6px from centre against a 31px radius and
    // spills over the rim.
    const SCALE_SINGLE: u32 = 5;
    const SCALE_PAIR: u32 = 4;
    const GLYPH_W: u32 = 5;
    const GLYPH_H: u32 = 7;
    const GAP: u32 = 1;

    let glyphs: Vec<[&'static str; 7]> = text.chars().filter_map(glyph).collect();
    if glyphs.is_empty() {
        return;
    }
    let scale = if glyphs.len() > 1 { SCALE_PAIR } else { SCALE_SINGLE };
    let total_w = glyphs.len() as u32 * GLYPH_W * scale + (glyphs.len() as u32 - 1) * GAP * scale;
    let total_h = GLYPH_H * scale;
    let start_x = (SIZE.saturating_sub(total_w)) / 2;
    let start_y = (SIZE.saturating_sub(total_h)) / 2;

    let mut x_offset = 0u32;
    for glyph in &glyphs {
        for (row, line) in glyph.iter().enumerate() {
            for (col, ch) in line.chars().enumerate() {
                if ch != '#' {
                    continue;
                }
                for dy in 0..scale {
                    for dx in 0..scale {
                        let x = start_x + x_offset + col as u32 * scale + dx;
                        let y = start_y + row as u32 * scale + dy;
                        if x >= SIZE || y >= SIZE {
                            continue;
                        }
                        let index = ((y * SIZE + x) * 4) as usize;
                        pixels[index] = 255;
                        pixels[index + 1] = 255;
                        pixels[index + 2] = 255;
                        pixels[index + 3] = 255;
                    }
                }
            }
        }
        x_offset += (GLYPH_W + GAP) * scale;
    }
}

fn encode_png(pixels: &[u8]) -> Vec<u8> {
    let stride = (SIZE * 4) as usize;
    let mut raw = Vec::with_capacity((SIZE as usize) * (stride + 1));
    for row in 0..SIZE as usize {
        raw.push(0); // filter type: None
        raw.extend_from_slice(&pixels[row * stride..(row + 1) * stride]);
    }

    let mut out = Vec::new();
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&SIZE.to_be_bytes());
    ihdr.extend_from_slice(&SIZE.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA, deflate, no filter, no interlace
    write_chunk(&mut out, b"IHDR", &ihdr);

    let mut zlib = Vec::with_capacity(raw.len() + 32);
    zlib.extend_from_slice(&[0x78, 0x01]);
    for block in raw.chunks(65_535) {
        let final_block = block.as_ptr_range().end == raw.as_ptr_range().end;
        zlib.push(if final_block { 1 } else { 0 });
        let len = block.len() as u16;
        zlib.extend_from_slice(&len.to_le_bytes());
        zlib.extend_from_slice(&(!len).to_le_bytes());
        zlib.extend_from_slice(block);
    }
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());
    write_chunk(&mut out, b"IDAT", &zlib);
    write_chunk(&mut out, b"IEND", &[]);
    out
}

fn write_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc_input = Vec::with_capacity(kind.len() + data.len());
    crc_input.extend_from_slice(kind);
    crc_input.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

fn adler32(data: &[u8]) -> u32 {
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for byte in data {
        a = (a + *byte as u32) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for byte in data {
        crc ^= *byte as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_has_valid_signature_and_bounded_size() {
        let bytes = overlay_png(3).expect("a count renders a badge");
        assert_eq!(
            &bytes[..8],
            &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]
        );
        assert!(bytes.len() > 100);
        assert!(bytes.ends_with(&[0xAE, 0x42, 0x60, 0x82]), "IEND crc");
    }

    #[test]
    fn zero_count_removes_the_badge_instead_of_drawing_an_empty_one() {
        // A transparent image is not equivalent to "no overlay": the slot stays
        // occupied and the compositor can leave banding over the taskbar icon.
        assert!(overlay_png(0).is_none());
    }

    #[test]
    fn text_is_drawn_for_large_counts() {
        assert!(overlay_png(42).is_some());
        assert!(overlay_png(150).is_some());
    }

    #[test]
    fn two_digit_text_stays_inside_the_circle() {
        // 64px canvas, 31px usable radius. Two glyphs at scale 4 span 44x28,
        // whose corner sits 26.1px from centre -- inside the rim.
        let bytes = overlay_png(12).expect("badge");
        let image = decode_rgba(&bytes);
        let mut max_radius = 0.0f32;
        let center = (SIZE as f32 - 1.0) / 2.0;
        for y in 0..SIZE {
            for x in 0..SIZE {
                let index = ((y * SIZE + x) * 4) as usize;
                if image[index + 3] > 200 {
                    let dx = x as f32 - center;
                    let dy = y as f32 - center;
                    max_radius = max_radius.max((dx * dx + dy * dy).sqrt());
                }
            }
        }
        assert!(
            max_radius <= center,
            "badge content reached {max_radius:.1}px from centre, outside the {center:.1}px radius"
        );
    }

    /// Decode our own stored-deflate PNG back to RGBA so the pixel assertions
    /// above run against the real encoder output rather than a parallel path.
    fn decode_rgba(bytes: &[u8]) -> Vec<u8> {
        // Skip the 8-byte signature, walk chunks, inflate stored IDAT blocks.
        let mut offset = 8;
        let mut raw = Vec::new();
        while offset + 8 <= bytes.len() {
            let length = u32::from_be_bytes([
                bytes[offset],
                bytes[offset + 1],
                bytes[offset + 2],
                bytes[offset + 3],
            ]) as usize;
            let kind = &bytes[offset + 4..offset + 8];
            let data = &bytes[offset + 8..offset + 8 + length];
            if kind == b"IDAT" {
                let mut i = 2; // zlib header
                while i + 5 <= data.len() {
                    let final_block = data[i] == 1;
                    let len = u16::from_le_bytes([data[i + 1], data[i + 2]]) as usize;
                    raw.extend_from_slice(&data[i + 5..i + 5 + len]);
                    i += 5 + len;
                    if final_block {
                        break;
                    }
                }
                break;
            }
            offset += 12 + length;
        }
        // Drop the per-row filter byte.
        let stride = (SIZE * 4) as usize;
        let mut pixels = Vec::with_capacity((SIZE * SIZE * 4) as usize);
        for row in 0..SIZE as usize {
            let start = row * (stride + 1) + 1;
            pixels.extend_from_slice(&raw[start..start + stride]);
        }
        pixels
    }
}
