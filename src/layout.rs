//! 海报墙几何：固定 6 张、3 列×2 行，短列表自动换网格。
//!
//! 这里只算位置，不做解码/缩放；绘制在 `render` 里完成，测试可以直接验证
//! 单元格边界、宽高比和非重叠。

/// 海报墙最多显示的条目数。
pub const WALL_POSTERS: usize = 6;
/// 边距至少这么多像素，避免高 DPI 小窗口贴边。
const MIN_MARGIN: u32 = 12;
/// 卡片间距至少这么多像素。
const MIN_GAP: u32 = 8;
/// 边距相对较短边的比例。
const MARGIN_RATIO: f32 = 0.02;
/// 卡片间距相对较短边的比例。
const GAP_RATIO: f32 = 0.012;

/// 一个海报格子（屏幕像素）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// 海报数量对应的列数/行数；只有 2–6 张会用到海报墙。
pub fn grid_for(count: usize) -> (u32, u32) {
    match count {
        0 | 1 => (1, 1),
        2 => (2, 1),
        3 => (3, 1),
        4 => (2, 2),
        _ => (3, 2),
    }
}

pub fn margin_for(width: u32, height: u32) -> u32 {
    let short = width.min(height) as f32;
    ((short * MARGIN_RATIO).round() as u32).max(MIN_MARGIN)
}

pub fn gap_for(width: u32, height: u32) -> u32 {
    let short = width.min(height) as f32;
    ((short * GAP_RATIO).round() as u32).max(MIN_GAP)
}

/// 计算每张海报的格子。行内数量不足时整行居中，不画空卡占位。
pub fn cells_for(count: usize, width: u32, height: u32) -> Vec<Cell> {
    if count == 0 || width == 0 || height == 0 {
        return Vec::new();
    }
    let count = count.min(WALL_POSTERS);
    let (cols, rows) = grid_for(count);
    let margin = margin_for(width, height);
    let gap = gap_for(width, height);

    // 极小的画布上边距也不能把可用区域吃成 0。
    let max_margin_x = width.saturating_sub(1) / 2;
    let max_margin_y = height.saturating_sub(1) / 2;
    let margin_x = margin.min(max_margin_x);
    let margin_y = margin.min(max_margin_y);

    let usable_w = width
        .saturating_sub(margin_x * 2 + gap * cols.saturating_sub(1))
        .max(cols);
    let usable_h = height
        .saturating_sub(margin_y * 2 + gap * rows.saturating_sub(1))
        .max(rows);
    let cell_w = (usable_w / cols).max(1);
    let cell_h = (usable_h / rows).max(1);

    let grid_h = cell_h * rows + gap * rows.saturating_sub(1);
    let y0 = height.saturating_sub(grid_h) / 2;

    let mut cells = Vec::with_capacity(count);
    for row in 0..rows {
        let used = (count as u32).saturating_sub(row * cols).min(cols);
        if used == 0 {
            break;
        }
        let row_w = cell_w * used + gap * used.saturating_sub(1);
        let x0 = width.saturating_sub(row_w) / 2;
        for col in 0..used {
            cells.push(Cell {
                x: x0 + col * (cell_w + gap),
                y: y0 + row * (cell_h + gap),
                width: cell_w,
                height: cell_h,
            });
        }
    }
    cells
}

