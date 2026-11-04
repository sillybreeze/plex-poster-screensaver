//! 图像处理：缩放到屏幕、取色氛围光、Ken Burns 合成。
//!
//! 像素统一用 `u32`（0x00RRGGBB），对应 softbuffer 的缓冲区格式。

use std::fs;
use std::path::Path;

use image::imageops::FilterType;
use image::{Rgb, RgbImage};

pub const POSTER_SCALE: f32 = 1.0;
pub const KEN_BURNS_ZOOM: f32 = 0.035;
pub const DIRECTIONS: [(f32, f32); 6] = [
    (1.0, 0.0),
    (-1.0, 0.0),
    (0.0, 1.0),
    (0.0, -1.0),
    (0.7, 0.7),
    (-0.7, -0.7),
];

pub struct Composition {
    pub background: Vec<u32>,
    pub poster: RgbImage,
    pub position: (i32, i32),
    pub width: u32,
    pub height: u32,
}

pub fn build(path: &Path, width: u32, height: u32) -> Result<Composition, String> {
    // 不能依赖扩展名：Plex 可能把 PNG 内容缓存在 .jpg 文件名下。
    // `image::open` 只看扩展名，所以这里把文件读进来，按内容识别格式。
    let data = fs::read(path).map_err(|e| format!("读取封面失败 {}: {e}", path.display()))?;
    build_from_bytes(&data, width, height).map_err(|e| format!("{}: {e}", path.display()))
}

/// 直接从内存里的图片字节构建；缓存目录不可写时也用这条路径。
///
/// `image::load_from_memory` 按文件内容判断格式，不关心扩展名。
pub fn build_from_bytes(data: &[u8], width: u32, height: u32) -> Result<Composition, String> {
    let img = image::load_from_memory(data)
        .map_err(|e| format!("解码封面失败: {e}"))?
        .to_rgb8();
    Ok(build_from_image(img, width, height))
}

fn build_from_image(img: RgbImage, width: u32, height: u32) -> Composition {
    let (pw, ph) = fit(img.width(), img.height(), width, height);
    let poster = resize(&img, pw, ph);
    let background = ambient(&poster, width, height);
    let position = (((width - pw) / 2) as i32, ((height - ph) / 2) as i32);

    Composition {
        background,
        poster,
        position,
        width,
        height,
    }
}

fn fit(iw: u32, ih: u32, w: u32, h: u32) -> (u32, u32) {
    let scale = (w as f32 / iw as f32).min(h as f32 / ih as f32) * POSTER_SCALE;
    (
        ((iw as f32 * scale).round() as u32).max(1),
        ((ih as f32 * scale).round() as u32).max(1),
    )
}

fn resize(src: &RgbImage, w: u32, h: u32) -> RgbImage {
    if src.width() == w && src.height() == h {
        return src.clone();
    }
    image::imageops::resize(src, w, h, FilterType::Lanczos3)
}

#[inline]
fn pack(c: [f32; 3]) -> u32 {
    let r = c[0].clamp(0.0, 255.0) as u32;
    let g = c[1].clamp(0.0, 255.0) as u32;
    let b = c[2].clamp(0.0, 255.0) as u32;
    (r << 16) | (g << 8) | b
}

