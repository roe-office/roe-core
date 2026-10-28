//! 条件格式 IconSet 渲染：17 种 Excel 预设图标集
//!
//! 参考 Excel OOXML ST_IconSetType 完整枚举（17 种），图标形状语义
//! 参考 univer（packages/sheets-conditional-formatting）的图标分组方案
//! （箭头/形状/标记/评级四大类），具体几何图形自行用 tiny-skia
//! PathBuilder 绘制矢量图形（不依赖外部图标资源文件/位图）。
//!
//! 分档逻辑：每种预设对应 N 档（3/4/5），cfvo 阈值决定数值落在哪一档，
//! 复用 cond_format_render.rs 已有的 resolve_cfvo 阈值计算逻辑。

use tiny_skia::{Paint, PathBuilder, PixmapMut, Rect, Transform};

use crate::oo_icon_paths::{icon_by_id, preset_icon_ids, IconPath};
use crate::tile::{col_width_at_px, col_x_px, row_height_at_px, row_y_px, CalcRenderConfig};
use roe_model::calc::{CellValue, CfValue, Sheet};

// ============================================================
// 旧版手绘IconSet实现（v132会话起停用，改用 oo_icon_paths.rs 的
// OnlyOffice矢量数据 + draw_icon_by_id）。整块保留、未删除，
// 将来若需要脱离OnlyOffice数据自绘图标，可作为参考基础。
// ============================================================
#[allow(dead_code)]
mod legacy_hand_drawn_icons {
use super::*;

// /// 图标语义类别（对应 univer icon-map.ts 的分组：arrow/shape/mark/rank）
// #[derive(Debug, Clone, Copy, PartialEq)]
// pub enum IconGlyph {
//     /// 箭头：方向由 dir 决定（-1=下，0=右/持平，1=上，2=右上，-2=右下）
//     Arrow { dir: i8, gray: bool },
//     /// 实心圆点（交通灯/评级）
//     Circle,
//     /// 实心三角（3Signs 的"上升"图标 / 3Triangles）
//     Triangle,
//     /// 菱形（3Signs 的"警示"图标）
//     Diamond,
//     /// 三角旗
//     Flag,
//     /// 对勾
//     Check,
//     /// 感叹号
//     Exclaim,
//     /// 叉
//     Cross,
//     /// 五角星，fill 为填充比例：1.0 实心 / 0.5 半空心 / 0.0 空心
//     Star { fill: f32 },
//     /// 饼状扇形（5Quarters），fill 为填充比例 0.0~1.0（对应 0/25/50/75/100%）
//     QuarterPie { fill: f32 },
//     /// 信号强度柱状条（4Rating/5Rating），level 是点亮的柱子数，total 是总柱子数
//     SignalBars { level: u8, total: u8 },
//     /// 小方块网格（5Boxes），level 是点亮的方块数（满 4 格）
//     Boxes { level: u8 },
// }
//
// /// 颜色语义：绿/黄（金）/红/灰/深灰/紫（信号条用）/无描边留白
// #[derive(Debug, Clone, Copy)]
// pub struct IconColor(pub u8, pub u8, pub u8);
//
// pub const COLOR_GREEN: IconColor = IconColor(0x3d, 0xb8, 0x75);
// pub const COLOR_GOLD: IconColor = IconColor(0xf5, 0xb7, 0x2e);
// pub const COLOR_RED: IconColor = IconColor(0xf0, 0x6f, 0x6f);
// pub const COLOR_GRAY: IconColor = IconColor(0xb0, 0xb0, 0xba);
// pub const COLOR_DARK_GRAY: IconColor = IconColor(0x55, 0x55, 0x5a);
// pub const COLOR_BLACK: IconColor = IconColor(0x2b, 0x2b, 0x2e);
// pub const COLOR_BLUE: IconColor = IconColor(0x5b, 0x5b, 0xd6);
// pub const COLOR_PINK: IconColor = IconColor(0xe0, 0x8a, 0xa8);
// pub const COLOR_WHITE: IconColor = IconColor(0xff, 0xff, 0xff);
//
// /// 一个 icon set 档位：图标语义 + 颜色
// #[derive(Debug, Clone, Copy)]
// pub struct IconSpec {
//     pub glyph: IconGlyph,
//     pub color: IconColor,
// }
//
// const fn spec(glyph: IconGlyph, color: IconColor) -> IconSpec {
//     IconSpec { glyph, color }
// }
//
// /// 17 种 Excel 预设 → 图标序列（低档在前，高档在后，与 cfvo 阈值顺序一致）。
// /// reverse=true 时由调用方负责翻转序列，这里始终按"从低到高"的自然顺序定义。
// pub fn preset_icons(style: &str) -> Vec<IconSpec> {
//     use IconGlyph::*;
//     match style {
//         "3Arrows" => vec![
//             spec(Arrow { dir: -1, gray: false }, COLOR_RED),
//             spec(Arrow { dir: 0, gray: false }, COLOR_GOLD),
//             spec(Arrow { dir: 1, gray: false }, COLOR_GREEN),
//         ],
//         "3ArrowsGray" => vec![
//             spec(Arrow { dir: -1, gray: true }, COLOR_GRAY),
//             spec(Arrow { dir: 0, gray: true }, COLOR_GRAY),
//             spec(Arrow { dir: 1, gray: true }, COLOR_GRAY),
//         ],
//         "3Triangles" => vec![
//             spec(Triangle, COLOR_RED),
//             spec(Circle, COLOR_GOLD), // "cross"档用圆点近似持平语义
//             spec(Triangle, COLOR_GREEN),
//         ],
//         "3TrafficLights1" => vec![
//             spec(Circle, COLOR_RED),
//             spec(Circle, COLOR_GOLD),
//             spec(Circle, COLOR_GREEN),
//         ],
//         "3TrafficLights2" => vec![
//             spec(Circle, COLOR_RED),
//             spec(Circle, COLOR_GOLD),
//             spec(Circle, COLOR_GREEN),
//         ],
//         "3Signs" => vec![
//             spec(Diamond, COLOR_RED),
//             spec(Triangle, COLOR_GOLD),
//             spec(Circle, COLOR_GREEN),
//         ],
//         "3Symbols" | "3Symbols2" => vec![
//             spec(Cross, COLOR_RED),
//             spec(Exclaim, COLOR_GOLD),
//             spec(Check, COLOR_GREEN),
//         ],
//         "3Flags" => vec![
//             spec(Flag, COLOR_RED),
//             spec(Flag, COLOR_GOLD),
//             spec(Flag, COLOR_GREEN),
//         ],
//         "3Stars" => vec![
//             spec(Star { fill: 0.0 }, COLOR_GOLD),
//             spec(Star { fill: 0.5 }, COLOR_GOLD),
//             spec(Star { fill: 1.0 }, COLOR_GOLD),
//         ],
//         "4Arrows" => vec![
//             spec(Arrow { dir: -1, gray: false }, COLOR_RED),
//             spec(Arrow { dir: -2, gray: false }, COLOR_GOLD),
//             spec(Arrow { dir: 2, gray: false }, COLOR_GOLD),
//             spec(Arrow { dir: 1, gray: false }, COLOR_GREEN),
//         ],
//         "4ArrowsGray" => vec![
//             spec(Arrow { dir: -1, gray: true }, COLOR_GRAY),
//             spec(Arrow { dir: -2, gray: true }, COLOR_GRAY),
//             spec(Arrow { dir: 2, gray: true }, COLOR_GRAY),
//             spec(Arrow { dir: 1, gray: true }, COLOR_GRAY),
//         ],
//         "4RedToBlack" => vec![
//             spec(Circle, COLOR_RED),
//             spec(Circle, COLOR_PINK),
//             spec(Circle, COLOR_GRAY),
//             spec(Circle, COLOR_BLACK),
//         ],
//         "4TrafficLights" => vec![
//             spec(Circle, COLOR_RED),
//             spec(Circle, COLOR_GOLD),
//             spec(Circle, COLOR_GREEN),
//             spec(Circle, COLOR_BLACK),
//         ],
//         "4Rating" => vec![
//             spec(SignalBars { level: 1, total: 4 }, COLOR_BLUE),
//             spec(SignalBars { level: 2, total: 4 }, COLOR_BLUE),
//             spec(SignalBars { level: 3, total: 4 }, COLOR_BLUE),
//             spec(SignalBars { level: 4, total: 4 }, COLOR_BLUE),
//         ],
//         "5Arrows" => vec![
//             spec(Arrow { dir: -1, gray: false }, COLOR_RED),
//             spec(Arrow { dir: -2, gray: false }, COLOR_GOLD),
//             spec(Arrow { dir: 0, gray: false }, COLOR_GOLD),
//             spec(Arrow { dir: 2, gray: false }, COLOR_GOLD),
//             spec(Arrow { dir: 1, gray: false }, COLOR_GREEN),
//         ],
//         "5ArrowsGray" => vec![
//             spec(Arrow { dir: -1, gray: true }, COLOR_GRAY),
//             spec(Arrow { dir: -2, gray: true }, COLOR_GRAY),
//             spec(Arrow { dir: 0, gray: true }, COLOR_GRAY),
//             spec(Arrow { dir: 2, gray: true }, COLOR_GRAY),
//             spec(Arrow { dir: 1, gray: true }, COLOR_GRAY),
//         ],
//         "5Rating" => vec![
//             spec(SignalBars { level: 1, total: 5 }, COLOR_BLUE),
//             spec(SignalBars { level: 2, total: 5 }, COLOR_BLUE),
//             spec(SignalBars { level: 3, total: 5 }, COLOR_BLUE),
//             spec(SignalBars { level: 4, total: 5 }, COLOR_BLUE),
//             spec(SignalBars { level: 5, total: 5 }, COLOR_BLUE),
//         ],
//         "5Quarters" => vec![
//             spec(QuarterPie { fill: 0.0 }, COLOR_GRAY),
//             spec(QuarterPie { fill: 0.25 }, COLOR_GRAY),
//             spec(QuarterPie { fill: 0.5 }, COLOR_GRAY),
//             spec(QuarterPie { fill: 0.75 }, COLOR_GRAY),
//             spec(QuarterPie { fill: 1.0 }, COLOR_GRAY),
//         ],
//         "5Boxes" => vec![
//             spec(Boxes { level: 0 }, COLOR_GRAY),
//             spec(Boxes { level: 1 }, COLOR_GRAY),
//             spec(Boxes { level: 2 }, COLOR_GRAY),
//             spec(Boxes { level: 3 }, COLOR_GRAY),
//             spec(Boxes { level: 4 }, COLOR_GRAY),
//         ],
//         // 未知/自定义预设：兜底为 3TrafficLights1，避免 panic 或空白
//         _ => vec![
//             spec(Circle, COLOR_RED),
//             spec(Circle, COLOR_GOLD),
//             spec(Circle, COLOR_GREEN),
//         ],
//     }
// }
//
// /// 在给定的正方形区域 (cx, cy, size) 内画一个图标。cx/cy 是图标中心点，
// /// size 是图标的外接正方形边长。
// pub fn draw_icon(
//     pixmap: &mut PixmapMut,
//     tx: Transform,
//     icon: &IconSpec,
//     cx: f32,
//     cy: f32,
//     size: f32,
// ) {
//     let mut paint = Paint::default();
//     paint.anti_alias = true;
//     let IconColor(r, g, b) = icon.color;
//     paint.set_color_rgba8(r, g, b, 0xff);
//
//     let half = size / 2.0;
//
//     match icon.glyph {
//         IconGlyph::Arrow { dir, gray: _ } => draw_arrow(pixmap, tx, &paint, cx, cy, half, dir),
//         IconGlyph::Circle => draw_circle(pixmap, tx, &paint, cx, cy, half),
//         IconGlyph::Triangle => draw_triangle(pixmap, tx, &paint, cx, cy, half, true),
//         IconGlyph::Diamond => draw_diamond(pixmap, tx, &paint, cx, cy, half),
//         IconGlyph::Flag => draw_flag(pixmap, tx, &paint, cx, cy, half),
//         IconGlyph::Check => draw_check(pixmap, tx, &paint, cx, cy, half),
//         IconGlyph::Exclaim => draw_exclaim(pixmap, tx, &paint, cx, cy, half),
//         IconGlyph::Cross => draw_cross(pixmap, tx, &paint, cx, cy, half),
//         IconGlyph::Star { fill } => draw_star(pixmap, tx, &paint, cx, cy, half, fill),
//         IconGlyph::QuarterPie { fill } => draw_quarter_pie(pixmap, tx, &paint, cx, cy, half, fill),
//         IconGlyph::SignalBars { level, total } => {
//             draw_signal_bars(pixmap, tx, &paint, cx, cy, half, level, total)
//         }
//         IconGlyph::Boxes { level } => draw_boxes(pixmap, tx, &paint, cx, cy, half, level),
//     }
// }
//
// fn fill_path(pixmap: &mut PixmapMut, tx: Transform, paint: &Paint, pb: PathBuilder) {
//     if let Some(path) = pb.finish() {
//         pixmap.fill_path(&path, paint, tiny_skia::FillRule::Winding, tx, None);
//     }
// }
//
// /// 圆点：交通灯/评级常用
// fn draw_circle(pixmap: &mut PixmapMut, tx: Transform, paint: &Paint, cx: f32, cy: f32, half: f32) {
//     if let Some(path) = tiny_skia::PathBuilder::from_circle(cx, cy, half * 0.85) {
//         pixmap.fill_path(&path, paint, tiny_skia::FillRule::Winding, tx, None);
//     }
// }
//
// /// 箭头：dir = 1(上)/-1(下)/0(右，持平)/2(右上)/-2(右下)。
// /// 用一个三角形箭头头 + 矩形箭身组合，整体指向 dir 对应的方向。
// fn draw_arrow(pixmap: &mut PixmapMut, tx: Transform, paint: &Paint, cx: f32, cy: f32, half: f32, dir: i8) {
//     // 先在"朝上"的局部坐标系里定义箭头形状（頂点在上），再按 dir 旋转。
//     let angle_deg: f32 = match dir {
//         1 => 0.0,      // 上
//         -1 => 180.0,   // 下
//         0 => 90.0,     // 右（持平）
//         2 => 45.0,     // 右上
//         -2 => 135.0,   // 右下
//         _ => 0.0,
//     };
//
//     let w = half * 0.7;   // 箭身宽度的一半
//     let head_w = half * 1.0; // 箭头宽度的一半
//     let head_h = half * 0.9; // 箭头高度
//     let body_h = half * 0.9; // 箭身长度
//     let rad = angle_deg.to_radians();
//     let (s, c) = rad.sin_cos();
//
//     // 局部坐标（相对中心 0,0，朝上为 -y 方向）→ 世界坐标的旋转函数
//     let rot = |lx: f32, ly: f32| -> (f32, f32) {
//         // 旋转 + 平移到 (cx, cy)
//         (cx + lx * c - ly * s, cy + lx * s + ly * c)
//     };
//
//     let mut pb = PathBuilder::new();
//     // 箭头三角形头部：顶点在最上方 (0, -half)，底边在 (±head_w, -half+head_h)
//     let (tx0, ty0) = rot(0.0, -half);
//     let (tx1, ty1) = rot(-head_w, -half + head_h);
//     let (tx2, ty2) = rot(head_w, -half + head_h);
//     pb.move_to(tx0, ty0);
//     pb.line_to(tx1, ty1);
//     pb.line_to(tx2, ty2);
//     pb.close();
//
//     // 箭身矩形：从头部底边到底部
//     let (bx0, by0) = rot(-w, -half + head_h);
//     let (bx1, by1) = rot(w, -half + head_h);
//     let (bx2, by2) = rot(w, -half + head_h + body_h);
//     let (bx3, by3) = rot(-w, -half + head_h + body_h);
//     pb.move_to(bx0, by0);
//     pb.line_to(bx1, by1);
//     pb.line_to(bx2, by2);
//     pb.line_to(bx3, by3);
//     pb.close();
//
//     fill_path(pixmap, tx, paint, pb);
// }
//
// /// 实心三角形（尖朝上）
// fn draw_triangle(pixmap: &mut PixmapMut, tx: Transform, paint: &Paint, cx: f32, cy: f32, half: f32, up: bool) {
//     let mut pb = PathBuilder::new();
//     let h = half * 0.85;
//     if up {
//         pb.move_to(cx, cy - h);
//         pb.line_to(cx - h, cy + h * 0.8);
//         pb.line_to(cx + h, cy + h * 0.8);
//     } else {
//         pb.move_to(cx, cy + h);
//         pb.line_to(cx - h, cy - h * 0.8);
//         pb.line_to(cx + h, cy - h * 0.8);
//     }
//     pb.close();
//     fill_path(pixmap, tx, paint, pb);
// }
//
// /// 菱形
// fn draw_diamond(pixmap: &mut PixmapMut, tx: Transform, paint: &Paint, cx: f32, cy: f32, half: f32) {
//     let h = half * 0.85;
//     let mut pb = PathBuilder::new();
//     pb.move_to(cx, cy - h);
//     pb.line_to(cx + h, cy);
//     pb.line_to(cx, cy + h);
//     pb.line_to(cx - h, cy);
//     pb.close();
//     fill_path(pixmap, tx, paint, pb);
// }
//
// /// 三角旗：竖杆 + 三角旗面
// fn draw_flag(pixmap: &mut PixmapMut, tx: Transform, paint: &Paint, cx: f32, cy: f32, half: f32) {
//     let pole_x = cx - half * 0.6;
//     let mut pb = PathBuilder::new();
//     // 旗杆（细矩形）
//     if let Some(rect) = Rect::from_xywh(pole_x - half * 0.08, cy - half * 0.9, half * 0.16, half * 1.8) {
//         pb.push_rect(rect);
//     }
//     // 旗面（三角形，挂在杆的上半部分右侧）
//     pb.move_to(pole_x, cy - half * 0.9);
//     pb.line_to(pole_x + half * 1.3, cy - half * 0.45);
//     pb.line_to(pole_x, cy);
//     pb.close();
//     fill_path(pixmap, tx, paint, pb);
// }
//
// /// 对勾 ✓：用两段粗折线（折线转minimal多边形）实现
// fn draw_check(pixmap: &mut PixmapMut, tx: Transform, paint: &Paint, cx: f32, cy: f32, half: f32) {
//     let stroke_w = half * 0.35;
//     let p1 = (cx - half * 0.8, cy + half * 0.05);
//     let p2 = (cx - half * 0.2, cy + half * 0.7);
//     let p3 = (cx + half * 0.9, cy - half * 0.7);
//
//     draw_thick_segment(pixmap, tx, paint, p1, p2, stroke_w);
//     draw_thick_segment(pixmap, tx, paint, p2, p3, stroke_w);
// }
//
// /// 叉 ✗：两条交叉粗线
// fn draw_cross(pixmap: &mut PixmapMut, tx: Transform, paint: &Paint, cx: f32, cy: f32, half: f32) {
//     let stroke_w = half * 0.3;
//     let d = half * 0.7;
//     draw_thick_segment(pixmap, tx, paint, (cx - d, cy - d), (cx + d, cy + d), stroke_w);
//     draw_thick_segment(pixmap, tx, paint, (cx - d, cy + d), (cx + d, cy - d), stroke_w);
// }
//
// /// 感叹号：竖条 + 底部圆点
// fn draw_exclaim(pixmap: &mut PixmapMut, tx: Transform, paint: &Paint, cx: f32, cy: f32, half: f32) {
//     let bar_w = half * 0.3;
//     let mut pb = PathBuilder::new();
//     if let Some(rect) = Rect::from_xywh(cx - bar_w / 2.0, cy - half * 0.85, bar_w, half * 1.1) {
//         pb.push_rect(rect);
//     }
//     fill_path(pixmap, tx, paint, pb);
//
//     if let Some(path) = PathBuilder::from_circle(cx, cy + half * 0.65, bar_w * 0.6) {
//         pixmap.fill_path(&path, paint, tiny_skia::FillRule::Winding, tx, None);
//     }
// }
//
// /// 画一条有宽度的线段（用两端各自往垂直方向偏移，构成一个矩形/胶囊近似）
// fn draw_thick_segment(
//     pixmap: &mut PixmapMut,
//     tx: Transform,
//     paint: &Paint,
//     (x0, y0): (f32, f32),
//     (x1, y1): (f32, f32),
//     width: f32,
// ) {
//     let dx = x1 - x0;
//     let dy = y1 - y0;
//     let len = (dx * dx + dy * dy).sqrt().max(1e-6);
//     let nx = -dy / len * (width / 2.0);
//     let ny = dx / len * (width / 2.0);
//
//     let mut pb = PathBuilder::new();
//     pb.move_to(x0 + nx, y0 + ny);
//     pb.line_to(x1 + nx, y1 + ny);
//     pb.line_to(x1 - nx, y1 - ny);
//     pb.line_to(x0 - nx, y0 - ny);
//     pb.close();
//     fill_path(pixmap, tx, paint, pb);
// }
//
// /// 五角星：10 个点交替内外半径。fill 控制"实心/半空心/空心"——
// /// 0.0 只画描边轮廓（用极细的填充近似描边，避免引入 stroke API 依赖），
// /// 0.5 画一半（左半实心），1.0 整体实心。
// fn draw_star(pixmap: &mut PixmapMut, tx: Transform, paint: &Paint, cx: f32, cy: f32, half: f32, fill: f32) {
//     let outer = half * 0.95;
//     let inner = outer * 0.42;
//     let mut points = Vec::with_capacity(10);
//     for i in 0..10 {
//         let angle = std::f32::consts::PI / 2.0 * 3.0 + (i as f32) * std::f32::consts::PI / 5.0;
//         let r = if i % 2 == 0 { outer } else { inner };
//         points.push((cx + r * angle.cos(), cy - r * angle.sin()));
//     }
//
//     if fill >= 0.99 {
//         let mut pb = PathBuilder::new();
//         pb.move_to(points[0].0, points[0].1);
//         for &(x, y) in &points[1..] {
//             pb.line_to(x, y);
//         }
//         pb.close();
//         fill_path(pixmap, tx, paint, pb);
//     } else if fill <= 0.01 {
//         // 空心：画一个很细的轮廓近似（内外两条边界之间的窄环）
//         let mut pb = PathBuilder::new();
//         pb.move_to(points[0].0, points[0].1);
//         for &(x, y) in &points[1..] {
//             pb.line_to(x, y);
//         }
//         pb.close();
//         // 内缩 88% 再画一次相反方向，形成镂空描边（even-odd 近似）
//         let inset: Vec<(f32, f32)> = points.iter().map(|&(x, y)| {
//             (cx + (x - cx) * 0.82, cy + (y - cy) * 0.82)
//         }).collect();
//         pb.move_to(inset[0].0, inset[0].1);
//         for &(x, y) in inset[1..].iter().rev() {
//             pb.line_to(x, y);
//         }
//         pb.close();
//         if let Some(path) = pb.finish() {
//             pixmap.fill_path(&path, paint, tiny_skia::FillRule::EvenOdd, tx, None);
//         }
//     } else {
//         // 半空心：左半用实心多边形裁剪近似——直接画整颗星，
//         // 再画一个白色矩形盖住右半（简化实现，视觉上足够区分档位）。
//         let mut pb = PathBuilder::new();
//         pb.move_to(points[0].0, points[0].1);
//         for &(x, y) in &points[1..] {
//             pb.line_to(x, y);
//         }
//         pb.close();
//         fill_path(pixmap, tx, paint, pb);
//
//         let mut white = Paint::default();
//         white.anti_alias = true;
//         white.set_color_rgba8(255, 255, 255, 0xff);
//         let mut pb2 = PathBuilder::new();
//         if let Some(rect) = Rect::from_xywh(cx, cy - half, half, half * 2.0) {
//             pb2.push_rect(rect);
//         }
//         fill_path(pixmap, tx, &white, pb2);
//     }
// }
//
// /// 扇形饼图（5Quarters）：fill 是填充比例 0.0~1.0，从 12 点方向顺时针填充。
// /// 先画一个空心圆环轮廓（浅灰描边近似：小圆环 even-odd），再叠加填充扇形。
// fn draw_quarter_pie(pixmap: &mut PixmapMut, tx: Transform, paint: &Paint, cx: f32, cy: f32, half: f32, fill: f32) {
//     let r = half * 0.85;
//
//     // 外圈轮廓（用于 fill=0 时也能看到"空心圆"的档位）
//     let mut outline_paint = Paint::default();
//     outline_paint.anti_alias = true;
//     outline_paint.set_color_rgba8(0xc0, 0xc0, 0xc8, 0xff);
//     if let Some(path) = PathBuilder::from_circle(cx, cy, r) {
//         pixmap.fill_path(&path, &outline_paint, tiny_skia::FillRule::Winding, tx, None);
//     }
//
//     if fill <= 0.01 {
//         // 纯空心：整体已经是浅灰圆，中间再挖一个白色小圆做"环"效果
//         let mut white = Paint::default();
//         white.anti_alias = true;
//         white.set_color_rgba8(255, 255, 255, 0xff);
//         if let Some(path) = PathBuilder::from_circle(cx, cy, r * 0.78) {
//             pixmap.fill_path(&path, &white, tiny_skia::FillRule::Winding, tx, None);
//         }
//         return;
//     }
//
//     if fill >= 0.99 {
//         if let Some(path) = PathBuilder::from_circle(cx, cy, r) {
//             pixmap.fill_path(&path, paint, tiny_skia::FillRule::Winding, tx, None);
//         }
//         return;
//     }
//
//     // 部分填充：从 12 点方向开始，顺时针画 fill*360 度的扇形
//     let start_angle = -std::f32::consts::FRAC_PI_2; // 12点方向
//     let sweep = fill * std::f32::consts::TAU;
//     let steps = 24;
//     let mut pb = PathBuilder::new();
//     pb.move_to(cx, cy);
//     for i in 0..=steps {
//         let t = start_angle + sweep * (i as f32 / steps as f32);
//         pb.line_to(cx + r * t.cos(), cy + r * t.sin());
//     }
//     pb.close();
//     fill_path(pixmap, tx, paint, pb);
// }
//
// /// 信号强度柱状条（4Rating/5Rating）：level 根柱子点亮（深色），其余浅灰。
// fn draw_signal_bars(
//     pixmap: &mut PixmapMut,
//     tx: Transform,
//     paint: &Paint,
//     cx: f32,
//     cy: f32,
//     half: f32,
//     level: u8,
//     total: u8,
// ) {
//     let bar_w = half * 1.6 / total as f32 * 0.7;
//     let gap = half * 1.6 / total as f32 * 0.3;
//     let total_w = (bar_w + gap) * total as f32 - gap;
//     let start_x = cx - total_w / 2.0;
//
//     let mut light_paint = Paint::default();
//     light_paint.anti_alias = true;
//     light_paint.set_color_rgba8(0xd8, 0xd8, 0xe0, 0xff);
//
//     for i in 0..total {
//         let bar_h = half * 1.6 * ((i + 1) as f32 / total as f32);
//         let x = start_x + i as f32 * (bar_w + gap);
//         let y = cy + half * 0.8 - bar_h;
//         let use_paint = if i < level { paint } else { &light_paint };
//         let mut pb = PathBuilder::new();
//         if let Some(rect) = Rect::from_xywh(x, y, bar_w, bar_h) {
//             pb.push_rect(rect);
//         }
//         fill_path(pixmap, tx, use_paint, pb);
//     }
// }
//
// /// 小方块网格（5Boxes）：2x2 网格，level 个点亮（深色），其余浅灰。
// fn draw_boxes(pixmap: &mut PixmapMut, tx: Transform, paint: &Paint, cx: f32, cy: f32, half: f32, level: u8) {
//     let box_size = half * 0.8;
//     let gap = half * 0.15;
//     let total_w = box_size * 2.0 + gap;
//     let start_x = cx - total_w / 2.0;
//     let start_y = cy - total_w / 2.0;
//
//     let mut light_paint = Paint::default();
//     light_paint.anti_alias = true;
//     light_paint.set_color_rgba8(0xd8, 0xd8, 0xe0, 0xff);
//
//     for i in 0..4u8 {
//         let row = i / 2;
//         let col = i % 2;
//         let x = start_x + col as f32 * (box_size + gap);
//         let y = start_y + row as f32 * (box_size + gap);
//         let use_paint = if i < level { paint } else { &light_paint };
//         let mut pb = PathBuilder::new();
//         if let Some(rect) = Rect::from_xywh(x, y, box_size, box_size) {
//             pb.push_rect(rect);
//         }
//         fill_path(pixmap, tx, use_paint, pb);
//     }
// }
//
// /// 从 CellValue 提取数值，非数值返回 None（IconSet 只对数值生效）
}
// ============ 旧版手绘实现结束 ============

