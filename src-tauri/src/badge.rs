//! Tiny PNG renderer for the Windows taskbar overlay badge.
//!
//! The Windows overlay icon has to be an `Image`, which is decoded from PNG
//! bytes by `tauri` (`image-png` feature). Instead of pulling in an image
//! encoding crate we emit an uncompressed RGBA PNG (stored deflate blocks):
//! it is a few hundred bytes and keeps the dependency graph small.

const SIZE: u32 = 32;

/// Render a red circular badge with the unread count (`9+` above nine).
///
/// A count of zero yields a transparent image, which removes the overlay.
pub fn overlay_png(count: u32) -> Vec<u8> {
    let mut pixels = vec![0u8; (SIZE * SIZE * 4) as usize];
    if count == 0 {
        return encode_png(&pixels);
    }

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
    encode_png(&pixels)
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
    const SCALE: u32 = 3;
    const GLYPH_W: u32 = 5;
    const GLYPH_H: u32 = 7;
    const GAP: u32 = 1;

    let glyphs: Vec<[&'static str; 7]> = text.chars().filter_map(glyph).collect();
    if glyphs.is_empty() {
        return;
    }
    let total_w = glyphs.len() as u32 * GLYPH_W * SCALE + (glyphs.len() as u32 - 1) * GAP * SCALE;
    let total_h = GLYPH_H * SCALE;
    let start_x = (SIZE.saturating_sub(total_w)) / 2;
    let start_y = (SIZE.saturating_sub(total_h)) / 2;

    let mut x_offset = 0u32;
    for glyph in &glyphs {
        for (row, line) in glyph.iter().enumerate() {
            for (col, ch) in line.chars().enumerate() {
                if ch != '#' {
                    continue;
                }
                for dy in 0..SCALE {
                    for dx in 0..SCALE {
                        let x = start_x + x_offset + col as u32 * SCALE + dx;
                        let y = start_y + row as u32 * SCALE + dy;
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
        x_offset += (GLYPH_W + GAP) * SCALE;
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
        let bytes = overlay_png(3);
        assert_eq!(
            &bytes[..8],
            &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]
        );
        assert!(bytes.len() > 100);
        assert!(bytes.ends_with(&[0xAE, 0x42, 0x60, 0x82]), "IEND crc");
    }

    #[test]
    fn zero_count_is_transparent_but_valid() {
        let bytes = overlay_png(0);
        assert_eq!(
            &bytes[..8],
            &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]
        );
    }

    #[test]
    fn text_is_drawn_for_large_counts() {
        let bytes = overlay_png(42);
        assert!(!bytes.is_empty());
        let bytes = overlay_png(150);
        assert!(!bytes.is_empty());
    }
}
