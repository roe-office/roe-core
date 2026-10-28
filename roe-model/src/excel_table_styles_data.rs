// 本文件由 scripts/gen_excel_table_styles.py 生成,请勿手改;改脚本后重跑。
// 数据: 60 个内置表格样式(Light1~21 / Medium1~28 / Dark1~11)的数值参数。
//
// 约定:
// - ThemeColor.theme 与 Table.theme_palette 同序: 0=lt1 1=dk1 2=lt2 3=dk2 4~9=accent1~6。
// - tint 为 Excel 的 HSL 明度变换系数(正=变亮,负=变暗),0 表示无 tint。
// - parts 下标见 PART_*;None 表示该样式不定义该 part。
// - borders 下标见 SIDE_*;线型见 BORDER_*。
// - 条纹: 第 1 个数据行用 firstRowStripe,第 2 个用 secondRowStripe,依次交替(步长恒为 1)。

#![allow(dead_code)]

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ThemeColor {
    pub theme: u8,
    pub tint: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BorderSide {
    pub style: u8,
    pub color: ThemeColor,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PartStyle {
    pub fill: Option<ThemeColor>,
    pub font_color: Option<ThemeColor>,
    pub bold: bool,
    /// 顺序: top, bottom, left, right, horizontal(内部横线), vertical(内部竖线)
    pub borders: [Option<BorderSide>; 6],
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TableStyleDef {
    pub name: &'static str,
    pub parts: [Option<PartStyle>; 9],
}

pub const PART_WHOLE_TABLE: usize = 0;
pub const PART_HEADER_ROW: usize = 1;
pub const PART_TOTAL_ROW: usize = 2;
pub const PART_FIRST_COLUMN: usize = 3;
pub const PART_LAST_COLUMN: usize = 4;
pub const PART_FIRST_ROW_STRIPE: usize = 5;
pub const PART_SECOND_ROW_STRIPE: usize = 6;
pub const PART_FIRST_COLUMN_STRIPE: usize = 7;
pub const PART_SECOND_COLUMN_STRIPE: usize = 8;

pub const SIDE_TOP: usize = 0;
pub const SIDE_BOTTOM: usize = 1;
pub const SIDE_LEFT: usize = 2;
pub const SIDE_RIGHT: usize = 3;
pub const SIDE_HORIZONTAL: usize = 4;
pub const SIDE_VERTICAL: usize = 5;

pub const BORDER_THIN: u8 = 1;
pub const BORDER_MEDIUM: u8 = 2;
pub const BORDER_DOUBLE: u8 = 3;
pub const BORDER_THICK: u8 = 4;

const fn tc(theme: u8, tint: f32) -> ThemeColor {
    ThemeColor { theme, tint }
}
const fn bs(style: u8, color: ThemeColor) -> Option<BorderSide> {
    Some(BorderSide { style, color })
}
const N: Option<BorderSide> = None;

pub fn find_table_style(name: &str) -> Option<&'static TableStyleDef> {
    TABLE_STYLES.iter().find(|s| s.name == name)
}

pub static TABLE_STYLES: &[TableStyleDef] = &[
    TableStyleDef {
        name: "TableStyleLight1",
        parts: [
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: false, borders: [bs(1, tc(1, 0.000000)), bs(1, tc(1, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, bs(1, tc(1, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [bs(1, tc(1, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(0, -0.149998)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(0, -0.149998)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleLight2",
        parts: [
            Some(PartStyle { fill: None, font_color: Some(tc(4, -0.249977)), bold: false, borders: [bs(1, tc(4, 0.000000)), bs(1, tc(4, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(4, -0.249977)), bold: true, borders: [N, bs(1, tc(4, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(4, -0.249977)), bold: true, borders: [bs(1, tc(4, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(4, -0.249977)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(4, -0.249977)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(4, 0.799982)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(4, 0.799982)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleLight3",
        parts: [
            Some(PartStyle { fill: None, font_color: Some(tc(5, -0.249977)), bold: false, borders: [bs(1, tc(5, 0.000000)), bs(1, tc(5, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(5, -0.249977)), bold: true, borders: [N, bs(1, tc(5, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(5, -0.249977)), bold: true, borders: [bs(1, tc(5, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(5, -0.249977)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(5, -0.249977)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(5, 0.799982)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(5, 0.799982)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleLight4",
        parts: [
            Some(PartStyle { fill: None, font_color: Some(tc(6, -0.249977)), bold: false, borders: [bs(1, tc(6, 0.000000)), bs(1, tc(6, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(6, -0.249977)), bold: true, borders: [N, bs(1, tc(6, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(6, -0.249977)), bold: true, borders: [bs(1, tc(6, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(6, -0.249977)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(6, -0.249977)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(6, 0.799982)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(6, 0.799982)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleLight5",
        parts: [
            Some(PartStyle { fill: None, font_color: Some(tc(7, -0.249977)), bold: false, borders: [bs(1, tc(7, 0.000000)), bs(1, tc(7, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(7, -0.249977)), bold: true, borders: [N, bs(1, tc(7, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(7, -0.249977)), bold: true, borders: [bs(1, tc(7, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(7, -0.249977)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(7, -0.249977)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(7, 0.799982)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(7, 0.799982)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleLight6",
        parts: [
            Some(PartStyle { fill: None, font_color: Some(tc(8, -0.249977)), bold: false, borders: [bs(1, tc(8, 0.000000)), bs(1, tc(8, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(8, -0.249977)), bold: true, borders: [N, bs(1, tc(8, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(8, -0.249977)), bold: true, borders: [bs(1, tc(8, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(8, -0.249977)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(8, -0.249977)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(8, 0.799982)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(8, 0.799982)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleLight7",
        parts: [
            Some(PartStyle { fill: None, font_color: Some(tc(9, -0.249977)), bold: false, borders: [bs(1, tc(9, 0.000000)), bs(1, tc(9, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(9, -0.249977)), bold: true, borders: [N, bs(1, tc(9, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(9, -0.249977)), bold: true, borders: [bs(1, tc(9, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(9, -0.249977)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(9, -0.249977)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(9, 0.799982)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(9, 0.799982)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleLight8",
        parts: [
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: false, borders: [bs(1, tc(1, 0.000000)), bs(1, tc(1, 0.000000)), bs(1, tc(1, 0.000000)), bs(1, tc(1, 0.000000)), N, N] }),
            Some(PartStyle { fill: Some(tc(1, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [bs(3, tc(1, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: None, bold: false, borders: [bs(1, tc(1, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: None, bold: false, borders: [bs(1, tc(1, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: None, bold: false, borders: [N, N, bs(1, tc(1, 0.000000)), N, N, N] }),
            Some(PartStyle { fill: None, font_color: None, bold: false, borders: [N, N, bs(1, tc(1, 0.000000)), N, N, N] })
        ],
    },
    TableStyleDef {
        name: "TableStyleLight9",
        parts: [
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: false, borders: [bs(1, tc(4, 0.000000)), bs(1, tc(4, 0.000000)), bs(1, tc(4, 0.000000)), bs(1, tc(4, 0.000000)), N, N] }),
            Some(PartStyle { fill: Some(tc(4, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [bs(3, tc(4, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: None, bold: false, borders: [bs(1, tc(4, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: None, bold: false, borders: [bs(1, tc(4, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: None, bold: false, borders: [N, N, bs(1, tc(4, 0.000000)), N, N, N] }),
            Some(PartStyle { fill: None, font_color: None, bold: false, borders: [N, N, bs(1, tc(4, 0.000000)), N, N, N] })
        ],
    },
    TableStyleDef {
        name: "TableStyleLight10",
        parts: [
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: false, borders: [bs(1, tc(5, 0.000000)), bs(1, tc(5, 0.000000)), bs(1, tc(5, 0.000000)), bs(1, tc(5, 0.000000)), N, N] }),
            Some(PartStyle { fill: Some(tc(5, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [bs(3, tc(5, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: None, bold: false, borders: [bs(1, tc(5, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: None, bold: false, borders: [bs(1, tc(5, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: None, bold: false, borders: [N, N, bs(1, tc(5, 0.000000)), N, N, N] }),
            Some(PartStyle { fill: None, font_color: None, bold: false, borders: [N, N, bs(1, tc(5, 0.000000)), N, N, N] })
        ],
    },
    TableStyleDef {
        name: "TableStyleLight11",
        parts: [
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: false, borders: [bs(1, tc(6, 0.000000)), bs(1, tc(6, 0.000000)), bs(1, tc(6, 0.000000)), bs(1, tc(6, 0.000000)), N, N] }),
            Some(PartStyle { fill: Some(tc(6, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [bs(3, tc(6, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: None, bold: false, borders: [bs(1, tc(6, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: None, bold: false, borders: [bs(1, tc(6, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: None, bold: false, borders: [N, N, bs(1, tc(6, 0.000000)), N, N, N] }),
            Some(PartStyle { fill: None, font_color: None, bold: false, borders: [N, N, bs(1, tc(6, 0.000000)), N, N, N] })
        ],
    },
    TableStyleDef {
        name: "TableStyleLight12",
        parts: [
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: false, borders: [bs(1, tc(7, 0.000000)), bs(1, tc(7, 0.000000)), bs(1, tc(7, 0.000000)), bs(1, tc(7, 0.000000)), N, N] }),
            Some(PartStyle { fill: Some(tc(7, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [bs(3, tc(7, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: None, bold: false, borders: [bs(1, tc(7, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: None, bold: false, borders: [bs(1, tc(7, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: None, bold: false, borders: [N, N, bs(1, tc(7, 0.000000)), N, N, N] }),
            Some(PartStyle { fill: None, font_color: None, bold: false, borders: [N, N, bs(1, tc(7, 0.000000)), N, N, N] })
        ],
    },
    TableStyleDef {
        name: "TableStyleLight13",
        parts: [
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: false, borders: [bs(1, tc(8, 0.000000)), bs(1, tc(8, 0.000000)), bs(1, tc(8, 0.000000)), bs(1, tc(8, 0.000000)), N, N] }),
            Some(PartStyle { fill: Some(tc(8, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [bs(3, tc(8, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: None, bold: false, borders: [bs(1, tc(8, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: None, bold: false, borders: [bs(1, tc(8, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: None, bold: false, borders: [N, N, bs(1, tc(8, 0.000000)), N, N, N] }),
            Some(PartStyle { fill: None, font_color: None, bold: false, borders: [N, N, bs(1, tc(8, 0.000000)), N, N, N] })
        ],
    },
    TableStyleDef {
        name: "TableStyleLight14",
        parts: [
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: false, borders: [bs(1, tc(9, 0.000000)), bs(1, tc(9, 0.000000)), bs(1, tc(9, 0.000000)), bs(1, tc(9, 0.000000)), N, N] }),
            Some(PartStyle { fill: Some(tc(9, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [bs(3, tc(9, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: None, bold: false, borders: [bs(1, tc(9, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: None, bold: false, borders: [bs(1, tc(9, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: None, bold: false, borders: [N, N, bs(1, tc(9, 0.000000)), N, N, N] }),
            Some(PartStyle { fill: None, font_color: None, bold: false, borders: [N, N, bs(1, tc(9, 0.000000)), N, N, N] })
        ],
    },
    TableStyleDef {
        name: "TableStyleLight15",
        parts: [
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: false, borders: [bs(1, tc(1, 0.000000)), bs(1, tc(1, 0.000000)), bs(1, tc(1, 0.000000)), bs(1, tc(1, 0.000000)), bs(1, tc(1, 0.000000)), bs(1, tc(1, 0.000000))] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, bs(2, tc(1, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [bs(3, tc(1, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(0, -0.149998)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(0, -0.149998)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleLight16",
        parts: [
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: false, borders: [bs(1, tc(4, 0.000000)), bs(1, tc(4, 0.000000)), bs(1, tc(4, 0.000000)), bs(1, tc(4, 0.000000)), bs(1, tc(4, 0.000000)), bs(1, tc(4, 0.000000))] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, bs(2, tc(4, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [bs(3, tc(4, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(4, 0.799982)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(4, 0.799982)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleLight17",
        parts: [
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: false, borders: [bs(1, tc(5, 0.000000)), bs(1, tc(5, 0.000000)), bs(1, tc(5, 0.000000)), bs(1, tc(5, 0.000000)), bs(1, tc(5, 0.000000)), bs(1, tc(5, 0.000000))] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, bs(2, tc(5, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [bs(3, tc(5, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(5, 0.799982)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(5, 0.799982)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleLight18",
        parts: [
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: false, borders: [bs(1, tc(6, 0.000000)), bs(1, tc(6, 0.000000)), bs(1, tc(6, 0.000000)), bs(1, tc(6, 0.000000)), bs(1, tc(6, 0.000000)), bs(1, tc(6, 0.000000))] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, bs(2, tc(6, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [bs(3, tc(6, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(6, 0.799982)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(6, 0.799982)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleLight19",
        parts: [
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: false, borders: [bs(1, tc(7, 0.000000)), bs(1, tc(7, 0.000000)), bs(1, tc(7, 0.000000)), bs(1, tc(7, 0.000000)), bs(1, tc(7, 0.000000)), bs(1, tc(7, 0.000000))] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, bs(2, tc(7, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [bs(3, tc(7, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(7, 0.799982)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(7, 0.799982)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleLight20",
        parts: [
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: false, borders: [bs(1, tc(8, 0.000000)), bs(1, tc(8, 0.000000)), bs(1, tc(8, 0.000000)), bs(1, tc(8, 0.000000)), bs(1, tc(8, 0.000000)), bs(1, tc(8, 0.000000))] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, bs(2, tc(8, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [bs(3, tc(8, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(8, 0.799982)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(8, 0.799982)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleLight21",
        parts: [
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: false, borders: [bs(1, tc(9, 0.000000)), bs(1, tc(9, 0.000000)), bs(1, tc(9, 0.000000)), bs(1, tc(9, 0.000000)), bs(1, tc(9, 0.000000)), bs(1, tc(9, 0.000000))] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, bs(2, tc(9, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [bs(3, tc(9, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(9, 0.799982)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(9, 0.799982)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleMedium1",
        parts: [
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: false, borders: [bs(1, tc(1, 0.000000)), bs(1, tc(1, 0.000000)), bs(1, tc(1, 0.000000)), bs(1, tc(1, 0.000000)), bs(1, tc(1, 0.000000)), N] }),
            Some(PartStyle { fill: Some(tc(1, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [bs(3, tc(1, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(0, -0.149998)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(0, -0.149998)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleMedium2",
        parts: [
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: false, borders: [bs(1, tc(4, 0.399976)), bs(1, tc(4, 0.399976)), bs(1, tc(4, 0.399976)), bs(1, tc(4, 0.399976)), bs(1, tc(4, 0.399976)), N] }),
            Some(PartStyle { fill: Some(tc(4, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [bs(3, tc(4, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(4, 0.799982)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(4, 0.799982)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleMedium3",
        parts: [
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: false, borders: [bs(1, tc(5, 0.399976)), bs(1, tc(5, 0.399976)), bs(1, tc(5, 0.399976)), bs(1, tc(5, 0.399976)), bs(1, tc(5, 0.399976)), N] }),
            Some(PartStyle { fill: Some(tc(5, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [bs(3, tc(5, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(5, 0.799982)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(5, 0.799982)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleMedium4",
        parts: [
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: false, borders: [bs(1, tc(6, 0.399976)), bs(1, tc(6, 0.399976)), bs(1, tc(6, 0.399976)), bs(1, tc(6, 0.399976)), bs(1, tc(6, 0.399976)), N] }),
            Some(PartStyle { fill: Some(tc(6, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [bs(3, tc(6, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(6, 0.799982)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(6, 0.799982)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleMedium5",
        parts: [
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: false, borders: [bs(1, tc(7, 0.399976)), bs(1, tc(7, 0.399976)), bs(1, tc(7, 0.399976)), bs(1, tc(7, 0.399976)), bs(1, tc(7, 0.399976)), N] }),
            Some(PartStyle { fill: Some(tc(7, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [bs(3, tc(7, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(7, 0.799982)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(7, 0.799982)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleMedium6",
        parts: [
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: false, borders: [bs(1, tc(8, 0.399976)), bs(1, tc(8, 0.399976)), bs(1, tc(8, 0.399976)), bs(1, tc(8, 0.399976)), bs(1, tc(8, 0.399976)), N] }),
            Some(PartStyle { fill: Some(tc(8, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [bs(3, tc(8, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(8, 0.799982)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(8, 0.799982)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleMedium7",
        parts: [
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: false, borders: [bs(1, tc(9, 0.399976)), bs(1, tc(9, 0.399976)), bs(1, tc(9, 0.399976)), bs(1, tc(9, 0.399976)), bs(1, tc(9, 0.399976)), N] }),
            Some(PartStyle { fill: Some(tc(9, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [bs(3, tc(9, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(9, 0.799982)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(9, 0.799982)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleMedium8",
        parts: [
            Some(PartStyle { fill: Some(tc(0, -0.149998)), font_color: Some(tc(1, 0.000000)), bold: false, borders: [N, N, N, N, bs(1, tc(0, 0.000000)), bs(1, tc(0, 0.000000))] }),
            Some(PartStyle { fill: Some(tc(1, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, bs(4, tc(0, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(1, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [bs(4, tc(0, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(1, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(1, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(0, -0.349986)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(0, -0.349986)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleMedium9",
        parts: [
            Some(PartStyle { fill: Some(tc(4, 0.799982)), font_color: Some(tc(1, 0.000000)), bold: false, borders: [N, N, N, N, bs(1, tc(0, 0.000000)), bs(1, tc(0, 0.000000))] }),
            Some(PartStyle { fill: Some(tc(4, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, bs(4, tc(0, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(4, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [bs(4, tc(0, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(4, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(4, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(4, 0.599994)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(4, 0.599994)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleMedium10",
        parts: [
            Some(PartStyle { fill: Some(tc(5, 0.799982)), font_color: Some(tc(1, 0.000000)), bold: false, borders: [N, N, N, N, bs(1, tc(0, 0.000000)), bs(1, tc(0, 0.000000))] }),
            Some(PartStyle { fill: Some(tc(5, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, bs(4, tc(0, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(5, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [bs(4, tc(0, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(5, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(5, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(5, 0.599994)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(5, 0.599994)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleMedium11",
        parts: [
            Some(PartStyle { fill: Some(tc(6, 0.799982)), font_color: Some(tc(1, 0.000000)), bold: false, borders: [N, N, N, N, bs(1, tc(0, 0.000000)), bs(1, tc(0, 0.000000))] }),
            Some(PartStyle { fill: Some(tc(6, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, bs(4, tc(0, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(6, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [bs(4, tc(0, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(6, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(6, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(6, 0.599994)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(6, 0.599994)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleMedium12",
        parts: [
            Some(PartStyle { fill: Some(tc(7, 0.799982)), font_color: Some(tc(1, 0.000000)), bold: false, borders: [N, N, N, N, bs(1, tc(0, 0.000000)), bs(1, tc(0, 0.000000))] }),
            Some(PartStyle { fill: Some(tc(7, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, bs(4, tc(0, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(7, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [bs(4, tc(0, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(7, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(7, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(7, 0.599994)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(7, 0.599994)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleMedium13",
        parts: [
            Some(PartStyle { fill: Some(tc(8, 0.799982)), font_color: Some(tc(1, 0.000000)), bold: false, borders: [N, N, N, N, bs(1, tc(0, 0.000000)), bs(1, tc(0, 0.000000))] }),
            Some(PartStyle { fill: Some(tc(8, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, bs(4, tc(0, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(8, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [bs(4, tc(0, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(8, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(8, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(8, 0.599994)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(8, 0.599994)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleMedium14",
        parts: [
            Some(PartStyle { fill: Some(tc(9, 0.799982)), font_color: Some(tc(1, 0.000000)), bold: false, borders: [N, N, N, N, bs(1, tc(0, 0.000000)), bs(1, tc(0, 0.000000))] }),
            Some(PartStyle { fill: Some(tc(9, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, bs(4, tc(0, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(9, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [bs(4, tc(0, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(9, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(9, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(9, 0.599994)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(9, 0.599994)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleMedium15",
        parts: [
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: false, borders: [bs(2, tc(1, 0.000000)), bs(2, tc(1, 0.000000)), bs(1, tc(1, 0.000000)), bs(1, tc(1, 0.000000)), bs(1, tc(1, 0.000000)), bs(1, tc(1, 0.000000))] }),
            Some(PartStyle { fill: Some(tc(1, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, bs(2, tc(1, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: None, bold: false, borders: [bs(3, tc(1, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(1, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(1, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(0, -0.149998)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(0, -0.149998)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleMedium16",
        parts: [
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: false, borders: [bs(2, tc(1, 0.000000)), bs(2, tc(1, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(4, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, bs(2, tc(1, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: None, bold: false, borders: [bs(3, tc(1, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(4, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(4, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(0, -0.149998)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(0, -0.149998)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleMedium17",
        parts: [
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: false, borders: [bs(2, tc(1, 0.000000)), bs(2, tc(1, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(5, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, bs(2, tc(1, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: None, bold: false, borders: [bs(3, tc(1, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(5, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(5, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(0, -0.149998)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(0, -0.149998)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleMedium18",
        parts: [
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: false, borders: [bs(2, tc(1, 0.000000)), bs(2, tc(1, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(6, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, bs(2, tc(1, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: None, bold: false, borders: [bs(3, tc(1, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(6, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(6, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(0, -0.149998)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(0, -0.149998)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleMedium19",
        parts: [
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: false, borders: [bs(2, tc(1, 0.000000)), bs(2, tc(1, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(7, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, bs(2, tc(1, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: None, bold: false, borders: [bs(3, tc(1, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(7, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(7, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(0, -0.149998)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(0, -0.149998)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleMedium20",
        parts: [
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: false, borders: [bs(2, tc(1, 0.000000)), bs(2, tc(1, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(8, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, bs(2, tc(1, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: None, bold: false, borders: [bs(3, tc(1, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(8, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(8, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(0, -0.149998)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(0, -0.149998)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleMedium21",
        parts: [
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: false, borders: [bs(2, tc(1, 0.000000)), bs(2, tc(1, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(9, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, bs(2, tc(1, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: None, bold: false, borders: [bs(3, tc(1, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(9, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(9, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(0, -0.149998)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(0, -0.149998)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleMedium22",
        parts: [
            Some(PartStyle { fill: Some(tc(0, -0.149998)), font_color: Some(tc(1, 0.000000)), bold: false, borders: [bs(1, tc(1, 0.000000)), bs(1, tc(1, 0.000000)), bs(1, tc(1, 0.000000)), bs(1, tc(1, 0.000000)), bs(1, tc(1, 0.000000)), bs(1, tc(1, 0.000000))] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [bs(2, tc(1, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(0, -0.349986)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(0, -0.349986)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleMedium23",
        parts: [
            Some(PartStyle { fill: Some(tc(4, 0.799982)), font_color: Some(tc(1, 0.000000)), bold: false, borders: [bs(1, tc(4, 0.399976)), bs(1, tc(4, 0.399976)), bs(1, tc(4, 0.399976)), bs(1, tc(4, 0.399976)), bs(1, tc(4, 0.399976)), bs(1, tc(4, 0.399976))] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [bs(2, tc(4, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(4, 0.599994)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(4, 0.599994)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleMedium24",
        parts: [
            Some(PartStyle { fill: Some(tc(5, 0.799982)), font_color: Some(tc(1, 0.000000)), bold: false, borders: [bs(1, tc(5, 0.399976)), bs(1, tc(5, 0.399976)), bs(1, tc(5, 0.399976)), bs(1, tc(5, 0.399976)), bs(1, tc(5, 0.399976)), bs(1, tc(5, 0.399976))] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [bs(2, tc(5, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(5, 0.599994)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(5, 0.599994)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleMedium25",
        parts: [
            Some(PartStyle { fill: Some(tc(6, 0.799982)), font_color: Some(tc(1, 0.000000)), bold: false, borders: [bs(1, tc(6, 0.399976)), bs(1, tc(6, 0.399976)), bs(1, tc(6, 0.399976)), bs(1, tc(6, 0.399976)), bs(1, tc(6, 0.399976)), bs(1, tc(6, 0.399976))] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [bs(2, tc(6, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(6, 0.599994)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(6, 0.599994)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleMedium26",
        parts: [
            Some(PartStyle { fill: Some(tc(7, 0.799982)), font_color: Some(tc(1, 0.000000)), bold: false, borders: [bs(1, tc(7, 0.399976)), bs(1, tc(7, 0.399976)), bs(1, tc(7, 0.399976)), bs(1, tc(7, 0.399976)), bs(1, tc(7, 0.399976)), bs(1, tc(7, 0.399976))] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [bs(2, tc(7, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(7, 0.599994)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(7, 0.599994)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleMedium27",
        parts: [
            Some(PartStyle { fill: Some(tc(8, 0.799982)), font_color: Some(tc(1, 0.000000)), bold: false, borders: [bs(1, tc(8, 0.399976)), bs(1, tc(8, 0.399976)), bs(1, tc(8, 0.399976)), bs(1, tc(8, 0.399976)), bs(1, tc(8, 0.399976)), bs(1, tc(8, 0.399976))] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [bs(2, tc(8, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(8, 0.599994)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(8, 0.599994)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleMedium28",
        parts: [
            Some(PartStyle { fill: Some(tc(9, 0.799982)), font_color: Some(tc(1, 0.000000)), bold: false, borders: [bs(1, tc(9, 0.399976)), bs(1, tc(9, 0.399976)), bs(1, tc(9, 0.399976)), bs(1, tc(9, 0.399976)), bs(1, tc(9, 0.399976)), bs(1, tc(9, 0.399976))] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [bs(2, tc(9, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(9, 0.599994)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(9, 0.599994)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleDark1",
        parts: [
            Some(PartStyle { fill: Some(tc(1, 0.449995)), font_color: Some(tc(0, 0.000000)), bold: false, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(1, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, bs(2, tc(0, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(1, 0.149998)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [bs(2, tc(0, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(1, 0.249977)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, bs(2, tc(0, 0.000000)), N, N] }),
            Some(PartStyle { fill: Some(tc(1, 0.249977)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, bs(2, tc(0, 0.000000)), N, N, N] }),
            Some(PartStyle { fill: Some(tc(1, 0.249977)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(1, 0.249977)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleDark2",
        parts: [
            Some(PartStyle { fill: Some(tc(4, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: false, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(1, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, bs(2, tc(0, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(4, -0.499985)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [bs(2, tc(0, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(4, -0.249977)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, bs(2, tc(0, 0.000000)), N, N] }),
            Some(PartStyle { fill: Some(tc(4, -0.249977)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, bs(2, tc(0, 0.000000)), N, N, N] }),
            Some(PartStyle { fill: Some(tc(4, -0.249977)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(4, -0.249977)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleDark3",
        parts: [
            Some(PartStyle { fill: Some(tc(5, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: false, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(1, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, bs(2, tc(0, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(5, -0.499985)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [bs(2, tc(0, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(5, -0.249977)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, bs(2, tc(0, 0.000000)), N, N] }),
            Some(PartStyle { fill: Some(tc(5, -0.249977)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, bs(2, tc(0, 0.000000)), N, N, N] }),
            Some(PartStyle { fill: Some(tc(5, -0.249977)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(5, -0.249977)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleDark4",
        parts: [
            Some(PartStyle { fill: Some(tc(6, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: false, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(1, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, bs(2, tc(0, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(6, -0.499985)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [bs(2, tc(0, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(6, -0.249977)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, bs(2, tc(0, 0.000000)), N, N] }),
            Some(PartStyle { fill: Some(tc(6, -0.249977)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, bs(2, tc(0, 0.000000)), N, N, N] }),
            Some(PartStyle { fill: Some(tc(6, -0.249977)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(6, -0.249977)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleDark5",
        parts: [
            Some(PartStyle { fill: Some(tc(7, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: false, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(1, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, bs(2, tc(0, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(7, -0.499985)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [bs(2, tc(0, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(7, -0.249977)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, bs(2, tc(0, 0.000000)), N, N] }),
            Some(PartStyle { fill: Some(tc(7, -0.249977)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, bs(2, tc(0, 0.000000)), N, N, N] }),
            Some(PartStyle { fill: Some(tc(7, -0.249977)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(7, -0.249977)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleDark6",
        parts: [
            Some(PartStyle { fill: Some(tc(8, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: false, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(1, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, bs(2, tc(0, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(8, -0.499985)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [bs(2, tc(0, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(8, -0.249977)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, bs(2, tc(0, 0.000000)), N, N] }),
            Some(PartStyle { fill: Some(tc(8, -0.249977)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, bs(2, tc(0, 0.000000)), N, N, N] }),
            Some(PartStyle { fill: Some(tc(8, -0.249977)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(8, -0.249977)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleDark7",
        parts: [
            Some(PartStyle { fill: Some(tc(9, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: false, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(1, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, bs(2, tc(0, 0.000000)), N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(9, -0.499985)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [bs(2, tc(0, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(9, -0.249977)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, N, bs(2, tc(0, 0.000000)), N, N] }),
            Some(PartStyle { fill: Some(tc(9, -0.249977)), font_color: Some(tc(0, 0.000000)), bold: true, borders: [N, N, bs(2, tc(0, 0.000000)), N, N, N] }),
            Some(PartStyle { fill: Some(tc(9, -0.249977)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(9, -0.249977)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleDark8",
        parts: [
            Some(PartStyle { fill: Some(tc(0, -0.149998)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(1, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: false, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [bs(3, tc(1, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(0, -0.349986)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(0, -0.349986)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleDark9",
        parts: [
            Some(PartStyle { fill: Some(tc(4, 0.799982)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(5, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: false, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [bs(3, tc(1, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(4, 0.599994)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(4, 0.599994)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleDark10",
        parts: [
            Some(PartStyle { fill: Some(tc(6, 0.799982)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(7, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: false, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [bs(3, tc(1, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(6, 0.599994)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(6, 0.599994)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
    TableStyleDef {
        name: "TableStyleDark11",
        parts: [
            Some(PartStyle { fill: Some(tc(8, 0.799982)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(9, 0.000000)), font_color: Some(tc(0, 0.000000)), bold: false, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [bs(3, tc(1, 0.000000)), N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: None, font_color: Some(tc(1, 0.000000)), bold: true, borders: [N, N, N, N, N, N] }),
            Some(PartStyle { fill: Some(tc(8, 0.599994)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None,
            Some(PartStyle { fill: Some(tc(8, 0.599994)), font_color: None, bold: false, borders: [N, N, N, N, N, N] }),
            None
        ],
    },
];