/// 十六进制/命名色字符串 → tiny_skia Paint 的 RGBA。
/// 数据源只出现了 "#RRGGBB" 和 "white" 两种形式，故只需处理这两种。
fn parse_icon_fill(fill: &str) -> (u8, u8, u8) {
    if fill.eq_ignore_ascii_case("white") {
        return (0xff, 0xff, 0xff);
    }
    let s = fill.trim_start_matches('#');
    if s.len() == 6 {
        if let (Ok(r), Ok(g), Ok(b)) = (
            u8::from_str_radix(&s[0..2], 16),
            u8::from_str_radix(&s[2..4], 16),
            u8::from_str_radix(&s[4..6], 16),
        ) {
            return (r, g, b);
        }
    }
    (0, 0, 0) // 兜底黑色，理论上不会命中（数据源固定，已核对）
}

/// 把一条 SVG path `d` 属性字符串（16x16 viewBox 坐标系）解析后，按
/// (cx, cy, half) 仿射变换（viewBox 0..16 缩放平移到目标图标区域），
/// 写入 tiny_skia PathBuilder。
///
/// 仿射规则：viewBox 中心 (8,8) 映射到目标中心 (cx,cy)，viewBox 半宽
/// (8) 映射到 half，即 `screen = center + (viewbox - 8) * (half / 8)`。
///
/// 弧线命令（A）特判：本项目所有源数据里的 A 命令都来自"整圆用两段
/// 半圆弧拼接"这一种规整情形（见 oo_icon_paths.rs 生成过程），不是
/// 任意椭圆弧，因此不需要通用椭圆弧→贝塞尔转换数学：直接把每段弧用
/// 一批采样点（沿圆周circle_steps等分）连成折线近似，弧本身半径*2=
/// 整圆直径，视觉上16px小图标此近似与真圆无可辨差异。
fn append_svg_path_transformed(pb: &mut PathBuilder, d: &str, cx: f32, cy: f32, half: f32) {
    let scale = half / 8.0;
    let map = |x: f32, y: f32| -> (f32, f32) { (cx + (x - 8.0) * scale, cy + (y - 8.0) * scale) };

    let mut cur = (0.0f32, 0.0f32);
    let mut start = (0.0f32, 0.0f32);
    let mut has_current = false;

    for seg in svgtypes::PathParser::from(d) {
        let seg = match seg {
            Ok(s) => s,
            Err(_) => continue, // 数据源固定且已核对，理论不会命中；容错跳过避免panic
        };
        use svgtypes::PathSegment::*;
        match seg {
            MoveTo { abs, x, y } => {
                let (x, y) = if abs { (x as f32, y as f32) } else { (cur.0 + x as f32, cur.1 + y as f32) };
                let (sx, sy) = map(x, y);
                pb.move_to(sx, sy);
                cur = (x, y);
                start = cur;
                has_current = true;
            }
            LineTo { abs, x, y } => {
                let (x, y) = if abs { (x as f32, y as f32) } else { (cur.0 + x as f32, cur.1 + y as f32) };
                let (sx, sy) = map(x, y);
                pb.line_to(sx, sy);
                cur = (x, y);
            }
            HorizontalLineTo { abs, x } => {
                let x = if abs { x as f32 } else { cur.0 + x as f32 };
                let (sx, sy) = map(x, cur.1);
                pb.line_to(sx, sy);
                cur = (x, cur.1);
            }
            VerticalLineTo { abs, y } => {
                let y = if abs { y as f32 } else { cur.1 + y as f32 };
                let (sx, sy) = map(cur.0, y);
                pb.line_to(sx, sy);
                cur = (cur.0, y);
            }
            CurveTo { abs, x1, y1, x2, y2, x, y } => {
                let (x1, y1, x2, y2, x, y) = if abs {
                    (x1 as f32, y1 as f32, x2 as f32, y2 as f32, x as f32, y as f32)
                } else {
                    (
                        cur.0 + x1 as f32, cur.1 + y1 as f32,
                        cur.0 + x2 as f32, cur.1 + y2 as f32,
                        cur.0 + x as f32, cur.1 + y as f32,
                    )
                };
                let (s1x, s1y) = map(x1, y1);
                let (s2x, s2y) = map(x2, y2);
                let (sx, sy) = map(x, y);
                pb.cubic_to(s1x, s1y, s2x, s2y, sx, sy);
                cur = (x, y);
            }
            Quadratic { abs, x1, y1, x, y } => {
                let (x1, y1, x, y) = if abs {
                    (x1 as f32, y1 as f32, x as f32, y as f32)
                } else {
                    (cur.0 + x1 as f32, cur.1 + y1 as f32, cur.0 + x as f32, cur.1 + y as f32)
                };
                let (s1x, s1y) = map(x1, y1);
                let (sx, sy) = map(x, y);
                pb.quad_to(s1x, s1y, sx, sy);
                cur = (x, y);
            }
            EllipticalArc { abs, rx, ry, x, y, .. } => {
                // 特判：本项目数据里的弧均为"整圆两段半圆弧"，用采样折线近似
                let (ex, ey) = if abs { (x as f32, y as f32) } else { (cur.0 + x as f32, cur.1 + y as f32) };
                let r = ((rx as f32) + (ry as f32)) / 2.0; // rx应==ry（正圆），容错取均值
                // 圆心：cur和ex是直径两端点（半圆弧场景），圆心为中点
                let mid = ((cur.0 + ex) / 2.0, (cur.1 + ey) / 2.0);
                // 以cur为起角，沿圆周走到ex，采样16个点画折线（半圆足够平滑）
                let start_angle = (cur.1 - mid.1).atan2(cur.0 - mid.0);
                let end_angle = (ey - mid.1).atan2(ex - mid.0);
                let mut delta = end_angle - start_angle;
                // 统一走"较长"的弧方向（半圆弧，绝对值应接近PI）；若算出的短弧不对，翻转方向
                if delta.abs() < std::f32::consts::PI * 0.5 {
                    delta = if delta >= 0.0 { delta + std::f32::consts::PI * 2.0 } else { delta - std::f32::consts::PI * 2.0 };
                }
                let steps = 16;
                for i in 1..=steps {
                    let t = start_angle + delta * (i as f32 / steps as f32);
                    let px = mid.0 + r * t.cos();
                    let py = mid.1 + r * t.sin();
                    let (sx, sy) = map(px, py);
                    pb.line_to(sx, sy);
                }
                cur = (ex, ey);
            }
            ClosePath { .. } => {
                pb.close();
                cur = start;
            }
            // S/s (SmoothCurveTo)、T/t (SmoothQuadratic)：本项目50个图标
            // 数据源（已核对）未使用这两种命令，仅为满足match穷尽性而
            // 保留分支，实际不会被触发。若未来更新图标数据源用到这两种
            // 命令，需要在此补充"镜像上一控制点"的正确实现。
            SmoothCurveTo { .. } | SmoothQuadratic { .. } => {}
        }
    }
    let _ = has_current;
}

