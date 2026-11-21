//! A tiny PNG encoder for placeholder images (uncompressed deflate, RGB8).

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let mut c = kind.to_vec();
    c.extend_from_slice(data);
    out.extend_from_slice(&c);
    out.extend_from_slice(&crc32(&c).to_be_bytes());
}

/// Encode `rgb` (width * height * 3 bytes) as a PNG.
pub fn encode(width: u32, height: u32, rgb: &[u8]) -> Vec<u8> {
    assert_eq!(rgb.len(), (width * height * 3) as usize);
    let mut raw = Vec::with_capacity(rgb.len() + height as usize);
    for row in rgb.chunks(width as usize * 3) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    let mut z = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(65535).collect();
    for (i, blk) in blocks.iter().enumerate() {
        z.push((i + 1 == blocks.len()) as u8);
        let len = blk.len() as u16;
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(!len).to_le_bytes());
        z.extend_from_slice(blk);
    }
    z.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

/// A placeholder picture: a gradient in colors derived from `seed`, with bands.
pub fn placeholder(width: u32, height: u32, seed: u64) -> Vec<u8> {
    let c = |shift: u32| ((seed >> shift) & 0xff) as f32;
    let (r0, g0, b0) = (c(0), c(8), c(16));
    let (r1, g1, b1) = (c(24), c(32), c(40));
    let bands = 3 + (seed >> 48) % 6;
    let mut rgb = Vec::with_capacity((width * height * 3) as usize);
    for y in 0..height {
        for x in 0..width {
            let t = y as f32 / height.max(1) as f32;
            let band = ((x as u64 * bands) / width.max(1) as u64).is_multiple_of(2);
            let k = if band { 1.0 } else { 0.8 };
            rgb.push(((r0 * (1.0 - t) + r1 * t) * k) as u8);
            rgb.push(((g0 * (1.0 - t) + g1 * t) * k) as u8);
            rgb.push(((b0 * (1.0 - t) + b1 * t) * k) as u8);
        }
    }
    encode(width, height, &rgb)
}
