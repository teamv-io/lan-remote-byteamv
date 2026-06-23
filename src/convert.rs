/// BGRA (4 bytes/pixel, row-major) → planar I420 (Y + U + V, 4:2:0 subsampled)
/// Output layout: Y plane (width*height) then U (w/2 * h/2) then V (w/2 * h/2)
pub fn bgra_to_i420(bgra: &[u8], width: usize, height: usize) -> Vec<u8> {
    let pixels = width * height;
    let mut out = vec![0u8; pixels + pixels / 4 + pixels / 4];

    let u_off = pixels;
    let v_off = pixels + pixels / 4;

    for row in 0..height {
        for col in 0..width {
            let s = (row * width + col) * 4;
            let b = bgra[s] as i32;
            let g = bgra[s + 1] as i32;
            let r = bgra[s + 2] as i32;

            // BT.601 limited range
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

/// Planar I420 (with per-plane strides) → XRGB u32 pixels (0x00RRGGBB) for softbuffer
pub fn i420_strided_to_xrgb(
    y_plane: &[u8],
    y_stride: usize,
    u_plane: &[u8],
    u_stride: usize,
    v_plane: &[u8],
    v_stride: usize,
    width: usize,
    height: usize,
) -> Vec<u32> {
    let mut out = vec![0u32; width * height];

    for row in 0..height {
        for col in 0..width {
            let yv = y_plane[row * y_stride + col] as i32 - 16;
            let uv = u_plane[(row / 2) * u_stride + col / 2] as i32 - 128;
            let vv = v_plane[(row / 2) * v_stride + col / 2] as i32 - 128;

            // BT.601 limited range coefficients (integer approximation, *256 scaled)
            let r = ((298 * yv + 409 * vv + 128) >> 8).clamp(0, 255) as u32;
            let g = ((298 * yv - 100 * uv - 208 * vv + 128) >> 8).clamp(0, 255) as u32;
            let b = ((298 * yv + 516 * uv + 128) >> 8).clamp(0, 255) as u32;

            out[row * width + col] = (r << 16) | (g << 8) | b;
        }
    }
    out
}