/// 按 OnlyOffice 图标ID（1~50，见 oo_icon_paths.rs）在 (cx, cy) 为中心、
/// 边长 2*half 的区域内绘制矢量图标。多层图形（如交通灯的外框/内圈/圆心）
/// 按数据顺序依次填充，后画的覆盖先画的。
pub fn draw_icon_by_id(pixmap: &mut PixmapMut, tx: Transform, icon_id: u8, cx: f32, cy: f32, half: f32) {
    let layers: &[IconPath] = icon_by_id(icon_id);
    for (d, fill, stroke) in layers {
        let mut pb = PathBuilder::new();
        append_svg_path_transformed(&mut pb, d, cx, cy, half);
        let path = match pb.finish() {
            Some(p) => p,
            None => continue,
        };

        if let Some(fill_color) = fill {
            let (r, g, b) = parse_icon_fill(fill_color);
            let mut paint = Paint::default();
            paint.anti_alias = true;
            paint.set_color_rgba8(r, g, b, 0xff);
            pixmap.fill_path(&path, &paint, tiny_skia::FillRule::Winding, tx, None);
        }

        // ROOT-074: 描边层。"separate"模式（fill=None的独立层）靠这里
        // 独立画出可见轮廓；"inline"模式（fill+stroke同层，如iStarSilver）
        // 靠这里在填充之后叠加勾边。stroke宽度固定用viewBox坐标系下的
        // 1个单位（与原始SVG未显式指定stroke-width时的默认值一致），
        // 按当前图标的half/8缩放比例换算成实际像素宽度。
        if let Some((stroke_color, opacity)) = stroke {
            let (r, g, b) = parse_icon_fill(stroke_color);
            let alpha = (opacity.clamp(0.0, 1.0) * 255.0).round() as u8;
            let mut stroke_paint = Paint::default();
            stroke_paint.anti_alias = true;
            stroke_paint.set_color_rgba8(r, g, b, alpha);

            let stroke_width = (half / 8.0).max(0.1); // viewBox 1单位 -> 实际像素
            let stroke_cfg = tiny_skia::Stroke {
                width: stroke_width,
                ..Default::default()
            };
            pixmap.stroke_path(&path, &stroke_paint, &stroke_cfg, tx, None);
        }
    }
}

