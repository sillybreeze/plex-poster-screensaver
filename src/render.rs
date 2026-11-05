//! 图像处理：缩放到屏幕、取色氛围光、Ken Burns 合成。
//!
//! 像素统一用 `u32`（0x00RRGGBB），对应 softbuffer 的缓冲区格式。

use std::fs;
use std::path::Path;

use image::imageops::FilterType;
use image::{Rgb, RgbImage};

use crate::config::{MotionMode, Transition};
use crate::layout;

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

/// 海报墙空白处的统一低对比背景色（0xRRGGBB）。
pub const WALL_BACKGROUND: [u8; 3] = [18, 18, 22];

pub struct Composition {
    pub background: Vec<u32>,
    pub poster: RgbImage,
    pub position: (i32, i32),
    pub width: u32,
    pub height: u32,
}

/// 预取线程交付的一帧：单张布局是氛围背景 + 一张居中海报，
/// 海报墙布局是一整张已经合成好的全屏画布。
pub enum Frame {
    Single(Composition),
    Wall(WallComposition),
}

pub struct WallComposition {
    /// 全屏画布，0x00RRGGBB。
    pub canvas: Vec<u32>,
    pub width: u32,
    pub height: u32,
    /// 实际画出的海报数量（1–6）。
    pub cards: usize,
}

impl Frame {
    pub fn width(&self) -> u32 {
        match self {
            Self::Single(comp) => comp.width,
            Self::Wall(wall) => wall.width,
        }
    }

    pub fn height(&self) -> u32 {
        match self {
            Self::Single(comp) => comp.height,
            Self::Wall(wall) => wall.height,
        }
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width(), self.height())
    }
}

/// 按内容解码；缓存里常见 PNG 内容配 `.jpg` 文件名，不能看扩展名。
pub fn decode(data: &[u8]) -> Result<RgbImage, String> {
    image::load_from_memory(data)
        .map(|img| img.to_rgb8())
        .map_err(|e| format!("解码封面失败: {e}"))
}

