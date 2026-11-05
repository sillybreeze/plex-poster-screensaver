//! 设置窗口右侧的 CPU 预览：不依赖 D3D，按当前显示组合循环合成两张场景。

use std::time::Instant;

use image::imageops::FilterType;
use image::RgbImage;
use windows::Win32::Graphics::Gdi::{BITMAPINFO, BITMAPINFOHEADER};

use crate::config::{DisplayLayout, DisplaySettings, MotionMode, MAX_TRANSITION_SECONDS};
use crate::layout::WALL_POSTERS;
use crate::plex::Cache;
use crate::render::{self, Frame, DIRECTIONS};

const PREVIEW_MAX_HOLD: f64 = 4.0;

pub(crate) struct PreviewState {
    images: Vec<RgbImage>,
    scenes: Option<[Frame; 2]>,
    settings: DisplaySettings,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) frame: Vec<u32>,
    scratch: Vec<u32>,
    epoch: Instant,
}

impl PreviewState {
    pub(crate) fn new(
        settings: DisplaySettings,
        images: Vec<RgbImage>,
        width: u32,
        height: u32,
    ) -> Self {
        let mut state = Self {
            images,
            scenes: None,
            settings: settings.clone(),
            width,
            height,
            frame: vec![0; width as usize * height as usize],
            scratch: vec![0; width as usize * height as usize],
            epoch: Instant::now(),
        };
        state.scenes = Some(build_scenes(
            &state.images,
            state.settings.display_layout,
            width,
            height,
        ));
        state.update_frame(Instant::now());
        state
    }

    pub(crate) fn set_settings(&mut self, settings: DisplaySettings) {
        let rebuild =
            settings.display_layout != self.settings.display_layout || self.scenes.is_none();
        self.settings = settings;
        if rebuild {
            self.scenes = Some(build_scenes(
                &self.images,
                self.settings.display_layout,
                self.width,
                self.height,
            ));
        }
        self.epoch = Instant::now();
        self.update_frame(Instant::now());
    }