fn cell_as_f64(val: &CellValue) -> Option<f64> {
    match val {
        CellValue::Number(n) => Some(*n),
        CellValue::Formula { cached, .. } => match cached.as_ref() {
            CellValue::Number(n) => Some(*n),
            _ => None,
        },
        _ => None,
    }
}

/// 复用 cond_format_render.rs 的 resolve_cfvo 逻辑（同一套阈值语义），
/// 在这里独立实现一份（避免跨文件 pub 导出私有辅助函数增加耦合）。
fn resolve_cfvo(cfvo: &CfValue, sorted_vals: &[f64]) -> f64 {
    if sorted_vals.is_empty() {
        return cfvo.val.parse::<f64>().unwrap_or(0.0);
    }
    let data_min = sorted_vals[0];
    let data_max = sorted_vals[sorted_vals.len() - 1];
    match cfvo.kind.as_str() {
        "min" => data_min,
        "max" => data_max,
        "num" => cfvo.val.parse::<f64>().unwrap_or(data_min),
        "percent" => {
            let p = cfvo.val.parse::<f64>().unwrap_or(0.0).clamp(0.0, 100.0) / 100.0;
            data_min + (data_max - data_min) * p
        }
        "percentile" => {
            let p = cfvo.val.parse::<f64>().unwrap_or(0.0).clamp(0.0, 100.0) / 100.0;
            let idx = ((sorted_vals.len() as f64 - 1.0) * p).round() as usize;
            sorted_vals[idx.min(sorted_vals.len() - 1)]
        }
        "formula" => cfvo.val.parse::<f64>().unwrap_or(data_min),
        _ => data_min,
    }
}