pub fn decode_file(path: &Path) -> Result<RgbImage, String> {
    let data = fs::read(path).map_err(|e| format!("读取封面失败 {}: {e}", path.display()))?;
    decode(&data).map_err(|e| format!("{}: {e}", path.display()))
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

/// 单张布局：原始海报 → 完整居中海报 + 氛围背景。
pub fn compose_single(img: RgbImage, width: u32, height: u32) -> Composition {
    build_from_image(img, width, height)
}

/// 把一组原始海报合成为一帧。
///
/// 只有 1 张时退回单张布局（氛围背景 + 居中海报），不画空卡占位；
/// 0 张返回错误，让上层沿用“暂无内容”的等待/错误状态。
pub fn compose_wall_frame(
    mut images: Vec<RgbImage>,
    width: u32,
    height: u32,
) -> Result<Frame, String> {
    match images.len() {
        0 => Err("没有可用的海报".to_string()),
        1 => Ok(Frame::Single(build_from_image(
            images.remove(0),
            width,
            height,
        ))),
        _ => Ok(Frame::Wall(compose_wall(&images, width, height))),
    }
}

/// 在屏幕尺寸的画布上按 3×2（或短列表网格）摆好每张海报。
pub fn compose_wall(images: &[RgbImage], width: u32, height: u32) -> WallComposition {
    let background = pack(WALL_BACKGROUND.map(|c| c as f32));
    let mut canvas = vec![background; (width as usize).saturating_mul(height as usize)];
    let cells = layout::cells_for(images.len(), width, height);

    for (img, cell) in images.iter().zip(cells) {
        let (w, h) = layout::fit_in_cell(img.width(), img.height(), &cell);
        let scaled = image::imageops::resize(img, w, h, FilterType::Lanczos3);
        let x = cell.x + (cell.width.saturating_sub(w)) / 2;
        let y = cell.y + (cell.height.saturating_sub(h)) / 2;
        for (dx, dy, px) in scaled.enumerate_pixels() {
            let idx = (y + dy) as usize * width as usize + (x + dx) as usize;
            if let Some(slot) = canvas.get_mut(idx) {
                let [r, g, b] = px.0;
                *slot = ((r as u32) << 16) | ((g as u32) << 8) | b as u32;
            }
        }
    }

    WallComposition {
        canvas,
        width,
        height,
        cards: images.len(),
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

/// 方向滑入的四条边：值表示画面从哪一侧进入中央。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SlideDirection {
    Left,
    Right,
    Up,
    Down,
}

/// 一次转场实际使用的效果；随机方向在开始换图时就已经选定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionKind {
    Fade,
    Slide(SlideDirection),
}

/// 平滑缓动（smoothstep），在 0–1 上单调递增且两端导数为 0。
pub fn ease(progress: f32) -> f32 {
    let t = progress.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// 由 elapsed/duration 计算转场进度；时长非法时视为立即完成。
pub fn transition_progress(elapsed_secs: f64, duration_secs: f64) -> f32 {
    if !duration_secs.is_finite() || duration_secs <= 0.0 {
        return 1.0;
    }
    (elapsed_secs / duration_secs).clamp(0.0, 1.0) as f32
}

/// 混合种子，避免直接用低位的规律性。
fn mix_seed(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

/// 从四个方向里挑一个；如果上一张也是随机方向，尽量不和它连续重复。
///
/// 纯函数：同一个 seed 得到同一个结果，方向在一次转场期间不会变化。
pub fn pick_direction(seed: u64, previous: Option<SlideDirection>) -> SlideDirection {
    const ALL: [SlideDirection; 4] = [
        SlideDirection::Left,
        SlideDirection::Right,
        SlideDirection::Up,
        SlideDirection::Down,
    ];
    let mut choices = [SlideDirection::Left; 4];
    let mut len = 0usize;
    for direction in ALL {
        if Some(direction) != previous {
            choices[len] = direction;
            len += 1;
        }
    }
    let slot = (mix_seed(seed) % len as u64) as usize;
    choices[slot]
}

/// 配置里的 `transition` 值转换为本次实际效果；`random` 用 seed 选一次方向。
pub fn resolve_transition(
    transition: Transition,
    seed: u64,
    previous: Option<SlideDirection>,
) -> TransitionKind {
    match transition {
        Transition::Fade => TransitionKind::Fade,
        Transition::SlideLeft => TransitionKind::Slide(SlideDirection::Left),
        Transition::SlideRight => TransitionKind::Slide(SlideDirection::Right),
        Transition::SlideUp => TransitionKind::Slide(SlideDirection::Up),
        Transition::SlideDown => TransitionKind::Slide(SlideDirection::Down),
        Transition::Random => TransitionKind::Slide(pick_direction(seed, previous)),
    }
}

/// 本次换图是否需要转场。首帧、静止模式、时长为 0 都直接切到新画面。
pub fn choose_transition(
    transition: Transition,
    motion: MotionMode,
    seconds: f64,
    has_previous: bool,
    previous: Option<SlideDirection>,
    seed: u64,
) -> Option<TransitionKind> {
    if motion == MotionMode::Static || !seconds.is_finite() || seconds <= 0.0 || !has_previous {
        return None;
    }
    Some(resolve_transition(transition, seed, previous))
}

/// 方向滑入时对图层施加的偏移（屏幕像素）。
///
/// `rect` 是图层最终居中时的矩形 `[x, y, w, h]`；返回相对最终位置的偏移。
/// 起点保证图层完全在画面外：左/右从左右边缘外开始，上/下从上下边缘外开始；
/// 进度 1 时精确回到 `(0, 0)`。
pub fn slide_offset(
    direction: SlideDirection,
    progress: f32,
    rect: [f32; 4],
    screen: (u32, u32),
) -> (f32, f32) {
    let [x, y, w, h] = rect;
    let (screen_w, screen_h) = (screen.0 as f32, screen.1 as f32);
    let (start_x, start_y) = match direction {
        SlideDirection::Left => (-w, y),
        SlideDirection::Right => (screen_w, y),
        SlideDirection::Up => (x, -h),
        SlideDirection::Down => (x, screen_h),
    };
    let k = 1.0 - ease(progress);
    ((start_x - x) * k, (start_y - y) * k)
}

/// 转场中旧/新两层的透明度与偏移。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayerShift {
    /// 新画面背景层（单张布局）或整张墙面画布（海报墙）的透明度。
    pub bg_alpha: f32,
    /// 单张布局里海报图层的透明度；海报墙没有独立海报层，恒为 0。
    pub poster_alpha: f32,
    pub offset: (f32, f32),
}

/// 根据转场类型和进度计算图层参数。
///
/// 单张布局：背景按进度淡化，海报层保持不透明并滑入；
/// 海报墙布局：整张画布按进度移动，露出的部分保留旧场景。
pub fn transition_layers(
    kind: TransitionKind,
    progress: f32,
    rect: [f32; 4],
    screen: (u32, u32),
    wall: bool,
) -> LayerShift {
    let t = ease(progress);
    match kind {
        TransitionKind::Fade => LayerShift {
            bg_alpha: t,
            poster_alpha: t,
            offset: (0.0, 0.0),
        },
        TransitionKind::Slide(direction) => {
            let offset = slide_offset(direction, progress, rect, screen);
            if wall {
                LayerShift {
                    bg_alpha: 1.0,
                    poster_alpha: 0.0,
                    offset,
                }
            } else {
                LayerShift {
                    bg_alpha: t,
                    poster_alpha: 1.0,
                    offset,
                }
            }
        }
    }
}

/// 按布局类型把一帧画到 CPU 缓冲区，供快照与自检使用。
pub fn draw_frame(
    frame: &Frame,
    progress: f32,
    direction: (f32, f32),
    out: &mut [u32],
    full: bool,
) {
    match frame {
        Frame::Single(comp) => draw_poster(comp, progress, direction, out, full),
        Frame::Wall(wall) => {
            let n = out.len().min(wall.canvas.len());
            out[..n].copy_from_slice(&wall.canvas[..n]);
        }
    }
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

/// 把海报按 UV 采样叠加到已有画面上，`offset` 为相对最终居中位置的屏幕像素偏移。
///
/// 不重置背景，超出画布的部分自动裁掉；供设置窗口的 CPU 转场预览使用。
pub fn draw_poster_overlay(comp: &Composition, uv: [f32; 4], offset: (f32, f32), out: &mut [u32]) {
    let pw = comp.poster.width() as i32;
    let ph = comp.poster.height() as i32;
    if pw <= 0
        || ph <= 0
        || comp.width == 0
        || comp.height == 0
        || out.len() < comp.width as usize * comp.height as usize
    {
        return;
    }
    let ox = comp.position.0 + offset.0.round() as i32;
    let oy = comp.position.1 + offset.1.round() as i32;
    let x0 = ox.max(0);
    let x1 = (ox + pw).min(comp.width as i32);
    let y0 = oy.max(0);
    let y1 = (oy + ph).min(comp.height as i32);
    if x1 <= x0 || y1 <= y0 {
        return;
    }

    let dst_w = comp.width as usize;
    let raw = comp.poster.as_raw();
    let inv_w = 1.0 / pw as f32;
    let inv_h = 1.0 / ph as f32;
    for dy in y0..y1 {
        let qy = ((dy - oy) as f32 + 0.5) * inv_h;
        let sy = (uv[1] + qy * uv[3]) * ph as f32;
        let row = dy as usize * dst_w;
        for dx in x0..x1 {
            let qx = ((dx - ox) as f32 + 0.5) * inv_w;
            let sx = (uv[0] + qx * uv[2]) * pw as f32;
            out[row + dx as usize] = sample_bilinear(raw, pw, ph, sx, sy);
        }
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

        let comp = compose_single(
            decode_file(&jpg_name).expect("扩展名和内容不一致时也要能解码"),
            64,
            64,
        );
        assert_eq!((comp.width, comp.height), (64, 64));
        assert_eq!(comp.background.len(), 64 * 64);
        assert!(comp.poster.width() > 0 && comp.poster.height() > 0);

        let _ = fs::remove_dir_all(&dir);
    }

    const SCREEN: (u32, u32) = (1920, 1080);

    #[test]
    fn poster_overlay_clips_without_touching_outside_pixels() {
        let img = solid(20, 30, [200, 40, 40]);
        let comp = compose_single(img, 100, 100);
        let packed = (200u32 << 16) | (40 << 8) | 40;

        let mut out = vec![0u32; 100 * 100];
        draw_poster_overlay(&comp, [0.0, 0.0, 1.0, 1.0], (0.0, 0.0), &mut out);
        assert_eq!(out[50 * 100 + 50], packed);
        assert_eq!(out[0], 0, "海报矩形外不应被覆盖");

        // 完全移到画布外：一个像素都不写。
        let mut outside = vec![0u32; 100 * 100];
        draw_poster_overlay(&comp, [0.0, 0.0, 1.0, 1.0], (100.0, 0.0), &mut outside);
        assert!(outside.iter().all(|value| *value == 0));

        // 左移半个海报宽度：左半张被裁掉，右边缘外保持 0。
        let offset = -(comp.poster.width() as f32 / 2.0);
        let mut partial = vec![0u32; 100 * 100];
        draw_poster_overlay(&comp, [0.0, 0.0, 1.0, 1.0], (offset, 0.0), &mut partial);
        let ox = comp.position.0 + offset.round() as i32;
        let right = ox + comp.poster.width() as i32;
        assert_eq!(partial[50 * 100 + (right - 1) as usize], packed);
        if right < 100 {
            assert_eq!(partial[50 * 100 + right as usize], 0);
        }
    }

    #[test]
    fn ease_is_monotonic_and_hits_endpoints() {
        assert_eq!(ease(0.0), 0.0);
        assert_eq!(ease(1.0), 1.0);
        let mut previous = -1.0;
        for step in 0..=100 {
            let value = ease(step as f32 / 100.0);
            assert!(value >= previous, "缓动必须单调: {value} < {previous}");
            previous = value;
        }
        // 中点的 smoothstep 恰好是 0.5。
        assert!((ease(0.5) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn transition_progress_clamps_and_handles_bad_duration() {
        assert_eq!(transition_progress(0.0, 1.0), 0.0);
        assert_eq!(transition_progress(0.5, 1.0), 0.5);
        assert_eq!(transition_progress(2.0, 1.0), 1.0);
        assert_eq!(transition_progress(0.0, 0.0), 1.0);
        assert_eq!(transition_progress(0.0, f64::NAN), 1.0);
    }

    #[test]
    fn slide_starts_fully_off_screen_and_ends_centered() {
        let rect = [700.0, 200.0, 520.0, 680.0];
        let cases = [
            (SlideDirection::Left, -520.0),
            (SlideDirection::Right, 1920.0),
            (SlideDirection::Up, -680.0),
            (SlideDirection::Down, 1080.0),
        ];
        for (direction, outside) in cases {
            let (dx, dy) = slide_offset(direction, 0.0, rect, SCREEN);
            // 起点：图层完全离开画面（左/右看 x，上/下看 y）。
            match direction {
                SlideDirection::Left => {
                    assert!(rect[0] + dx + rect[2] <= 0.0, "左滑起点应完全离屏")
                }
                SlideDirection::Right => assert!(rect[0] + dx >= 1920.0, "右滑起点应完全离屏"),
                SlideDirection::Up => assert!(rect[1] + dy + rect[3] <= 0.0, "下滑起点应完全离屏"),
                SlideDirection::Down => assert!(rect[1] + dy >= 1080.0, "上滑起点应完全离屏"),
            }
            // 另一轴保持居中。
            if matches!(direction, SlideDirection::Left | SlideDirection::Right) {
                assert_eq!(dy, 0.0);
            } else {
                assert_eq!(dx, 0.0);
            }
            let (end_dx, end_dy) = slide_offset(direction, 1.0, rect, SCREEN);
            assert_eq!((end_dx, end_dy), (0.0, 0.0), "结束时必须精确归位");
            // 起点偏移方向与预期一致（左/上滑入为负，右/下滑入为正）。
            match direction {
                SlideDirection::Left | SlideDirection::Up => {
                    assert!(dx < 0.0 || dy < 0.0, "上/左滑入起点偏移应为负: {outside}");
                }
                SlideDirection::Right | SlideDirection::Down => {
                    assert!(dx > 0.0 || dy > 0.0, "下/右滑入起点偏移应为正: {outside}");
                }
            }
        }
    }

    #[test]
    fn slide_offset_is_monotonic_toward_center() {
        let rect = [700.0, 200.0, 520.0, 680.0];
        for direction in [
            SlideDirection::Left,
            SlideDirection::Right,
            SlideDirection::Up,
            SlideDirection::Down,
        ] {
            let mut previous = slide_offset(direction, 0.0, rect, SCREEN);
            for step in 1..=50 {
                let current = slide_offset(direction, step as f32 / 50.0, rect, SCREEN);
                let moved_toward_center = match direction {
                    SlideDirection::Left | SlideDirection::Up => {
                        current.0 >= previous.0 && current.1 >= previous.1
                    }
                    SlideDirection::Right | SlideDirection::Down => {
                        current.0 <= previous.0 && current.1 <= previous.1
                    }
                };
                assert!(
                    moved_toward_center,
                    "{direction:?} 的偏移必须单调收敛: {previous:?} -> {current:?}"
                );
                previous = current;
            }
        }
    }

    #[test]
    fn wall_slide_moves_the_whole_canvas() {
        let rect = [0.0, 0.0, 1920.0, 1080.0];
        let start = transition_layers(
            TransitionKind::Slide(SlideDirection::Left),
            0.0,
            rect,
            SCREEN,
            true,
        );
        assert_eq!(start.bg_alpha, 1.0, "墙面画布保持不透明");
        assert_eq!(start.poster_alpha, 0.0, "墙面没有独立海报层");
        assert_eq!(start.offset.0, -1920.0, "整张画布完全移到左边缘外");

        let end = transition_layers(
            TransitionKind::Slide(SlideDirection::Left),
            1.0,
            rect,
            SCREEN,
            true,
        );
        assert_eq!(end.offset, (0.0, 0.0));
    }

    #[test]
    fn single_slide_keeps_poster_opaque_and_fades_background() {
        let rect = [700.0, 200.0, 520.0, 680.0];
        let mid = transition_layers(
            TransitionKind::Slide(SlideDirection::Right),
            0.5,
            rect,
            SCREEN,
            false,
        );
        assert!((mid.bg_alpha - 0.5).abs() < 1e-6);
        assert_eq!(mid.poster_alpha, 1.0, "海报滑入时不应变成半透明残影");
        assert!(mid.offset.0 > 0.0);

        let fade = transition_layers(TransitionKind::Fade, 0.5, rect, SCREEN, false);
        assert_eq!(fade.offset, (0.0, 0.0));
        assert!((fade.bg_alpha - 0.5).abs() < 1e-6);
        assert!((fade.poster_alpha - 0.5).abs() < 1e-6);
    }

    #[test]
    fn choose_transition_skips_first_frame_static_and_zero_duration() {
        let seed = 42;
        assert_eq!(
            choose_transition(
                Transition::SlideLeft,
                MotionMode::Animated,
                1.0,
                false,
                None,
                seed
            ),
            None,
            "首帧没有旧画面，不应做空转场"
        );
        assert_eq!(
            choose_transition(
                Transition::SlideLeft,
                MotionMode::Static,
                1.0,
                true,
                None,
                seed
            ),
            None,
            "静止模式必须跳过转场"
        );
        assert_eq!(
            choose_transition(
                Transition::Fade,
                MotionMode::Animated,
                0.0,
                true,
                None,
                seed
            ),
            None,
            "--fade 0 直接切换"
        );
        assert_eq!(
            choose_transition(
                Transition::SlideUp,
                MotionMode::Animated,
                1.0,
                true,
                None,
                seed
            ),
            Some(TransitionKind::Slide(SlideDirection::Up))
        );
    }

    #[test]
    fn random_direction_is_stable_and_avoids_immediate_repeat() {
        let first = pick_direction(1, None);
        assert_eq!(first, pick_direction(1, None), "同一 seed 必须稳定");

        let mut seen = std::collections::HashSet::new();
        for seed in 0..200 {
            seen.insert(pick_direction(seed, None));
            let previous = pick_direction(seed, None);
            assert_ne!(
                pick_direction(seed, Some(previous)),
                previous,
                "随机方向不应与上一张连续重复"
            );
        }
        assert_eq!(seen.len(), 4, "四个方向都应有可能被选中");

        let _ = first;
    }

    #[test]
    fn resolve_transition_maps_all_values() {
        assert_eq!(
            resolve_transition(Transition::Fade, 7, None),
            TransitionKind::Fade
        );
        assert_eq!(
            resolve_transition(Transition::SlideLeft, 7, None),
            TransitionKind::Slide(SlideDirection::Left)
        );
        assert_eq!(
            resolve_transition(Transition::SlideRight, 7, None),
            TransitionKind::Slide(SlideDirection::Right)
        );
        assert_eq!(
            resolve_transition(Transition::SlideUp, 7, None),
            TransitionKind::Slide(SlideDirection::Up)
        );
        assert_eq!(
            resolve_transition(Transition::SlideDown, 7, None),
            TransitionKind::Slide(SlideDirection::Down)
        );
        assert!(matches!(
            resolve_transition(Transition::Random, 7, None),
            TransitionKind::Slide(_)
        ));
    }

    fn solid(width: u32, height: u32, rgb: [u8; 3]) -> RgbImage {
        RgbImage::from_pixel(width, height, Rgb(rgb))
    }

    #[test]
    fn one_or_zero_images_fall_back_correctly() {
        let single = compose_wall_frame(vec![solid(40, 60, [200, 40, 40])], 320, 180).unwrap();
        assert!(
            matches!(single, Frame::Single(_)),
            "只有 1 张时应退回单张布局"
        );
        assert_eq!(single.size(), (320, 180));
        match single {
            Frame::Single(comp) => {
                assert!(comp.poster.width() > 0 && comp.poster.height() > 0);
                assert_eq!(comp.background.len(), 320 * 180);
            }
            Frame::Wall(_) => unreachable!(),
        }

        assert!(compose_wall_frame(Vec::new(), 320, 180).is_err());
    }

    #[test]
    fn wall_canvas_has_screen_size_and_neutral_background() {
        let images = vec![
            solid(40, 60, [200, 40, 40]),
            solid(40, 60, [40, 200, 40]),
            solid(40, 60, [40, 40, 200]),
        ];
        let frame = compose_wall_frame(images, 640, 360).unwrap();
        match frame {
            Frame::Wall(wall) => {
                assert_eq!((wall.width, wall.height), (640, 360));
                assert_eq!(wall.canvas.len(), 640 * 360);
                assert_eq!(wall.cards, 3);
                let background = pack(WALL_BACKGROUND.map(|c| c as f32));
                // 四角一定在卡片之外，应保持统一背景色。
                for (x, y) in [(0usize, 0usize), (639, 0), (0, 359), (639, 359)] {
                    assert_eq!(
                        wall.canvas[y * 640 + x],
                        background,
                        "角点 ({x},{y}) 应是背景色"
                    );
                }
            }
            Frame::Single(_) => panic!("3 张应走海报墙"),
        }
    }

    #[test]
    fn wall_posters_are_drawn_inside_cells_without_overlap() {
        let images: Vec<RgbImage> = [
            [255u8, 0, 0],
            [0, 255, 0],
            [0, 0, 255],
            [255, 255, 0],
            [0, 255, 255],
            [255, 0, 255],
        ]
        .into_iter()
        .map(|rgb| solid(60, 90, rgb))
        .collect();
        let (width, height) = (640, 360);
        let frame = compose_wall_frame(images, width, height).unwrap();
        let Frame::Wall(wall) = frame else {
            panic!("6 张应走海报墙");
        };
        let cells = layout::cells_for(6, width, height);
        assert_eq!(cells.len(), 6);

        for (index, cell) in cells.iter().enumerate() {
            // 在每个格子里找该海报的纯色像素，必须全部落在格子内。
            let expected = [
                [255u32, 0, 0],
                [0, 255, 0],
                [0, 0, 255],
                [255, 255, 0],
                [0, 255, 255],
                [255, 0, 255],
            ][index];
            let packed = (expected[0] << 16) | (expected[1] << 8) | expected[2];
            let mut found = 0;
            for y in 0..height {
                for x in 0..width {
                    if wall.canvas[y as usize * width as usize + x as usize] == packed {
                        assert!(
                            x >= cell.x
                                && x < cell.x + cell.width
                                && y >= cell.y
                                && y < cell.y + cell.height,
                            "第 {index} 张海报的像素越出格子: ({x},{y}) 不在 {cell:?}"
                        );
                        found += 1;
                    }
                }
            }
            assert!(found > 0, "第 {index} 张海报没有画出来");
        }
    }
}
