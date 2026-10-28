//! Excel 内置表格样式(60 个预设)的数据驱动实现。
//! 数据来自 excel_table_styles_data.rs(由 scripts/gen_excel_table_styles.py 生成);
//! 颜色 = 主题色(Table.theme_palette) + HSL 明度 tint。
//! 渲染层按单元格位置调用 table_cell_style,得到"表格样式提供的"底色/字色/粗体/边框;
//! 单元格自身格式优先,由调用方负责先取自身格式再回退到这里。
//!
//! part 叠加顺序(后者按字段覆盖前者):
//!   wholeTable < 列条纹 < 行条纹 < 首列/末列 < 表头行/汇总行
//! 已知假设: 行/列条纹同时开启时行条纹覆盖列条纹(Excel 实际优先级待样本验证)。
use crate::calc::{Sheet, Table};
use crate::excel_table_styles_data::*;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TableBorder {
    /// 1=thin 2=medium 3=double 4=thick(同 excel_table_styles_data::BORDER_*)
    pub style: u8,
    pub color: [u8; 3],
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TableCellStyle {
    pub bg: Option<[u8; 3]>,
    pub fg: Option<[u8; 3]>,
    pub bold: bool,
    /// 顺序: top, bottom, left, right(该格自己的四条边)
    pub borders: [Option<TableBorder>; 4],
}

/// 单元格在表内的位置信息(与 Table 解耦,便于单测)
#[derive(Debug, Clone, Copy, Default)]
pub struct CellPos {
    pub is_header: bool,
    pub is_total: bool,
    /// 数据行序号(0-based,不含表头/汇总行);表头/汇总行无意义
    pub data_row: u32,
    /// 相对表左边界的列偏移
    pub col_off: u32,
    pub is_top_edge: bool,
    pub is_bottom_edge: bool,
    pub is_left_edge: bool,
    pub is_right_edge: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct StyleOpts {
    pub show_first_column: bool,
    pub show_last_column: bool,
    pub show_row_stripes: bool,
    pub show_column_stripes: bool,
}

/// Office 默认主题(2013+): 0=lt1 1=dk1 2=lt2 3=dk2 4~9=accent1~6
const DEFAULT_PALETTE: [[u8; 3]; 10] = [
    [0xFF, 0xFF, 0xFF], [0x00, 0x00, 0x00], [0xE7, 0xE6, 0xE6], [0x44, 0x54, 0x6A],
    [0x44, 0x72, 0xC4], [0xED, 0x7D, 0x31], [0xA5, 0xA5, 0xA5],
    [0xFF, 0xC0, 0x00], [0x5B, 0x9B, 0xD5], [0x70, 0xAD, 0x47],
];

fn rgb_to_hsl(c: [u8; 3]) -> (f32, f32, f32) {
    let r = c[0] as f32 / 255.0;
    let g = c[1] as f32 / 255.0;
    let b = c[2] as f32 / 255.0;
    let mx = r.max(g).max(b);
    let mn = r.min(g).min(b);
    let l = (mx + mn) / 2.0;
    let d = mx - mn;
    if d < 1e-6 {
        return (0.0, 0.0, l);
    }
    let s = d / (1.0 - (2.0 * l - 1.0).abs());
    let h = if mx == r {
        ((g - b) / d).rem_euclid(6.0)
    } else if mx == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    (h * 60.0, s, l)
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> [u8; 3] {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = (h / 60.0).rem_euclid(6.0);
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r1, g1, b1) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let f = |v: f32| ((v + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    [f(r1), f(g1), f(b1)]
}

/// Excel tint: HSL 明度变换。tint<0 变暗 L*(1+t);tint>0 变亮 L*(1-t)+t。
pub fn apply_tint(rgb: [u8; 3], tint: f32) -> [u8; 3] {
    if tint == 0.0 {
        return rgb;
    }
    let (h, s, l) = rgb_to_hsl(rgb);
    let l2 = if tint < 0.0 { l * (1.0 + tint) } else { l * (1.0 - tint) + tint };
    hsl_to_rgb(h, s, l2.clamp(0.0, 1.0))
}

fn resolve_color(c: ThemeColor, pal: &[[u8; 3]]) -> [u8; 3] {
    let i = c.theme as usize;
    let base = pal.get(i).copied()
        .or_else(|| DEFAULT_PALETTE.get(i).copied())
        .unwrap_or([0, 0, 0]);
    apply_tint(base, c.tint)
}

fn apply_part(out: &mut TableCellStyle, p: &PartStyle, pal: &[[u8; 3]], pos: &CellPos, whole: bool) {
    if let Some(f) = p.fill {
        out.bg = Some(resolve_color(f, pal));
    }
    if let Some(f) = p.font_color {
        out.fg = Some(resolve_color(f, pal));
    }
    if p.bold {
        out.bold = true;
    }
    // out.borders 顺序: top, bottom, left, right
    for side in 0..4usize {
        let src = if whole {
            // wholeTable: 外缘用 top/bottom/left/right,内部用 horizontal/vertical
            match side {
                0 => if pos.is_top_edge { SIDE_TOP } else { SIDE_HORIZONTAL },
                1 => if pos.is_bottom_edge { SIDE_BOTTOM } else { SIDE_HORIZONTAL },
                2 => if pos.is_left_edge { SIDE_LEFT } else { SIDE_VERTICAL },
                _ => if pos.is_right_edge { SIDE_RIGHT } else { SIDE_VERTICAL },
            }
        } else {
            // 其他 part: top/bottom/left/right 直接作用于该格;horizontal/vertical 暂忽略
            match side {
                0 => SIDE_TOP,
                1 => SIDE_BOTTOM,
                2 => SIDE_LEFT,
                _ => SIDE_RIGHT,
            }
        };
        if let Some(b) = p.borders[src] {
            out.borders[side] = Some(TableBorder {
                style: b.style,
                color: resolve_color(b.color, pal),
            });
        }
    }
}

/// 合成单个单元格的样式(纯函数)。
pub fn compose(def: &TableStyleDef, pal: &[[u8; 3]], pos: &CellPos, o: &StyleOpts) -> TableCellStyle {
    let mut out = TableCellStyle::default();
    let part = |i: usize| def.parts[i].as_ref();
    if let Some(p) = part(PART_WHOLE_TABLE) {
        apply_part(&mut out, p, pal, pos, true);
    }
    let is_data = !pos.is_header && !pos.is_total;
    if is_data && o.show_column_stripes {
        let i = if pos.col_off % 2 == 0 { PART_FIRST_COLUMN_STRIPE } else { PART_SECOND_COLUMN_STRIPE };
        if let Some(p) = part(i) {
            apply_part(&mut out, p, pal, pos, false);
        }
    }
    if is_data && o.show_row_stripes {
        let i = if pos.data_row % 2 == 0 { PART_FIRST_ROW_STRIPE } else { PART_SECOND_ROW_STRIPE };
        if let Some(p) = part(i) {
            apply_part(&mut out, p, pal, pos, false);
        }
    }
    if o.show_first_column && pos.is_left_edge {
        if let Some(p) = part(PART_FIRST_COLUMN) {
            apply_part(&mut out, p, pal, pos, false);
        }
    }
    if o.show_last_column && pos.is_right_edge {
        if let Some(p) = part(PART_LAST_COLUMN) {
            apply_part(&mut out, p, pal, pos, false);
        }
    }
    if pos.is_header {
        if let Some(p) = part(PART_HEADER_ROW) {
            apply_part(&mut out, p, pal, pos, false);
        }
    }
    if pos.is_total {
        if let Some(p) = part(PART_TOTAL_ROW) {
            apply_part(&mut out, p, pal, pos, false);
        }
    }
    out
}

/// 某个表格内 (row, col) 的样式;不在该表内或样式名未知返回 None。
pub fn table_style_at(t: &Table, row: u32, col: u32) -> Option<TableCellStyle> {
    let (r1, c1) = t.range.start;
    let (r2, c2) = t.range.end;
    if row < r1 || row > r2 || col < c1 || col > c2 {
        return None;
    }
    let def = find_table_style(t.style_name.as_deref()?)?;
    let pal: &[[u8; 3]] = if t.theme_palette.len() >= 10 { &t.theme_palette } else { &DEFAULT_PALETTE };
    let hdr = if t.has_header { 1 } else { 0 };
    let pos = CellPos {
        is_header: t.has_header && row == r1,
        is_total: t.has_totals && row == r2,
        data_row: row.saturating_sub(r1 + hdr),
        col_off: col - c1,
        is_top_edge: row == r1,
        is_bottom_edge: row == r2,
        is_left_edge: col == c1,
        is_right_edge: col == c2,
    };
    let o = StyleOpts {
        show_first_column: t.show_first_column,
        show_last_column: t.show_last_column,
        show_row_stripes: t.show_row_stripes,
        show_column_stripes: t.show_column_stripes,
    };
    Some(compose(def, pal, &pos, &o))
}

/// 单元格 (row, col) 所在表格给它的样式;不在表格内或样式未支持返回 None。
pub fn table_cell_style(sheet: &Sheet, row: u32, col: u32) -> Option<TableCellStyle> {
    for t in &sheet.tables {
        let (r1, c1) = t.range.start;
        let (r2, c2) = t.range.end;
        if row < r1 || row > r2 || col < c1 || col > c2 {
            continue;
        }
        return table_style_at(t, row, col);
    }
    None
}

/// 样式预览(菜单缩略图用): 在虚拟的 1 表头 + 3 数据行表上取中间列合成得到。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct StylePreview {
    pub header_bg: Option<[u8; 3]>,
    pub header_fg: Option<[u8; 3]>,
    /// 第 1 个数据行底色(firstRowStripe)
    pub stripe_a: Option<[u8; 3]>,
    /// 第 2 个数据行底色(secondRowStripe)
    pub stripe_b: Option<[u8; 3]>,
    /// 代表性线色: 表头上线 > 表头下线 > 末行下线
    pub line: Option<[u8; 3]>,
}

/// pal 不足 10 项时用 Office 默认色板。样式名未知返回 None。
pub fn style_preview(name: &str, pal: &[[u8; 3]]) -> Option<StylePreview> {
    let def = find_table_style(name)?;
    let pal: &[[u8; 3]] = if pal.len() >= 10 { pal } else { &DEFAULT_PALETTE };
    let o = StyleOpts { show_row_stripes: true, ..Default::default() };
    let at = |row: u32, data_row: u32| {
        let pos = CellPos {
            is_header: row == 0,
            data_row,
            col_off: 1,
            is_top_edge: row == 0,
            is_bottom_edge: row == 3,
            ..Default::default()
        };
        compose(def, pal, &pos, &o)
    };
    let header = at(0, 0);
    let d1 = at(1, 0);
    let d2 = at(2, 1);
    let last = at(3, 2);
    let line = header.borders[0]
        .or(header.borders[1])
        .or(last.borders[1])
        .map(|b| b.color);
    Some(StylePreview {
        header_bg: header.bg,
        header_fg: header.fg,
        stripe_a: d1.bg,
        stripe_b: d2.bg,
        line,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: [u8; 3], b: [u8; 3], tol: i32) -> bool {
        (0..3).all(|i| (a[i] as i32 - b[i] as i32).abs() <= tol)
    }

    fn def(name: &str) -> &'static TableStyleDef {
        find_table_style(name).unwrap_or_else(|| panic!("no style {}", name))
    }

    fn opts() -> StyleOpts {
        StyleOpts { show_row_stripes: true, ..Default::default() }
    }

    /// 3 行 x 3 列表(含表头)内的位置: row 0=表头,1,2=数据;col 0..2
    fn pos(row: u32, col: u32) -> CellPos {
        CellPos {
            is_header: row == 0,
            is_total: false,
            data_row: row.saturating_sub(1),
            col_off: col,
            is_top_edge: row == 0,
            is_bottom_edge: row == 2,
            is_left_edge: col == 0,
            is_right_edge: col == 2,
        }
    }

    #[test]
    fn all_60_styles_present() {
        assert_eq!(TABLE_STYLES.len(), 60);
        assert!(find_table_style("TableStyleLight1").is_some());
        assert!(find_table_style("TableStyleMedium28").is_some());
        assert!(find_table_style("TableStyleDark11").is_some());
        assert!(find_table_style("Custom_Styles1").is_none());
    }

    #[test]
    fn hsl_tint_close_to_excel() {
        // Excel 中 accent1(4472C4) tint 0.8 实际为 D9E1F2
        let c = apply_tint([0x44, 0x72, 0xC4], 0.79998168889431442);
        assert!(close(c, [0xD9, 0xE1, 0xF2], 3), "got {:02X?}", c);
        assert_eq!(apply_tint([1, 2, 3], 0.0), [1, 2, 3]);
        // 黑色变亮: 灰
        let g = apply_tint([0, 0, 0], 0.5);
        assert!(close(g, [128, 128, 128], 1), "got {:02X?}", g);
    }

    #[test]
    fn medium2_header_and_stripes() {
        let d = def("TableStyleMedium2");
        let h = compose(d, &DEFAULT_PALETTE, &pos(0, 1), &opts());
        assert_eq!(h.bg, Some([0x44, 0x72, 0xC4]));
        assert_eq!(h.fg, Some([0xFF, 0xFF, 0xFF]));
        assert!(h.bold);
        // 第 1 个数据行(下标 0)着条纹色
        let r0 = compose(d, &DEFAULT_PALETTE, &pos(1, 1), &opts());
        assert!(close(r0.bg.unwrap(), [0xD9, 0xE1, 0xF2], 3));
        assert!(!r0.bold);
        // 第 2 个数据行无 secondRowStripe -> 无底色
        let r1 = compose(d, &DEFAULT_PALETTE, &pos(2, 1), &opts());
        assert_eq!(r1.bg, None);
        // 关闭行条纹
        let off = compose(d, &DEFAULT_PALETTE, &pos(1, 1), &StyleOpts::default());
        assert_eq!(off.bg, None);
    }

    #[test]
    fn medium2_whole_table_borders() {
        let d = def("TableStyleMedium2");
        // 内部格: 四边都是 thin(来自 horizontal/vertical 与外缘不同,中心格 col1,row1)
        let mid = compose(d, &DEFAULT_PALETTE, &pos(1, 1), &opts());
        assert_eq!(mid.borders[0].map(|b| b.style), Some(BORDER_THIN));
        assert_eq!(mid.borders[1].map(|b| b.style), Some(BORDER_THIN));
        // 外缘格左边
        let left = compose(d, &DEFAULT_PALETTE, &pos(1, 0), &opts());
        assert_eq!(left.borders[2].map(|b| b.style), Some(BORDER_THIN));
        // 边框色 = accent1 tint 0.4
        assert!(close(left.borders[2].unwrap().color, apply_tint([0x44, 0x72, 0xC4], 0.39997558519241921), 0));
    }

    #[test]
    fn medium2_total_row_double_top() {
        let d = def("TableStyleMedium2");
        let mut p = pos(2, 1);
        p.is_total = true;
        let t = compose(d, &DEFAULT_PALETTE, &p, &opts());
        assert!(t.bold);
        assert_eq!(t.borders[0].map(|b| b.style), Some(BORDER_DOUBLE));
    }

    #[test]
    fn light8_both_stripes_have_top_border() {
        let d = def("TableStyleLight8");
        let a = compose(d, &DEFAULT_PALETTE, &pos(1, 1), &opts());
        let b = compose(d, &DEFAULT_PALETTE, &pos(2, 1), &opts());
        assert_eq!(a.borders[0].map(|x| x.style), Some(BORDER_THIN));
        assert_eq!(b.borders[0].map(|x| x.style), Some(BORDER_THIN));
        assert_eq!(a.borders[0].unwrap().color, [0, 0, 0]);
        // 表头: 黑底白字
        let h = compose(d, &DEFAULT_PALETTE, &pos(0, 1), &opts());
        assert_eq!(h.bg, Some([0, 0, 0]));
        assert_eq!(h.fg, Some([0xFF, 0xFF, 0xFF]));
    }

    #[test]
    fn dark3_structure() {
        let d = def("TableStyleDark3");
        let h = compose(d, &DEFAULT_PALETTE, &pos(0, 1), &opts());
        assert_eq!(h.bg, Some([0, 0, 0]));
        assert_eq!(h.borders[1].map(|b| b.style), Some(BORDER_MEDIUM));
        // 数据行 0: 条纹 = accent2 tint -0.25(比 wholeTable 底色暗)
        let r0 = compose(d, &DEFAULT_PALETTE, &pos(1, 1), &opts());
        let r1 = compose(d, &DEFAULT_PALETTE, &pos(2, 1), &opts());
        assert_eq!(r1.bg, Some([0xED, 0x7D, 0x31]));
        assert!(r0.bg.unwrap()[0] < 0xED);
        assert_eq!(r1.fg, Some([0xFF, 0xFF, 0xFF]));
    }

    #[test]
    fn first_column_overrides_stripe() {
        let d = def("TableStyleMedium2");
        let mut o = opts();
        o.show_first_column = true;
        let c = compose(d, &DEFAULT_PALETTE, &pos(1, 0), &o);
        assert!(c.bold);
    }
}

#[cfg(test)]
mod preview_tests {
    use super::*;

    fn close(a: [u8; 3], b: [u8; 3], tol: i32) -> bool {
        (0..3).all(|i| (a[i] as i32 - b[i] as i32).abs() <= tol)
    }

    #[test]
    fn unknown_style_is_none() {
        assert!(style_preview("NoSuchStyle", &[]).is_none());
    }

    #[test]
    fn all_builtin_styles_have_preview() {
        for d in TABLE_STYLES {
            assert!(style_preview(d.name, &[]).is_some(), "{}", d.name);
        }
    }

    #[test]
    fn medium2_header_is_accent1_with_white_font() {
        let p = style_preview("TableStyleMedium2", &[]).unwrap();
        assert!(close(p.header_bg.expect("Medium2 表头应有底色"), [0x44, 0x72, 0xC4], 3), "{:?}", p.header_bg);
        assert!(close(p.header_fg.expect("Medium2 表头应有字色"), [0xFF, 0xFF, 0xFF], 3), "{:?}", p.header_fg);
        assert!(p.stripe_a.is_some());
    }

    #[test]
    fn light1_header_has_no_fill() {
        let p = style_preview("TableStyleLight1", &[]).unwrap();
        assert!(p.header_bg.is_none(), "{:?}", p.header_bg);
        assert!(p.line.is_some());
    }

    #[test]
    fn custom_palette_changes_header() {
        let mut pal = DEFAULT_PALETTE.to_vec();
        pal[4] = [0x15, 0x60, 0x82];
        let p = style_preview("TableStyleMedium2", &pal).unwrap();
        assert!(close(p.header_bg.unwrap(), [0x15, 0x60, 0x82], 3));
    }
}