/// 根据阈值列表判断数值 n 落在第几档（0-indexed）。
/// thresholds 是"低到高"排序的阈值点（cfvo 语义：第 i 个阈值是"进入第 i+1 档
/// 所需的最小值"，第 0 个阈值通常是 min，不参与真正的比较判断——按 Excel 语义，
/// N 个 cfvo 产生 N 档：档位 i (i>=1) 的判断条件是 n >= threshold[i]）。
fn resolve_icon_index(n: f64, threshold_values: &[f64], num_icons: usize) -> usize {
    // threshold_values 长度应等于 num_icons（每档一个阈值，第 0 档阈值恒不参与
    // 比较，因为它就是最低档兜底）。从最高档往下找第一个 n >= threshold 的档位。
    let mut idx = 0;
    for i in 1..num_icons.min(threshold_values.len()) {
        if n >= threshold_values[i] {
            idx = i;
        }
    }
    idx
}

/// 收集 range 内所有数值（用于 percent/percentile 阈值计算），逻辑与
/// cond_format_render.rs 的 collect_range_values 一致，这里独立实现一份。
fn collect_range_values(sheet: &Sheet, r1: u32, c1: u32, r2: u32, c2: u32) -> Vec<f64> {
    let mut vals = Vec::new();
    for row in r1..=r2 {
        for col in c1..=c2 {
            if let Some(n) = cell_as_f64(&sheet.cells.get(row, col)) {
                vals.push(n);
            }
        }
    }
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap());
    vals
}