/// 按「占比 × 鲜艳度 × 亮度」挑主色：黑底海报不会得到一片黑。
fn dominant_colors(poster: &RgbImage) -> ([f32; 3], [f32; 3]) {
    let small = image::imageops::resize(poster, 64, 64, FilterType::Triangle);
    let mut bins = [0u32; 4096];
    for px in small.pixels() {
        let [r, g, b] = px.0;
        let idx = (((r >> 4) as usize) << 8) | (((g >> 4) as usize) << 4) | ((b >> 4) as usize);
        bins[idx] += 1;
    }

    let total: u32 = bins.iter().sum::<u32>().max(1);
    let mut scored: Vec<(f32, [f32; 3])> = Vec::new();
    for (idx, &count) in bins.iter().enumerate() {
        if count == 0 {
            continue;
        }
        let r = ((idx >> 8) & 0xf) as f32 * 17.0;
        let g = ((idx >> 4) & 0xf) as f32 * 17.0;
        let b = (idx & 0xf) as f32 * 17.0;
        let high = r.max(g).max(b);
        let low = r.min(g).min(b);
        let saturation = if high <= 0.0 {
            0.0
        } else {
            (high - low) / high
        };
        let luminance = (0.299 * r + 0.587 * g + 0.114 * b) / 255.0;
        let score =
            (count as f32 / total as f32).sqrt() * (0.15 + saturation) * luminance.min(0.85);
        scored.push((score, [r, g, b]));
    }
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    let top = scored.first().map(|v| v.1).unwrap_or([36.0, 36.0, 40.0]);
    let second =
        scored
            .get(1)
            .map(|v| v.1)
            .unwrap_or([top[0] * 0.55, top[1] * 0.55, top[2] * 0.55]);
    (top, second)
}

fn box_blur(src: &RgbImage, radius: i32) -> RgbImage {
    let (w, h) = (src.width() as i32, src.height() as i32);
    let mut out = src.clone();
    let get = |x: i32, y: i32| -> [f32; 3] {
        let px = src
            .get_pixel(x.clamp(0, w - 1) as u32, y.clamp(0, h - 1) as u32)
            .0;
        [px[0] as f32, px[1] as f32, px[2] as f32]
    };

    // 横向
    let mut tmp = vec![0f32; (w * h * 3) as usize];
    for y in 0..h {
        for x in 0..w {
            let mut acc = [0f32; 3];
            for d in -radius..=radius {
                let c = get(x + d, y);
                acc[0] += c[0];
                acc[1] += c[1];
                acc[2] += c[2];
            }
            let n = (radius * 2 + 1) as f32;
            let i = ((y * w + x) * 3) as usize;
            tmp[i] = acc[0] / n;
            tmp[i + 1] = acc[1] / n;
            tmp[i + 2] = acc[2] / n;
        }
    }
    // 纵向
    for y in 0..h {
        for x in 0..w {
            let mut acc = [0f32; 3];
            for d in -radius..=radius {
                let yy = (y + d).clamp(0, h - 1) as usize;
                let i = (yy * w as usize + x as usize) * 3;
                acc[0] += tmp[i];
                acc[1] += tmp[i + 1];
                acc[2] += tmp[i + 2];
            }
            let n = (radius * 2 + 1) as f32;
            out.put_pixel(
                x as u32,
                y as u32,
                Rgb([(acc[0] / n) as u8, (acc[1] / n) as u8, (acc[2] / n) as u8]),
            );
        }
    }
    out
}

/// 竖向渐变 + 封面模糊纹理 + 提饱和压暗 + 暗角。
fn ambient(poster: &RgbImage, width: u32, height: u32) -> Vec<u32> {
    let (top, bottom) = dominant_colors(poster);

    // 纹理：缩到 64 宽、模糊、再放大回屏幕尺寸
    let small_w = 64u32;
    let small_h = ((64.0 * height as f32 / width as f32).round() as u32).max(8);
    let tiny = image::imageops::resize(poster, small_w, small_h, FilterType::Triangle);
    let blurred = box_blur(&tiny, 3);
    let texture = image::imageops::resize(&blurred, width, height, FilterType::Triangle);

    // 竖向渐变（每行一个颜色）
    let gradient: Vec<[f32; 3]> = (0..height)
        .map(|y| {
            let t = y as f32 / (height.max(2) - 1) as f32;
            [
                top[0] + (bottom[0] - top[0]) * t,
                top[1] + (bottom[1] - top[1]) * t,
                top[2] + (bottom[2] - top[2]) * t,
            ]
        })
        .collect();

    // 暗角：预先算好 0..=1023 的查表
    let mut lut = [0f32; 1024];
    for (i, slot) in lut.iter_mut().enumerate() {
        let d = i as f32 / 1023.0;
        *slot = 1.0 - 0.59 * d.powf(2.4);
    }

    let cx = width as f32 / 2.0;
    let cy = height as f32 / 2.0;
    let max_d = (cx * cx + cy * cy).sqrt().max(1.0);

    let mut out = vec![0u32; (width * height) as usize];
    for y in 0..height {
        let dy = y as f32 - cy;
        let g = gradient[y as usize];
        let row = (y * width) as usize;
        for x in 0..width {
            let dx = x as f32 - cx;
            let d = ((dx * dx + dy * dy).sqrt() / max_d * 1023.0) as usize;
            let v = lut[d.min(1023)];

            let [tr, tg, tb] = texture.get_pixel(x, y).0.map(|c| c as f32);
            let mut c = [
                g[0] * 0.68 + tr * 0.32,
                g[1] * 0.68 + tg * 0.32,
                g[2] * 0.68 + tb * 0.32,
            ];
            // 提饱和
            let lum = 0.299 * c[0] + 0.587 * c[1] + 0.114 * c[2];
            for ch in &mut c {
                *ch = lum + (*ch - lum) * 1.25;
            }
            // 压暗 + 暗角
            for ch in &mut c {
                *ch *= 0.52 * v;
            }
            out[row + x as usize] = pack(c);
        }
    }
    out
}

