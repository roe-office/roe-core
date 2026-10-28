#!/usr/bin/env python3
"""从 OnlyOffice 预设表格样式 XML 提取数值,生成 roe-model/src/excel_table_styles_data.rs。

只提取数值(theme/tint/边框线型/粗体),不拷贝原文件内容或代码。
用法: python3 scripts/gen_excel_table_styles.py [SRC_XML] [OUT_RS]
注意: dxfId 是 1-based,即 dxfs[dxfId-1]。生成文件请勿手改,改本脚本后重跑。
"""
import os, re, sys
import xml.etree.ElementTree as ET

SRC = sys.argv[1] if len(sys.argv) > 1 else os.path.expanduser(
    "~/openlib/onlyoffice/core/OOXML/Binary/Sheets/XlsxDefaults/presetTableStyles.xml")
OUT = sys.argv[2] if len(sys.argv) > 2 else "roe-model/src/excel_table_styles_data.rs"

PARTS = ["wholeTable", "headerRow", "totalRow", "firstColumn", "lastColumn",
         "firstRowStripe", "secondRowStripe", "firstColumnStripe", "secondColumnStripe"]
SIDES = ["top", "bottom", "left", "right", "horizontal", "vertical"]
BSTYLE = {"thin": 1, "medium": 2, "double": 3, "thick": 4}

def local(t): return t.split('}')[-1]

def color(el):
    if el is None: return None
    assert "rgb" not in el.attrib and el.get("theme") is not None, el.attrib
    return (int(el.get("theme")), float(el.get("tint", "0")))

def fmt_tc(c): return "tc(%d, %.6f)" % (c[0], c[1])

def parse_dxf(d):
    font = d.find("font"); fill = d.find("fill"); border = d.find("border")
    bold = False; fcol = None
    if font is not None:
        b = font.find("b")
        if b is not None:
            assert "val" not in b.attrib; bold = True
        fcol = color(font.find("color"))
    fl = None
    if fill is not None:
        pf = fill.find("patternFill")
        assert pf.get("patternType") == "solid"
        fg = color(pf.find("fgColor")); bg = color(pf.find("bgColor"))
        assert fg == bg
        fl = fg
    bds = [None] * 6
    if border is not None:
        for side in border:
            nm = local(side.tag); assert nm in SIDES, nm
            bds[SIDES.index(nm)] = (BSTYLE[side.get("style")], color(side.find("color")))
    return dict(fill=fl, font=fcol, bold=bold, borders=bds)

def emit_part(p):
    if p is None: return "None"
    bds = ", ".join("N" if b is None else "bs(%d, %s)" % (b[0], fmt_tc(b[1])) for b in p["borders"])
    return "Some(PartStyle { fill: %s, font_color: %s, bold: %s, borders: [%s] })" % (
        "None" if p["fill"] is None else "Some(%s)" % fmt_tc(p["fill"]),
        "None" if p["font"] is None else "Some(%s)" % fmt_tc(p["font"]),
        "true" if p["bold"] else "false", bds)

root = ET.parse(SRC).getroot()
styles = [c for c in root if local(c.tag).startswith("TableStyle")]
assert len(styles) == 60, len(styles)

def sort_key(s):
    m = re.match(r"TableStyle(Light|Medium|Dark)(\d+)$", local(s.tag))
    return (["Light", "Medium", "Dark"].index(m.group(1)), int(m.group(2)))
styles.sort(key=sort_key)

total_parts = 0
body = []
for s in styles:
    name = local(s.tag)
    dxfs = [parse_dxf(d) for d in s.find("dxfs")]
    parts = [None] * 9
    for e in s.iter():
        if local(e.tag) != "tableStyleElement": continue
        i = PARTS.index(e.get("type")); k = int(e.get("dxfId"))
        assert 1 <= k <= len(dxfs)
        parts[i] = dxfs[k - 1]; total_parts += 1
    body.append("    TableStyleDef {\n        name: \"%s\",\n        parts: [\n%s\n        ],\n    }," % (
        name, ",\n".join("            " + emit_part(p) for p in parts)))
assert total_parts == 434, total_parts

head = '''// 本文件由 scripts/gen_excel_table_styles.py 生成,请勿手改;改脚本后重跑。
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
'''
with open(OUT, "w", encoding="utf-8") as f:
    f.write(head + "\n".join(body) + "\n];\n")
print("OK: %d 样式, %d 个 part -> %s" % (len(styles), total_parts, OUT))