/// 主渲染入口：对一个 IconSet 规则、一个 tile 可见范围，画出所有落在
/// range 内且与 tile 相交的图标。
///
/// 参数含义与 render_databar_and_colorscale 保持一致，供
/// cond_format_render.rs 里的 match 分支直接调用。
/// ROOT-075: 把x14 <x14:cfIcon iconSet="X" iconId="N"/> 里的
/// (预设名, 档内序号) 解析成全局图标ID（对应 oo_icon_paths::icon_by_id
/// 的入参）。iconId 是该预设内部从0开始的档位序号，例如
/// iconSet="3Arrows" iconId="2" 表示 preset_icon_ids("3Arrows") 的
/// 最后一档（下箭头位置对应绿色上箭头，具体取决于预设定义）。
/// "NoIcons" 由调用方在此函数之前特判处理（表示不绘制），不会走到这里。
fn resolve_override_icon_id(set_name: &str, id_in_set: u8) -> Option<u8> {
    let ids = preset_icon_ids(set_name);
    ids.get(id_in_set as usize).copied()
}

#[allow(clippy::too_many_arguments)]
pub fn render_icon_set(
    pixmap: &mut PixmapMut,
    sheet: &Sheet,
    cfg: &CalcRenderConfig,
    tx: Transform,
    row_start: u32,
    row_end: u32,
    col_start: u32,
    col_end: u32,
    icon_style: &str,
    thresholds: &[CfValue],
    percent: bool,
    reverse: bool,
    icons_only: bool,
    tier_overrides: &Option<Vec<Option<(String, u8)>>>,
    r1: u32,
    c1: u32,
    r2: u32,
    c2: u32,
) {
    let _ = icons_only; // v105：数字与图标并排显示暂未实现，仅画图标本身，
                         // 后续如需支持 icons_only=false 时同时画数字，
                         // 需要接入 cosmic-text，留待渲染质量提升阶段处理。
    let _ = percent; // thresholds 里的 CfValue.kind 已经自带 percent/percentile
                     // 等类型信息，resolve_cfvo 会按 kind 处理，这个标志位在
                     // xlsx 里主要影响解析时 cfvo 的默认 kind，渲染层不需要
                     // 额外分支。

    let mut icon_ids: Vec<u8> = preset_icon_ids(icon_style).to_vec();
    if reverse {
        icon_ids.reverse();
    }
    let num_icons = icon_ids.len();
    if num_icons == 0 {
        return;
    }

    let sorted_vals = collect_range_values(sheet, r1, c1, r2, c2);
    let threshold_values: Vec<f64> = thresholds
        .iter()
        .map(|cfvo| resolve_cfvo(cfvo, &sorted_vals))
        .collect();

    for row_idx in r1.max(row_start)..=r2.min(row_end.saturating_sub(1)) {
        for col_idx in c1.max(col_start)..=c2.min(col_end.saturating_sub(1)) {
            let n = match cell_as_f64(&sheet.cells.get(row_idx, col_idx)) {
                Some(n) => n,
                None => continue,
            };

            let icon_idx = resolve_icon_index(n, &threshold_values, num_icons);
            let tier = icon_idx.min(num_icons - 1);

            // ROOT-075: OOXML x14 "custom"混合图标集——该档可能被
            // <x14:cfIcon iconSet="X" iconId="N"/> 单独指定了不同来源
            // 的图标，覆盖外层统一icon_style的默认取值。
            let icon_id = match tier_overrides.as_ref().and_then(|v| v.get(tier)).and_then(|o| o.as_ref()) {
                Some((set_name, id)) => {
                    if set_name == "NoIcons" {
                        continue; // 该档明确指定不绘制任何图标
                    }
                    match resolve_override_icon_id(set_name, *id) {
                        Some(resolved) => resolved,
                        None => icon_ids[tier], // override本身查不到，退回默认档位图标，不静默画错
                    }
                }
                None => icon_ids[tier],
            };

            let cell_x = col_x_px(sheet, col_idx, cfg);
            let cell_y = row_y_px(sheet, row_idx, cfg);
            let cell_w = col_width_at_px(sheet, col_idx, cfg);
            let cell_h = row_height_at_px(sheet, row_idx, cfg);

            // 图标画在单元格左侧，大小取行高的 70%（Excel 观感）
            let icon_size = (cell_h * 0.7).min(cell_w * 0.7).max(4.0);
            let cx = cell_x + icon_size * 0.65;
            let cy = cell_y + cell_h / 2.0;

            draw_icon_by_id(pixmap, tx, icon_id, cx, cy, icon_size / 2.0);
        }
    }
}