/// 把图片按比例 fit 进格子，返回缩放后的像素尺寸（不变形、不裁切）。
pub fn fit_in_cell(image_w: u32, image_h: u32, cell: &Cell) -> (u32, u32) {
    if image_w == 0 || image_h == 0 || cell.width == 0 || cell.height == 0 {
        return (1, 1);
    }
    let scale = (cell.width as f32 / image_w as f32).min(cell.height as f32 / image_h as f32);
    (
        ((image_w as f32 * scale).round() as u32).max(1),
        ((image_h as f32 * scale).round() as u32).max(1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_dimensions_follow_short_lists() {
        assert_eq!(grid_for(0), (1, 1));
        assert_eq!(grid_for(1), (1, 1));
        assert_eq!(grid_for(2), (2, 1));
        assert_eq!(grid_for(3), (3, 1));
        assert_eq!(grid_for(4), (2, 2));
        assert_eq!(grid_for(5), (3, 2));
        assert_eq!(grid_for(6), (3, 2));
        assert_eq!(grid_for(9), (3, 2), "超过 6 张也只排 6 格");
    }

    #[test]
    fn zero_posters_produce_no_cells() {
        assert!(cells_for(0, 1920, 1080).is_empty());
        assert!(cells_for(3, 0, 1080).is_empty());
        assert!(cells_for(3, 1920, 0).is_empty());
    }

    #[test]
    fn cells_stay_inside_screen_and_never_overlap() {
        for (width, height) in [(1280, 720), (1920, 1080), (3840, 2160)] {
            for count in 2..=WALL_POSTERS {
                let cells = cells_for(count, width, height);
                assert_eq!(cells.len(), count, "{count} 张海报应有 {count} 个格子");

                for cell in &cells {
                    assert!(cell.width > 0 && cell.height > 0);
                    assert!(
                        cell.x + cell.width <= width,
                        "格子越出右边界: {cell:?} @ {width}x{height}"
                    );
                    assert!(
                        cell.y + cell.height <= height,
                        "格子越出下边界: {cell:?} @ {width}x{height}"
                    );
                }

                for (i, a) in cells.iter().enumerate() {
                    for b in cells.iter().skip(i + 1) {
                        let overlap_x = a.x < b.x + b.width && b.x < a.x + a.width;
                        let overlap_y = a.y < b.y + b.height && b.y < a.y + a.height;
                        assert!(
                            !(overlap_x && overlap_y),
                            "{count} 张时格子重叠: {a:?} / {b:?} @ {width}x{height}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn six_posters_use_three_by_two() {
        let cells = cells_for(6, 1920, 1080);
        assert_eq!(cells.len(), 6);
        // 第一行 3 个的 y 相同，第二行 3 个的 y 相同且更靠下。
        assert_eq!(cells[0].y, cells[1].y);
        assert_eq!(cells[1].y, cells[2].y);
        assert!(cells[3].y > cells[0].y);
        assert_eq!(cells[3].x, cells[0].x);
        assert_eq!(cells[3].width, cells[0].width);
        assert_eq!(cells[3].height, cells[0].height);
    }

    #[test]
    fn five_posters_center_the_last_row() {
        let cells = cells_for(5, 1920, 1080);
        assert_eq!(cells.len(), 5);
        // 第二行只有 2 张，应该比第一行的 3 张更靠中间（左右各缩进约半个单元）。
        let left_gap = cells[3].x;
        let right_gap = 1920 - (cells[4].x + cells[4].width);
        assert!(
            left_gap.abs_diff(right_gap) <= 1,
            "末行应按整行居中: 左 {left_gap} / 右 {right_gap}"
        );
        assert!(left_gap > cells[0].x, "只有两张时左右留白更大");
    }

    #[test]
    fn margins_and_gaps_scale_but_keep_minimums() {
        assert_eq!(margin_for(320, 240), MIN_MARGIN);
        assert_eq!(gap_for(320, 240), MIN_GAP);
        assert!(margin_for(3840, 2160) > margin_for(1920, 1080));
        assert!(gap_for(3840, 2160) > gap_for(1920, 1080));
    }

    #[test]
    fn fit_preserves_aspect_ratio() {
        let cell = Cell {
            x: 0,
            y: 0,
            width: 400,
            height: 600,
        };
        // 竖版海报：高度受限，宽度按比例缩小。
        let (w, h) = fit_in_cell(680, 1000, &cell);
        assert!(h <= cell.height && w <= cell.width);
        let source_ratio = 680.0 / 1000.0;
        let fit_ratio = w as f32 / h as f32;
        assert!(
            (source_ratio - fit_ratio).abs() < 0.01,
            "宽高比应保持: {fit_ratio} vs {source_ratio}"
        );

        // 横版图：宽度受限。
        let (w, h) = fit_in_cell(2000, 1000, &cell);
        assert_eq!(w, cell.width);
        assert!(h < cell.height);
    }

    #[test]
    fn tiny_canvas_still_gets_usable_cells() {
        let cells = cells_for(6, 96, 96);
        assert_eq!(cells.len(), 6);
        for cell in &cells {
            assert!(cell.width >= 1 && cell.height >= 1);
            assert!(cell.x + cell.width <= 96);
            assert!(cell.y + cell.height <= 96);
        }
    }
}
