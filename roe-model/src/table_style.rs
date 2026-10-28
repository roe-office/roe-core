//! PROOT-031: PowerPoint 内置表格样式（74种GUID）+ 文档自带 <a:tblStyleLst>
//! 自定义样式的统一数据结构。
//!
//! 设计照搬 LibreOffice(LOK) oox/source/drawingml/table/predefined-table-styles.cxx
//! 的思路：74个GUID先映射到12种"家族"（Light-Style-1/2/3、Medium-Style-
//! 1/2/3/4、Dark-Style-1/2、Themed-Style-1/2），每个家族一套规则，规则
//! 描述9个table part（wholeTbl/firstRow/firstCol/lastRow/lastCol/band1H/
//! band1V/band2H/band2V）各自的边框/填充/文字颜色/加粗。
//! 具体74-GUID映射表 + 12家族规则实现在 roe-parser（因为要贴着LOK源码逐条
//! 抄，属于"解析/查表逻辑"而非通用模型），本文件只放数据结构定义。

use serde::{Deserialize, Serialize};
use crate::common::Color;
use std::collections::HashMap;

/// 颜色变换类型，对应 OOXML <a:tint>/<a:shade>/<a:alpha>。
/// val 是 0-100000 的千分比精度百分比（100000 = 100%），与 OOXML 原始
/// XML 属性值同单位，直接照抄 LOK 传入 addTransformation 的数值即可，
/// 不需要额外换算。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum ColorTransform {
    Tint(u32),
    Shade(u32),
    Alpha(u32),
}

/// 主题色引用 + 可选变换链。scheme_token 复用现有 parse_theme_colors 产出
/// 的 key 命名（"tx1"/"lt1"/"dk1"/"lt2"/"dk2"/"accent1".."accent6"/"bg1"等），
/// 不新建颜色枚举，直接查 theme_colors: &HashMap<String, Color>。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColorRef {
    pub scheme_token: String,
    pub transforms: Vec<ColorTransform>,
}

impl ColorRef {
    pub fn new(token: &str) -> Self {
        Self { scheme_token: token.to_string(), transforms: Vec::new() }
    }

    pub fn with_transform(mut self, t: ColorTransform) -> Self {
        self.transforms.push(t);
        self
    }

    /// 解析为最终 RGBA。theme_colors 查不到时兜底黑色不透明（理论上不该
    /// 触发，因为所有 token 都来自 LOK 硬编码、有效的 scheme 名）。
    pub fn resolve(&self, theme_colors: &HashMap<String, Color>) -> Color {
        let base = theme_colors.get(&self.scheme_token)
            .copied()
            .unwrap_or(Color { r: 0, g: 0, b: 0, a: 255 });
        self.transforms.iter().fold(base, |c, t| apply_transform(c, *t))
    }
}

/// OOXML tint/shade 算法参照 ECMA-376 §20.1.2.3.32/.33：
/// tint 向白色插值，shade 向黑色插值，val 是 0-100000 千分比精度。
/// alpha 直接设置 alpha 通道百分比（覆盖式，非叠加式，与 OOXML 语义一致）。
fn apply_transform(c: Color, t: ColorTransform) -> Color {
    match t {
        ColorTransform::Tint(val) => {
            let f = (val as f32 / 100000.0).clamp(0.0, 1.0);
            Color {
                r: (c.r as f32 * f + 255.0 * (1.0 - f)).round() as u8,
                g: (c.g as f32 * f + 255.0 * (1.0 - f)).round() as u8,
                b: (c.b as f32 * f + 255.0 * (1.0 - f)).round() as u8,
                a: c.a,
            }
        }
        ColorTransform::Shade(val) => {
            let f = (val as f32 / 100000.0).clamp(0.0, 1.0);
            Color {
                r: (c.r as f32 * f).round() as u8,
                g: (c.g as f32 * f).round() as u8,
                b: (c.b as f32 * f).round() as u8,
                a: c.a,
            }
        }
        ColorTransform::Alpha(val) => {
            Color { a: ((val as f32 / 100000.0).clamp(0.0, 1.0) * 255.0).round() as u8, ..c }
        }
    }
}

/// 一个 table part（wholeTbl/firstRow/...）的样式规则。
///
/// 边框字段用 Option<ColorRef> 表示"这条边是否被本样式点亮"（对应 LOK
/// 的 setBorderLineType 有没有针对这条边调用），None 表示该样式不管这条
/// 边，渲染层 fallback 逻辑应保留 cell 自身已有边框设置或渲染层原有默认
/// 值，不应把 None 当成"强制无边框"来覆盖。
///
/// 边框线宽固定 1pt（对应 LOK moLineWidth = 12700 EMU），74种内置样式
/// 没有一种覆盖线宽，不做成可配置字段（过度设计）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TableStylePart {
    pub fill: Option<ColorRef>,
    pub border_left: Option<ColorRef>,
    pub border_right: Option<ColorRef>,
    pub border_top: Option<ColorRef>,
    pub border_bottom: Option<ColorRef>,
    pub border_inside_h: Option<ColorRef>,
    pub border_inside_v: Option<ColorRef>,
    pub text_color: Option<ColorRef>,
    pub text_bold: bool,
}

/// 完整表格样式，9个 part 对应 OOXML tblPr 的 firstRow/firstCol/lastRow/
/// lastCol/bandRow/bandCol 开关所激活的区域。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TableStyle {
    pub style_id: String,
    pub whole_tbl: TableStylePart,
    pub first_row: TableStylePart,
    pub first_col: TableStylePart,
    pub last_row: TableStylePart,
    pub last_col: TableStylePart,
    pub band1_h: TableStylePart,
    pub band1_v: TableStylePart,
    pub band2_h: TableStylePart,
    pub band2_v: TableStylePart,
}