/// ROOT-024后续修复：给文字渲染调用点使用的辅助函数。
/// 判断 (row_idx, col_idx) 是否落在某条 IconSet 规则范围内，若是，返回
/// 文字绘制应预留的左侧宽度（图标占用的空间，含右边距），否则返回 0.0。
/// 参照 MS365 的处理方式：图标固定占用一段宽度，单元格文字的可用宽度和
/// 起始x坐标据此收缩/右移，对齐方式（左中右）本身不需要改动，因为
/// 调用方只是把 cell_x/avail_w 当作"缩小后的虚拟单元格"来用。
pub fn icon_set_text_reserve_width(
    sheet: &Sheet,
    cfg: &CalcRenderConfig,
    row_idx: u32,
    col_idx: u32,
) -> f32 {
    for rule in &sheet.cond_formats {
        if let roe_model::calc::ConditionalFormatKind::IconSet { .. } = &rule.kind {
            let r1 = rule.range.start.0;
            let c1 = rule.range.start.1;
            let r2 = rule.range.end.0;
            let c2 = rule.range.end.1;
            if row_idx >= r1 && row_idx <= r2 && col_idx >= c1 && col_idx <= c2 {
                let cell_h = row_height_at_px(sheet, row_idx, cfg);
                let cell_w = col_width_at_px(sheet, col_idx, cfg);
                let icon_size = (cell_h * 0.7).min(cell_w * 0.7).max(4.0);
                // icon起始x偏移(0.65*size) + icon本身宽度(size) + 一点间距，
                // 近似值，不需要像素级精确，够用即可
                // 图标右边缘 = 0.65*size + 0.5*size = 1.15*size，再留3px间距；
                // 返回值要与cell_x/avail_w同为pt，所以px除以PT_TO_PX。
                return (icon_size * 1.15 + 3.0) / crate::tile_geometry::PT_TO_PX;
            }
        }
    }
    0.0
}