/// 计算 Ken Burns 在绘制时需要的 UV 矩形：[u0, v0, du, dv]。
pub fn ken_burns_uv(pw: f32, ph: f32, progress: f32, direction: (f32, f32)) -> [f32; 4] {
    let progress = progress.clamp(0.0, 1.0);
    let scale = 1.0 / (1.0 + KEN_BURNS_ZOOM * progress);
    let cw = pw * scale;
    let ch = ph * scale;
    let ease = progress * progress * (3.0 - 2.0 * progress);

    let cx = pw / 2.0 + direction.0 * (pw - cw) / 2.0 * ease;
    let cy = ph / 2.0 + direction.1 * (ph - ch) / 2.0 * ease;
    let left = (cx - cw / 2.0).clamp(0.0, (pw - cw).max(0.0));
    let top = (cy - ch / 2.0).clamp(0.0, (ph - ch).max(0.0));
    [left / pw, top / ph, cw / pw, ch / ph]
}

/// 把海报按 Ken Burns 进度画到 `out` 上。
///
/// `full = false` 时只重绘海报覆盖的那一块——动效期间背景是静止的，
/// 整屏拷贝 8MB 纯属浪费（1080p 下能省掉约 60% 的每帧内存流量）。
pub fn draw_poster(
    comp: &Composition,
    progress: f32,
    direction: (f32, f32),
    out: &mut [u32],
    full: bool,
) {
    if full {
        out.copy_from_slice(&comp.background);
    } else {
        restore_background(comp, out);
    }

    let pw = comp.poster.width() as f32;
    let ph = comp.poster.height() as f32;
    if pw < 1.0 || ph < 1.0 {
        return;
    }

    let progress = progress.clamp(0.0, 1.0);
    let scale = 1.0 / (1.0 + KEN_BURNS_ZOOM * progress);
    let cw = pw * scale;
    let ch = ph * scale;
    let ease = progress * progress * (3.0 - 2.0 * progress);

    let cx = pw / 2.0 + direction.0 * (pw - cw) / 2.0 * ease;
    let cy = ph / 2.0 + direction.1 * (ph - ch) / 2.0 * ease;
    let left = (cx - cw / 2.0).clamp(0.0, pw - cw);
    let top = (cy - ch / 2.0).clamp(0.0, ph - ch);

    let step_x = cw / pw;
    let step_y = ch / ph;
    let (ox, oy) = comp.position;
    let dst_w = comp.width as usize;

    let post = &comp.poster;
    let (iw, ih) = (post.width() as i32, post.height() as i32);
    let raw = post.as_raw();

    let mut sy = top + step_y * 0.5;
    for dy in 0..ph as i32 {
        let py = oy + dy;
        if py < 0 || py >= comp.height as i32 {
            sy += step_y;
            continue;
        }
        let mut sx = left + step_x * 0.5;
        let row = py as usize * dst_w;
        let limit = comp.width as i32 - ox;
        let n = (pw as i32).min(limit);
        for dx in 0..n.max(0) {
            let idx = row + (ox + dx) as usize;
            // 索引已在上面钳制过，unsafe 省掉逐像素边界检查
            unsafe {
                *out.get_unchecked_mut(idx) = sample_bilinear(raw, iw, ih, sx, sy);
            }
            sx += step_x;
        }
        sy += step_y;
    }
}