    pub(crate) fn bitmap_info(&self) -> BITMAPINFO {
        BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: self.width as i32,
                // 负高度表示自上而下的像素顺序，和 Vec<u32> 的排列一致。
                biHeight: -(self.height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0, // BI_RGB
                biSizeImage: self.width * self.height * 4,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    pub(crate) fn update_frame(&mut self, now: Instant) {
        let Some(scenes) = self.scenes.as_ref() else {
            return;
        };
        let settings = &self.settings;
        let animated = settings.motion_mode == MotionMode::Animated;
        let hold = settings.display_seconds.clamp(1.0, PREVIEW_MAX_HOLD);
        let transition = if animated {
            settings
                .transition_seconds
                .clamp(0.0, MAX_TRANSITION_SECONDS)
        } else {
            0.0
        };
        let cycle = hold + transition;
        if cycle <= 0.0 {
            return;
        }
        let elapsed = now.saturating_duration_since(self.epoch).as_secs_f64();
        let cycle_index = (elapsed / cycle) as u64;
        let t = elapsed % cycle;
        let from = (cycle_index % 2) as usize;
        let to = 1 - from;
        let direction = DIRECTIONS[(cycle_index as usize) % DIRECTIONS.len()];

        let ken =
            animated && settings.ken_burns && settings.display_layout == DisplayLayout::Single;
        if t < hold || transition <= 0.0 {
            let progress = if ken {
                (t / hold).clamp(0.0, 1.0) as f32
            } else {
                0.0
            };
            render_frame(&scenes[from], progress, direction, &mut self.frame);
            return;
        }

        let progress = ((t - hold) / transition).clamp(0.0, 1.0) as f32;
        let seed = cycle_index.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        let kind = render::resolve_transition(settings.transition, seed, None);
        let (rect, wall) = scene_rect(&scenes[to]);
        let shift =
            render::transition_layers(kind, progress, rect, (self.width, self.height), wall);
        let old_progress = if ken { 1.0 } else { 0.0 };

        match (&scenes[from], &scenes[to], kind) {
            (Frame::Single(_), Frame::Single(to_comp), render::TransitionKind::Slide(_)) => {
                render_frame(&scenes[from], old_progress, direction, &mut self.frame);
                blend_over(&mut self.frame, &to_comp.background, shift.bg_alpha);
                render::draw_poster_overlay(
                    to_comp,
                    [0.0, 0.0, 1.0, 1.0],
                    shift.offset,
                    &mut self.frame,
                );
            }
            (Frame::Wall(_), Frame::Wall(to_wall), render::TransitionKind::Slide(_)) => {
                render_frame(&scenes[from], 0.0, direction, &mut self.frame);
                blit_canvas(
                    &to_wall.canvas,
                    self.width,
                    self.height,
                    shift.offset,
                    &mut self.frame,
                );
            }
            _ => {
                render_frame(&scenes[from], old_progress, direction, &mut self.frame);
                render_frame(&scenes[to], 0.0, direction, &mut self.scratch);
                blend_over(&mut self.frame, &self.scratch, shift.bg_alpha);
            }
        }
    }
}

fn render_frame(scene: &Frame, progress: f32, direction: (f32, f32), out: &mut [u32]) {
    render::draw_frame(scene, progress, direction, out, true);
}

fn scene_rect(scene: &Frame) -> ([f32; 4], bool) {
    match scene {
        Frame::Single(comp) => (
            [
                comp.position.0 as f32,
                comp.position.1 as f32,
                comp.poster.width() as f32,
                comp.poster.height() as f32,
            ],
            false,
        ),
        Frame::Wall(wall) => ([0.0, 0.0, wall.width as f32, wall.height as f32], true),
    }
}

fn build_scenes(images: &[RgbImage], layout: DisplayLayout, width: u32, height: u32) -> [Frame; 2] {
    if images.is_empty() {
        let fallback = placeholder(0);
        return [
            Frame::Single(render::compose_single(fallback.clone(), width, height)),
            Frame::Single(render::compose_single(fallback, width, height)),
        ];
    }
    match layout {
        DisplayLayout::Single => {
            let first = images[0].clone();
            let second = images.get(1).cloned().unwrap_or_else(|| placeholder(1));
            [
                Frame::Single(render::compose_single(first, width, height)),
                Frame::Single(render::compose_single(second, width, height)),
            ]
        }
        DisplayLayout::Wall => {
            let count = images.len().clamp(2, WALL_POSTERS);
            let first: Vec<RgbImage> = (0..count)
                .map(|index| images[index % images.len()].clone())
                .collect();
            let second: Vec<RgbImage> = (0..count)
                .map(|index| images[(index + count / 2) % images.len()].clone())
                .collect();
            [
                compose_wall_or_single(first, width, height),
                compose_wall_or_single(second, width, height),
            ]
        }
    }
}

fn compose_wall_or_single(images: Vec<RgbImage>, width: u32, height: u32) -> Frame {
    render::compose_wall_frame(images, width, height)
        .unwrap_or_else(|_| Frame::Single(render::compose_single(placeholder(0), width, height)))
}

pub(crate) fn load_preview_images(cache: &Cache) -> Vec<RgbImage> {
    let mut cache = cache.clone();
    let mut images = Vec::new();
    for item in cache.offline_items(&[]) {
        if images.len() >= 12 {
            break;
        }
        let Some(path) = cache.get(&item) else {
            continue;
        };
        let Ok(image) = render::decode_file(&path) else {
            continue;
        };
        images.push(downscale(image, 360));
    }
    while images.len() < 2 {
        images.push(placeholder(images.len()));
    }
    images
}

fn downscale(image: RgbImage, limit: u32) -> RgbImage {
    let (width, height) = image.dimensions();
    let longest = width.max(height);
    if longest <= limit {
        return image;
    }
    let scale = limit as f32 / longest as f32;
    image::imageops::resize(
        &image,
        ((width as f32 * scale).round() as u32).max(1),
        ((height as f32 * scale).round() as u32).max(1),
        FilterType::Triangle,
    )
}

fn placeholder(index: usize) -> RgbImage {
    const COLORS: [[u8; 3]; 6] = [
        [34, 45, 74],
        [92, 48, 42],
        [44, 86, 56],
        [92, 78, 36],
        [64, 44, 92],
        [34, 82, 86],
    ];
    RgbImage::from_pixel(300, 450, image::Rgb(COLORS[index % COLORS.len()]))
}

/// `dst = dst * (1 - alpha) + src * alpha`，用于淡化和背景混合。
fn blend_over(dst: &mut [u32], src: &[u32], alpha: f32) {
    let weight = (alpha.clamp(0.0, 1.0) * 256.0).round() as u32;
    if weight == 0 {
        return;
    }
    if weight >= 256 {
        let len = dst.len().min(src.len());
        dst[..len].copy_from_slice(&src[..len]);
        return;
    }
    let inverse = 256 - weight;
    for (dst, src) in dst.iter_mut().zip(src.iter()) {
        let dr = (*dst >> 16) & 0xff;
        let dg = (*dst >> 8) & 0xff;
        let db = *dst & 0xff;
        let sr = (*src >> 16) & 0xff;
        let sg = (*src >> 8) & 0xff;
        let sb = *src & 0xff;
        let r = (dr * inverse + sr * weight + 128) >> 8;
        let g = (dg * inverse + sg * weight + 128) >> 8;
        let b = (db * inverse + sb * weight + 128) >> 8;
        *dst = (r << 16) | (g << 8) | b;
    }
}

/// 把整张画布按屏幕像素偏移覆盖到目标上，偏移外保留旧画面。
fn blit_canvas(canvas: &[u32], width: u32, height: u32, offset: (f32, f32), out: &mut [u32]) {
    let (w, h) = (width as i32, height as i32);
    let (ox, oy) = (offset.0.round() as i32, offset.1.round() as i32);
    if ox == 0 && oy == 0 {
        let len = out.len().min(canvas.len());
        out[..len].copy_from_slice(&canvas[..len]);
        return;
    }
    for sy in 0..h {
        let dy = sy + oy;
        if dy < 0 || dy >= h {
            continue;
        }
        let x0 = (-ox).max(0);
        let x1 = (w - ox).min(w);
        if x1 <= x0 {
            continue;
        }
        let len = (x1 - x0) as usize;
        let src_start = sy as usize * width as usize + x0 as usize;
        let dst_start = dy as usize * width as usize + (x0 + ox) as usize;
        if src_start + len <= canvas.len() && dst_start + len <= out.len() {
            out[dst_start..dst_start + len].copy_from_slice(&canvas[src_start..src_start + len]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: u32, height: u32, rgb: [u8; 3]) -> RgbImage {
        RgbImage::from_pixel(width, height, image::Rgb(rgb))
    }

    fn packed(rgb: [u8; 3]) -> u32 {
        ((rgb[0] as u32) << 16) | ((rgb[1] as u32) << 8) | rgb[2] as u32
    }

    #[test]
    fn blend_over_hits_endpoints_and_midpoint() {
        let mut dst = vec![packed([0, 0, 0]); 4];
        let src = vec![packed([255, 100, 50]); 4];
        blend_over(&mut dst, &src, 0.0);
        assert!(dst.iter().all(|value| *value == 0));
        blend_over(&mut dst, &src, 1.0);
        assert!(dst.iter().all(|value| *value == packed([255, 100, 50])));

        // 从黑色向目标色混合一半。
        let mut halfway = vec![0u32; 4];
        blend_over(&mut halfway, &src, 0.5);
        let value = halfway[0];
        assert!((((value >> 16) & 0xff) as i32 - 128).abs() <= 1);
        assert!((((value >> 8) & 0xff) as i32 - 50).abs() <= 1);
        assert!((((value & 0xff) as i32) - 25).abs() <= 1);
    }

    #[test]
    fn blit_canvas_clips_offsets_and_keeps_old_pixels() {
        let width = 6u32;
        let height = 2u32;
        let canvas: Vec<u32> = (0..width * height).map(|index| index + 1).collect();
        let mut out = vec![99u32; (width * height) as usize];

        blit_canvas(&canvas, width, height, (0.0, 0.0), &mut out);
        assert_eq!(out, canvas);

        let mut shifted = vec![99u32; (width * height) as usize];
        blit_canvas(&canvas, width, height, (2.0, 0.0), &mut shifted);
        assert_eq!(shifted[0], 99);
        assert_eq!(shifted[1], 99);
        assert_eq!(shifted[2], 1);
        assert_eq!(shifted[3], 2);

        let mut off = vec![99u32; (width * height) as usize];
        blit_canvas(&canvas, width, height, (width as f32, 0.0), &mut off);
        assert!(off.iter().all(|value| *value == 99));
    }

    #[test]
    fn build_scenes_matches_layout() {
        let images: Vec<RgbImage> = (0..6)
            .map(|index| solid(40, 60, [(index * 30) as u8, 80, 120]))
            .collect();
        let [first, second] = build_scenes(&images, DisplayLayout::Single, 320, 180);
        assert!(matches!(first, Frame::Single(_)));
        assert!(matches!(second, Frame::Single(_)));

        let [first, second] = build_scenes(&images, DisplayLayout::Wall, 320, 180);
        match (&first, &second) {
            (Frame::Wall(a), Frame::Wall(b)) => {
                assert_eq!(a.cards, 6);
                assert_eq!(b.cards, 6);
            }
            _ => panic!("6 张应组成海报墙"),
        }
    }

    #[test]
    fn static_preview_does_not_change_within_hold() {
        let images = vec![solid(40, 60, [200, 40, 40]), solid(40, 60, [40, 200, 40])];
        let settings = DisplaySettings {
            display_layout: DisplayLayout::Single,
            transition: crate::config::Transition::SlideLeft,
            transition_seconds: 1.0,
            display_seconds: 3.0,
            motion_mode: MotionMode::Static,
            ken_burns: true,
        };
        let mut preview = PreviewState::new(settings, images, 160, 90);
        preview.epoch = Instant::now();
        let epoch = preview.epoch;
        preview.update_frame(epoch);
        let first = preview.frame.clone();
        preview.update_frame(epoch + std::time::Duration::from_millis(900));
        assert_eq!(preview.frame, first, "静止模式停留段内不应有逐帧变化");
    }
}
