/// BGRA (4 bytes/pixel, row-major) → planar I420 (Y + U + V, 4:2:0 subsampled).
/// Output layout: Y plane (w×h) | U plane (w/2 × h/2) | V plane (w/2 × h/2).
/// Total size = w * h * 3 / 2, which is what openh264's YUVBuffer::from_vec expects.
pub fn bgra_to_i420(bgra: &[u8], width: usize, height: usize) -> Vec<u8> {
    let pixels = width * height;
    let mut out = vec![0u8; pixels * 3 / 2];

    let u_off = pixels;
    let v_off = pixels + pixels / 4;

    for row in 0..height {
        for col in 0..width {
            let s = (row * width + col) * 4;
            let b = bgra[s] as i32;
            let g = bgra[s + 1] as i32;
            let r = bgra[s + 2] as i32;

            // BT.601 limited-range integer coefficients
            out[row * width + col] =
                (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16).clamp(16, 235) as u8;

            if row % 2 == 0 && col % 2 == 0 {
                let uv = (row / 2) * (width / 2) + col / 2;
                out[u_off + uv] =
                    (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128).clamp(16, 240) as u8;
                out[v_off + uv] =
                    (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128).clamp(16, 240) as u8;
            }
        }
    }
    out
}