/// 把海报矩形区域的背景写回去（只动这一块）。
fn restore_background(comp: &Composition, out: &mut [u32]) {
    let pw = comp.poster.width() as i32;
    let ph = comp.poster.height() as i32;
    let (ox, oy) = comp.position;
    let dst_w = comp.width as i32;

    let x0 = ox.max(0);
    let x1 = (ox + pw).min(dst_w);
    if x1 <= x0 {
        return;
    }
    for dy in 0..ph {
        let py = oy + dy;
        if py < 0 || py >= comp.height as i32 {
            continue;
        }
        let start = (py * dst_w + x0) as usize;
        let end = (py * dst_w + x1) as usize;
        out[start..end].copy_from_slice(&comp.background[start..end]);
    }
}

#[inline]
fn sample_bilinear(raw: &[u8], iw: i32, ih: i32, x: f32, y: f32) -> u32 {
    let x0 = x.floor() as i32;
    let y0 = y.floor() as i32;
    let fx = x - x0 as f32;
    let fy = y - y0 as f32;

    let x1 = (x0 + 1).min(iw - 1);
    let y1 = (y0 + 1).min(ih - 1);
    let x0 = x0.clamp(0, iw - 1);
    let y0 = y0.clamp(0, ih - 1);

    let idx = |xx: i32, yy: i32| ((yy * iw + xx) * 3) as usize;
    let (i00, i10, i01, i11) = (idx(x0, y0), idx(x1, y0), idx(x0, y1), idx(x1, y1));

    let mut out = 0u32;
    for c in 0..3 {
        // 四个采样点的下标都来自 clamp 过的坐标，必定在界内
        let (p00, p10, p01, p11) = unsafe {
            (
                *raw.get_unchecked(i00 + c) as f32,
                *raw.get_unchecked(i10 + c) as f32,
                *raw.get_unchecked(i01 + c) as f32,
                *raw.get_unchecked(i11 + c) as f32,
            )
        };
        let top = p00 + (p10 - p00) * fx;
        let bottom = p01 + (p11 - p01) * fx;
        let value = (top + (bottom - top) * fy).clamp(0.0, 255.0) as u32;
        out |= value << (16 - 8 * c as u32);
    }
    out
}

pub fn save_png(out: &[u32], width: u32, height: u32, path: &Path) -> Result<(), String> {
    let mut buf = Vec::with_capacity(out.len() * 3);
    for v in out {
        buf.push(((v >> 16) & 0xff) as u8);
        buf.push(((v >> 8) & 0xff) as u8);
        buf.push((v & 0xff) as u8);
    }
    image::save_buffer(path, &buf, width, height, image::ColorType::Rgb8)
        .map_err(|e| format!("保存 PNG 失败: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_content_with_jpg_name_still_decodes() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("render-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let png = dir.join("poster.png");
        let jpg_name = dir.join("poster.jpg");
        let img = RgbImage::from_pixel(4, 6, Rgb([10, 20, 30]));
        image::save_buffer(&png, img.as_raw(), 4, 6, image::ColorType::Rgb8).unwrap();
        fs::rename(&png, &jpg_name).unwrap();

        let comp = build(&jpg_name, 64, 64).expect("扩展名和内容不一致时也要能解码");
        assert_eq!((comp.width, comp.height), (64, 64));
        assert_eq!(comp.background.len(), 64 * 64);
        assert!(comp.poster.width() > 0 && comp.poster.height() > 0);

        let _ = fs::remove_dir_all(&dir);
    }
}
