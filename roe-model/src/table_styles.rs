//! PROOT-031: PowerPoint 内置74种表格样式解析。
//!
//! 照搬 LibreOffice(LOK) oox/source/drawingml/table/predefined-table-styles.cxx
//! 的实现思路（该文件已复制进项目仓库供对照：
//! test_samples/curated_progress_pptx/reference/predefined-table-styles.cxx.lok-reference）：
//! 74个GUID先映射到12种"家族"（Light-Style-1/2/3、Medium-Style-1/2/3/4、
//! Dark-Style-1/2、Themed-Style-1/2），每个家族一套规则。
//!
//! 不用脚本自动转换家族规则——每个家族的具体规则是手写业务逻辑，边写边
//! 对照LOK源码核对，比自动转出来再调试可靠（GUID映射表本身是纯数据，
//! 可以脚本转，此处仍手写以保持与LOK源码逐条对照的可追溯性）。

use std::collections::HashMap;
use crate::table_style::{ColorRef, ColorTransform, TableStyle, TableStylePart};

/// GUID → (家族名, accent变体) 映射表。accent变体为空字符串表示"无强调色
/// 的基础版"。数据来源：LOK mStyleIdMap（第37-196行），逐条核对过。
fn style_id_map() -> HashMap<&'static str, (&'static str, &'static str)> {
    let mut m = HashMap::new();

    // Themed-Style-1
    m.insert("{2D5ABB26-0587-4C30-8999-92F81FD0307C}", ("Themed-Style-1", ""));
    m.insert("{3C2FFA5D-87B4-456A-9821-1D502468CF0F}", ("Themed-Style-1", "Accent1"));
    m.insert("{284E427A-3D55-4303-BF80-6455036E1DE7}", ("Themed-Style-1", "Accent2"));
    m.insert("{69C7853C-536D-4A76-A0AE-DD22124D55A5}", ("Themed-Style-1", "Accent3"));
    m.insert("{775DCB02-9BB8-47FD-8907-85C794F793BA}", ("Themed-Style-1", "Accent4"));
    m.insert("{35758FB7-9AC5-4552-8A53-C91805E547FA}", ("Themed-Style-1", "Accent5"));
    m.insert("{08FB837D-C827-4EFA-A057-4D05807E0F7C}", ("Themed-Style-1", "Accent6"));

    // Themed-Style-2
    m.insert("{5940675A-B579-460E-94D1-54222C63F5DA}", ("Themed-Style-2", ""));
    m.insert("{D113A9D2-9D6B-4929-AA2D-F23B5EE8CBE7}", ("Themed-Style-2", "Accent1"));
    m.insert("{18603FDC-E32A-4AB5-989C-0864C3EAD2B8}", ("Themed-Style-2", "Accent2"));
    m.insert("{306799F8-075E-4A3A-A7F6-7FBC6576F1A4}", ("Themed-Style-2", "Accent3"));
    m.insert("{E269D01E-BC32-4049-B463-5C60D7B0CCD2}", ("Themed-Style-2", "Accent4"));
    m.insert("{327F97BB-C833-4FB7-BDE5-3F7075034690}", ("Themed-Style-2", "Accent5"));
    m.insert("{638B1855-1B75-4FBE-930C-398BA8C253C6}", ("Themed-Style-2", "Accent6"));

    // Light-Style-1
    m.insert("{9D7B26C5-4107-4FEC-AEDC-1716B250A1EF}", ("Light-Style-1", ""));
    m.insert("{3B4B98B0-60AC-42C2-AFA5-B58CD77FA1E5}", ("Light-Style-1", "Accent1"));
    m.insert("{0E3FDE45-AF77-4B5C-9715-49D594BDF05E}", ("Light-Style-1", "Accent2"));
    m.insert("{C083E6E3-FA7D-4D7B-A595-EF9225AFEA82}", ("Light-Style-1", "Accent3"));
    m.insert("{D27102A9-8310-4765-A935-A1911B00CA55}", ("Light-Style-1", "Accent4"));
    m.insert("{5FD0F851-EC5A-4D38-B0AD-8093EC10F338}", ("Light-Style-1", "Accent5"));
    m.insert("{68D230F3-CF80-4859-8CE7-A43EE81993B5}", ("Light-Style-1", "Accent6"));

    // Light-Style-2
    m.insert("{7E9639D4-E3E2-4D34-9284-5A2195B3D0D7}", ("Light-Style-2", ""));
    m.insert("{69012ECD-51FC-41F1-AA8D-1B2483CD663E}", ("Light-Style-2", "Accent1"));
    m.insert("{72833802-FEF1-4C79-8D5D-14CF1EAF98D9}", ("Light-Style-2", "Accent2"));
    m.insert("{F2DE63D5-997A-4646-A377-4702673A728D}", ("Light-Style-2", "Accent3"));
    m.insert("{17292A2E-F333-43FB-9621-5CBBE7FDCDCB}", ("Light-Style-2", "Accent4"));
    m.insert("{5A111915-BE36-4E01-A7E5-04B1672EAD32}", ("Light-Style-2", "Accent5"));
    m.insert("{912C8C85-51F0-491E-9774-3900AFEF0FD7}", ("Light-Style-2", "Accent6"));

    // Light-Style-3
    m.insert("{616DA210-FB5B-4158-B5E0-FEB733F419BA}", ("Light-Style-3", ""));
    m.insert("{BC89EF96-8CEA-46FF-86C4-4CE0E7609802}", ("Light-Style-3", "Accent1"));
    m.insert("{5DA37D80-6434-44D0-A028-1B22A696006F}", ("Light-Style-3", "Accent2"));
    m.insert("{8799B23B-EC83-4686-B30A-512413B5E67A}", ("Light-Style-3", "Accent3"));
    m.insert("{ED083AE6-46FA-4A59-8FB0-9F97EB10719F}", ("Light-Style-3", "Accent4"));
    m.insert("{BDBED569-4797-4DF1-A0F4-6AAB3CD982D8}", ("Light-Style-3", "Accent5"));
    m.insert("{E8B1032C-EA38-4F05-BA0D-38AFFFC7BED3}", ("Light-Style-3", "Accent6"));

    // Medium-Style-1
    m.insert("{793D81CF-94F2-401A-BA57-92F5A7B2D0C5}", ("Medium-Style-1", ""));
    m.insert("{B301B821-A1FF-4177-AEE7-76D212191A09}", ("Medium-Style-1", "Accent1"));
    m.insert("{9DCAF9ED-07DC-4A11-8D7F-57B35C25682E}", ("Medium-Style-1", "Accent2"));
    m.insert("{1FECB4D8-DB02-4DC6-A0A2-4F2EBAE1DC90}", ("Medium-Style-1", "Accent3"));
    m.insert("{1E171933-4619-4E11-9A3F-F7608DF75F80}", ("Medium-Style-1", "Accent4"));
    m.insert("{FABFCF23-3B69-468F-B69F-88F6DE6A72F2}", ("Medium-Style-1", "Accent5"));
    m.insert("{10A1B5D5-9B99-4C35-A422-299274C87663}", ("Medium-Style-1", "Accent6"));

    // Medium-Style-2
    m.insert("{073A0DAA-6AF3-43AB-8588-CEC1D06C72B9}", ("Medium-Style-2", ""));
    m.insert("{5C22544A-7EE6-4342-B048-85BDC9FD1C3A}", ("Medium-Style-2", "Accent1"));
    m.insert("{21E4AEA4-8DFA-4A89-87EB-49C32662AFE0}", ("Medium-Style-2", "Accent2"));
    m.insert("{F5AB1C69-6EDB-4FF4-983F-18BD219EF322}", ("Medium-Style-2", "Accent3"));
    m.insert("{00A15C55-8517-42AA-B614-E9B94910E393}", ("Medium-Style-2", "Accent4"));
    m.insert("{7DF18680-E054-41AD-8BC1-D1AEF772440D}", ("Medium-Style-2", "Accent5"));
    m.insert("{93296810-A885-4BE3-A3E7-6D5BEEA58F35}", ("Medium-Style-2", "Accent6"));

    // Medium-Style-3
    m.insert("{8EC20E35-A176-4012-BC5E-935CFFF8708E}", ("Medium-Style-3", ""));
    m.insert("{6E25E649-3F16-4E02-A733-19D2CDBF48F0}", ("Medium-Style-3", "Accent1"));
    m.insert("{85BE263C-DBD7-4A20-BB59-AAB30ACAA65A}", ("Medium-Style-3", "Accent2"));
    m.insert("{EB344D84-9AFB-497E-A393-DC336BA19D2E}", ("Medium-Style-3", "Accent3"));
    m.insert("{EB9631B5-78F2-41C9-869B-9F39066F8104}", ("Medium-Style-3", "Accent4"));
    m.insert("{74C1A8A3-306A-4EB7-A6B1-4F7E0EB9C5D6}", ("Medium-Style-3", "Accent5"));
    m.insert("{2A488322-F2BA-4B5B-9748-0D474271808F}", ("Medium-Style-3", "Accent6"));

    // Medium-Style-4
    m.insert("{D7AC3CCA-C797-4891-BE02-D94E43425B78}", ("Medium-Style-4", ""));
    m.insert("{69CF1AB2-1976-4502-BF36-3FF5EA218861}", ("Medium-Style-4", "Accent1"));
    m.insert("{8A107856-5554-42FB-B03E-39F5DBC370BA}", ("Medium-Style-4", "Accent2"));
    m.insert("{0505E3EF-67EA-436B-97B2-0124C06EBD24}", ("Medium-Style-4", "Accent3"));
    m.insert("{C4B1156A-380E-4F78-BDF5-A606A8083BF9}", ("Medium-Style-4", "Accent4"));
    m.insert("{22838BEF-8BB2-4498-84A7-C5851F593DF1}", ("Medium-Style-4", "Accent5"));
    m.insert("{16D9F66E-5EB9-4882-86FB-DCBF35E3C3E4}", ("Medium-Style-4", "Accent6"));

    // Dark-Style-1
    m.insert("{E8034E78-7F5D-4C2E-B375-FC64B27BC917}", ("Dark-Style-1", ""));
    m.insert("{125E5076-3810-47DD-B79F-674D7AD40C01}", ("Dark-Style-1", "Accent1"));
    m.insert("{37CE84F3-28C3-443E-9E96-99CF82512B78}", ("Dark-Style-1", "Accent2"));
    m.insert("{D03447BB-5D67-496B-8E87-E561075AD55C}", ("Dark-Style-1", "Accent3"));
    m.insert("{E929F9F4-4A8F-4326-A1B4-22849713DDAB}", ("Dark-Style-1", "Accent4"));
    m.insert("{8FD4443E-F989-4FC4-A0C8-D5A2AF1F390B}", ("Dark-Style-1", "Accent5"));
    m.insert("{AF606853-7671-496A-8E4F-DF71F8EC918B}", ("Dark-Style-1", "Accent6"));

    // Dark-Style-2（注意：LOK源码此家族只有 ""/Accent1/Accent3/Accent5
    // 四条，没有Accent2/4/6，逐条核对过，非遗漏）
    m.insert("{5202B0CA-FC54-4496-8BCA-5EF66A818D29}", ("Dark-Style-2", ""));
    m.insert("{0660B408-B3CF-4A94-85FC-2B1E0A45F4A2}", ("Dark-Style-2", "Accent1"));
    m.insert("{91EBBBCC-DAD2-459C-BE2E-F6DE35CF9A28}", ("Dark-Style-2", "Accent3"));
    m.insert("{46F890A9-2807-4EBB-B81D-B2AA78EC7F39}", ("Dark-Style-2", "Accent5"));

    m
}

/// accent变体名 → theme scheme token。空字符串（无强调色变体）不应调用
/// 此函数，调用方应在 accent_name.is_empty() 分支走各家族规则里定义的
/// 默认token（通常是 tx1/dk1，各家族不同）。
fn resolve_accent_token(accent_name: &str) -> &'static str {
    match accent_name {
        "Accent1" => "accent1",
        "Accent2" => "accent2",
        "Accent3" => "accent3",
        "Accent4" => "accent4",
        "Accent5" => "accent5",
        "Accent6" => "accent6",
        _ => "dk1", // LOK resolveToken 兜底值，实际不应触发（见上方注释）
    }
}

/// 按 styleId 查询是否为 PowerPoint 内置样式，返回 (家族名, accent变体名)。
pub fn lookup_style_id(style_id: &str) -> Option<(&'static str, &'static str)> {
    style_id_map().get(style_id).copied()
}

/// 边框默认线宽，对应 LOK moLineWidth = 12700 EMU = 1pt。所有74种内置
/// 样式共用此线宽，无样式覆盖，故不做成TableStylePart的字段（详见
/// roe-model/src/table_style.rs 顶部注释）。当前未在 TableStylePart 里
/// 承载线宽字段，渲染层沿用其现有边框宽度 fallback 逻辑即可。

/// 按 tableStyleId 解析出完整的 TableStyle。styleId 需带花括号，如
/// "{3B4B98B0-60AC-42C2-AFA5-B58CD77FA1E5}"（与XML原始文本一致，
/// parse_table 已保留原始花括号格式，不需要额外处理）。
///
/// 返回 None 表示该 styleId 不是任何一种内置样式（例如是文档自带
/// <a:tblStyleLst> 里的自定义样式ID，或者是无效/未知GUID），调用方应
/// 转而查文档自带的自定义样式表（未来独立任务，见bug_log PROOT-031）。
pub fn resolve_predefined_table_style(style_id: &str) -> Option<TableStyle> {
    let (style_name, accent_name) = lookup_style_id(style_id)?;

    let mut whole_tbl = TableStylePart::default();
    let mut first_row = TableStylePart::default();
    let mut first_col = TableStylePart::default();
    let mut last_row = TableStylePart::default();
    let mut last_col = TableStylePart::default();
    let mut band1_h = TableStylePart::default();
    let mut band1_v = TableStylePart::default();
    let mut band2_h = TableStylePart::default();
    let mut band2_v = TableStylePart::default();

    match style_name {
        "Themed-Style-1" => {
            if !accent_name.is_empty() {
                let accent_val = resolve_accent_token(accent_name);

                whole_tbl.border_left = Some(ColorRef::new(accent_val));
                whole_tbl.border_right = Some(ColorRef::new(accent_val));
                whole_tbl.border_top = Some(ColorRef::new(accent_val));
                whole_tbl.border_bottom = Some(ColorRef::new(accent_val));
                whole_tbl.border_inside_h = Some(ColorRef::new(accent_val));
                whole_tbl.border_inside_v = Some(ColorRef::new(accent_val));

                whole_tbl.text_color = Some(ColorRef::new("dk1"));
                first_row.text_color = Some(ColorRef::new("lt1"));

                // LOK: firstRow四边都点亮为accent_val，唯独bottom边额外覆盖成lt1
                first_row.border_left = Some(ColorRef::new(accent_val));
                first_row.border_right = Some(ColorRef::new(accent_val));
                first_row.border_top = Some(ColorRef::new(accent_val));
                first_row.border_bottom = Some(ColorRef::new("lt1"));
                first_row.fill = Some(ColorRef::new(accent_val));

                last_row.border_left = Some(ColorRef::new(accent_val));
                last_row.border_right = Some(ColorRef::new(accent_val));
                last_row.border_top = Some(ColorRef::new(accent_val));
                last_row.border_bottom = Some(ColorRef::new(accent_val));

                first_col.border_left = Some(ColorRef::new(accent_val));
                first_col.border_right = Some(ColorRef::new(accent_val));
                first_col.border_top = Some(ColorRef::new(accent_val));
                first_col.border_bottom = Some(ColorRef::new(accent_val));
                first_col.border_inside_h = Some(ColorRef::new(accent_val));

                last_col.border_left = Some(ColorRef::new(accent_val));
                last_col.border_right = Some(ColorRef::new(accent_val));
                last_col.border_top = Some(ColorRef::new(accent_val));
                last_col.border_bottom = Some(ColorRef::new(accent_val));
                last_col.border_inside_h = Some(ColorRef::new(accent_val));

                band1_h.fill = Some(ColorRef::new(accent_val));
                band1_v.fill = Some(ColorRef::new(accent_val));
            } else {
                whole_tbl.text_color = Some(ColorRef::new("tx1"));
            }

            band1_h.fill = band1_h.fill.map(|c| c.with_transform(ColorTransform::Alpha(40000)));
            band1_v.fill = band1_v.fill.map(|c| c.with_transform(ColorTransform::Alpha(40000)));
        }

        "Themed-Style-2" => {
            // LOK 546-549行：wholeTbl四边先无条件setSchemeClr(accent_val)，
            // 553-556行再无条件叠加tint 50000%，与accent_name是否为空无关，
            // 此处先用None占位，两分支结束后统一赋值+统一叠加变换。
            let accent_val: &str;

            if !accent_name.is_empty() {
                first_row.border_bottom = Some(ColorRef::new("lt1"));
                last_row.border_top = Some(ColorRef::new("lt1"));
                first_col.border_right = Some(ColorRef::new("lt1"));
                last_col.border_left = Some(ColorRef::new("lt1"));

                whole_tbl.text_color = Some(ColorRef::new("lt1"));
                first_row.text_color = Some(ColorRef::new("lt1"));

                accent_val = resolve_accent_token(accent_name);

                // LOK pTblBgFillProperties 对应表格整体背景（独立于wholeTbl
                // part之外的table级别背景，getBackgroundFillProperties()）。
                // 当前 TableStyle 结构未单独建模这一层（第一阶段简化，见
                // roe-model/src/table_style.rs 文档），暂用 whole_tbl.fill
                // 承载，效果差异仅在wholeTbl自身还有显式fill时才会体现，
                // 本家族wholeTbl part本身在LOK里没有设置fill，故此简化
                // 本次不产生可观测偏差。
                whole_tbl.fill = Some(ColorRef::new(accent_val));

                // LOK 532-533行：仅accent_name非空分支才给band1H/V设置lt1底色，
                // 为空分支完全不设置（LOK该情形下颜色未定义，实际退化为
                // 默认黑色——这是LOK源码本身的边界状态，此处忠实复刻，不额外
                // 补一个"看起来更合理"的颜色，以保持与参照实现行为一致）。
                band1_h.fill = Some(ColorRef::new("lt1"));
                band1_v.fill = Some(ColorRef::new("lt1"));
            } else {
                whole_tbl.border_inside_v = Some(ColorRef::new("tx1"));
                whole_tbl.border_inside_h = Some(ColorRef::new("tx1"));
                accent_val = "tx1";
            }

            whole_tbl.border_left = Some(ColorRef::new(accent_val).with_transform(ColorTransform::Tint(50000)));
            whole_tbl.border_right = Some(ColorRef::new(accent_val).with_transform(ColorTransform::Tint(50000)));
            whole_tbl.border_top = Some(ColorRef::new(accent_val).with_transform(ColorTransform::Tint(50000)));
            whole_tbl.border_bottom = Some(ColorRef::new(accent_val).with_transform(ColorTransform::Tint(50000)));

            // LOK 551-552行：band1H/V的alpha=20000叠加无条件执行，与
            // accent_name是否为空无关；为空分支时band1_h.fill/band1_v.fill
            // 仍是None（上面未赋值），.map()在None上是no-op，天然复刻了
            // LOK"颜色未定义但仍尝试叠加变换"的行为（LOK对未初始化颜色调用
            // addTransformation不会崩溃，只是变换作用在默认黑色上；我们这里
            // 选择保持None不强行给默认黑色，因为渲染层fallback对None的处理
            // 是"不覆盖cell自身设置"，比强行画黑色更安全，两者在"accent_name
            // 为空的Themed-Style-2"这个冷门场景下的视觉差异待下次真实样本
            // 验证，非本次重点）。
            band1_h.fill = band1_h.fill.map(|c| c.with_transform(ColorTransform::Alpha(20000)));
            band1_v.fill = band1_v.fill.map(|c| c.with_transform(ColorTransform::Alpha(20000)));
        }

        "Light-Style-1" => {
            whole_tbl.border_top = Some(ColorRef::new("tx1"));
            whole_tbl.border_bottom = Some(ColorRef::new("tx1"));
            first_row.border_bottom = Some(ColorRef::new("tx1"));
            last_row.border_top = Some(ColorRef::new("tx1"));

            whole_tbl.text_color = Some(ColorRef::new("tx1"));

            let accent_val = if !accent_name.is_empty() {
                resolve_accent_token(accent_name)
            } else {
                "tx1"
            };

            whole_tbl.border_top = Some(ColorRef::new(accent_val));
            whole_tbl.border_bottom = Some(ColorRef::new(accent_val));
            first_row.border_bottom = Some(ColorRef::new(accent_val));
            last_row.border_top = Some(ColorRef::new(accent_val));

            first_row.text_bold = true;
            first_col.text_bold = true;
            last_col.text_bold = true;

            band1_h.fill = Some(ColorRef::new(accent_val).with_transform(ColorTransform::Alpha(20000)));
            band1_v.fill = Some(ColorRef::new(accent_val).with_transform(ColorTransform::Alpha(20000)));
        }

        // LOK 591-631行
        "Light-Style-2" => {
            whole_tbl.border_left = Some(ColorRef::new("tx1"));
            whole_tbl.border_right = Some(ColorRef::new("tx1"));
            whole_tbl.border_top = Some(ColorRef::new("tx1"));
            whole_tbl.border_bottom = Some(ColorRef::new("tx1"));
            last_row.border_top = Some(ColorRef::new("tx1"));
            band1_h.border_top = Some(ColorRef::new("tx1"));
            band1_h.border_bottom = Some(ColorRef::new("tx1"));
            band1_v.border_left = Some(ColorRef::new("tx1"));
            band1_v.border_right = Some(ColorRef::new("tx1"));
            band2_v.border_left = Some(ColorRef::new("tx1"));
            band2_v.border_right = Some(ColorRef::new("tx1"));

            whole_tbl.text_color = Some(ColorRef::new("tx1"));
            first_row.text_color = Some(ColorRef::new("bg1"));

            let accent_val = if !accent_name.is_empty() {
                resolve_accent_token(accent_name)
            } else {
                "tx1"
            };

            whole_tbl.border_left = Some(ColorRef::new(accent_val));
            whole_tbl.border_right = Some(ColorRef::new(accent_val));
            whole_tbl.border_top = Some(ColorRef::new(accent_val));
            whole_tbl.border_bottom = Some(ColorRef::new(accent_val));

            first_row.fill = Some(ColorRef::new(accent_val));
            last_row.border_top = Some(ColorRef::new(accent_val));

            band1_h.border_top = Some(ColorRef::new(accent_val));
            band1_h.border_bottom = Some(ColorRef::new(accent_val));

            band1_v.border_left = Some(ColorRef::new(accent_val));
            band1_v.border_right = Some(ColorRef::new(accent_val));

            band2_v.border_left = Some(ColorRef::new(accent_val));
            band2_v.border_right = Some(ColorRef::new(accent_val));
        }

        // LOK 632-667行
        "Light-Style-3" => {
            whole_tbl.border_left = Some(ColorRef::new("tx1"));
            whole_tbl.border_right = Some(ColorRef::new("tx1"));
            whole_tbl.border_top = Some(ColorRef::new("tx1"));
            whole_tbl.border_bottom = Some(ColorRef::new("tx1"));
            whole_tbl.border_inside_h = Some(ColorRef::new("tx1"));
            whole_tbl.border_inside_v = Some(ColorRef::new("tx1"));
            first_row.border_bottom = Some(ColorRef::new("tx1"));
            last_row.border_top = Some(ColorRef::new("tx1"));

            whole_tbl.text_color = Some(ColorRef::new("tx1"));

            let accent_val = if !accent_name.is_empty() {
                resolve_accent_token(accent_name)
            } else {
                "tx1"
            };

            whole_tbl.border_left = Some(ColorRef::new(accent_val));
            whole_tbl.border_right = Some(ColorRef::new(accent_val));
            whole_tbl.border_top = Some(ColorRef::new(accent_val));
            whole_tbl.border_bottom = Some(ColorRef::new(accent_val));
            whole_tbl.border_inside_h = Some(ColorRef::new(accent_val));
            whole_tbl.border_inside_v = Some(ColorRef::new(accent_val));

            first_row.text_color = Some(ColorRef::new(accent_val));
            first_row.border_bottom = Some(ColorRef::new(accent_val));
            last_row.border_top = Some(ColorRef::new(accent_val));
            band1_h.fill = Some(ColorRef::new(accent_val));
            band1_v.fill = Some(ColorRef::new(accent_val));

            band1_h.fill = band1_h.fill.map(|c| c.with_transform(ColorTransform::Alpha(20000)));
            band1_v.fill = band1_v.fill.map(|c| c.with_transform(ColorTransform::Alpha(20000)));
        }

        // LOK 668-703行
        "Medium-Style-1" => {
            whole_tbl.border_left = Some(ColorRef::new("dk1"));
            whole_tbl.border_right = Some(ColorRef::new("dk1"));
            whole_tbl.border_top = Some(ColorRef::new("dk1"));
            whole_tbl.border_bottom = Some(ColorRef::new("dk1"));
            whole_tbl.border_inside_h = Some(ColorRef::new("dk1"));
            last_row.border_top = Some(ColorRef::new("dk1"));

            whole_tbl.text_color = Some(ColorRef::new("dk1"));
            first_row.text_color = Some(ColorRef::new("lt1"));
            whole_tbl.fill = Some(ColorRef::new("lt1"));
            last_row.fill = Some(ColorRef::new("lt1"));

            let accent_val = if !accent_name.is_empty() {
                resolve_accent_token(accent_name)
            } else {
                "dk1"
            };

            whole_tbl.border_left = Some(ColorRef::new(accent_val));
            whole_tbl.border_right = Some(ColorRef::new(accent_val));
            whole_tbl.border_top = Some(ColorRef::new(accent_val));
            whole_tbl.border_bottom = Some(ColorRef::new(accent_val));
            whole_tbl.border_inside_h = Some(ColorRef::new(accent_val));

            first_row.fill = Some(ColorRef::new(accent_val));
            band1_h.fill = Some(ColorRef::new(accent_val));
            band1_v.fill = Some(ColorRef::new(accent_val));

            last_row.border_top = Some(ColorRef::new(accent_val));

            band1_h.fill = band1_h.fill.map(|c| c.with_transform(ColorTransform::Tint(20000)));
            band1_v.fill = band1_v.fill.map(|c| c.with_transform(ColorTransform::Tint(20000)));
        }

        // LOK 704-748行
        "Medium-Style-2" => {
            first_row.border_bottom = Some(ColorRef::new("lt1"));
            last_row.border_top = Some(ColorRef::new("lt1"));
            whole_tbl.border_left = Some(ColorRef::new("lt1"));
            whole_tbl.border_right = Some(ColorRef::new("lt1"));
            whole_tbl.border_top = Some(ColorRef::new("lt1"));
            whole_tbl.border_bottom = Some(ColorRef::new("lt1"));
            whole_tbl.border_inside_h = Some(ColorRef::new("lt1"));
            whole_tbl.border_inside_v = Some(ColorRef::new("lt1"));

            whole_tbl.text_color = Some(ColorRef::new("dk1"));
            first_row.text_color = Some(ColorRef::new("lt1"));
            last_row.text_color = Some(ColorRef::new("lt1"));
            first_col.text_color = Some(ColorRef::new("lt1"));
            last_col.text_color = Some(ColorRef::new("lt1"));

            let accent_val = if !accent_name.is_empty() {
                resolve_accent_token(accent_name)
            } else {
                "dk1"
            };

            whole_tbl.fill = Some(ColorRef::new(accent_val).with_transform(ColorTransform::Tint(20000)));
            first_row.fill = Some(ColorRef::new(accent_val));
            last_row.fill = Some(ColorRef::new(accent_val));
            first_col.fill = Some(ColorRef::new(accent_val));
            last_col.fill = Some(ColorRef::new(accent_val));
            band1_h.fill = Some(ColorRef::new(accent_val).with_transform(ColorTransform::Tint(40000)));
            band1_v.fill = Some(ColorRef::new(accent_val).with_transform(ColorTransform::Tint(40000)));
        }

        // LOK 749-783行
        "Medium-Style-3" => {
            whole_tbl.border_top = Some(ColorRef::new("dk1"));
            whole_tbl.border_bottom = Some(ColorRef::new("dk1"));
            first_row.border_bottom = Some(ColorRef::new("dk1"));
            last_row.border_top = Some(ColorRef::new("dk1"));

            whole_tbl.text_color = Some(ColorRef::new("dk1"));
            first_col.text_color = Some(ColorRef::new("lt1"));
            last_col.text_color = Some(ColorRef::new("lt1"));
            whole_tbl.fill = Some(ColorRef::new("lt1"));
            last_row.fill = Some(ColorRef::new("lt1"));
            band1_h.fill = Some(ColorRef::new("dk1"));
            band1_v.fill = Some(ColorRef::new("dk1"));

            first_row.text_color = Some(ColorRef::new("lt1"));

            let accent_val = if !accent_name.is_empty() {
                resolve_accent_token(accent_name)
            } else {
                "dk1"
            };

            first_row.fill = Some(ColorRef::new(accent_val));
            first_col.fill = Some(ColorRef::new(accent_val));
            last_col.fill = Some(ColorRef::new(accent_val));

            band1_h.fill = band1_h.fill.map(|c| c.with_transform(ColorTransform::Tint(20000)));
            band1_v.fill = band1_v.fill.map(|c| c.with_transform(ColorTransform::Tint(20000)));
        }

        // LOK 784-822行
        "Medium-Style-4" => {
            whole_tbl.border_left = Some(ColorRef::new("dk1"));
            whole_tbl.border_right = Some(ColorRef::new("dk1"));
            whole_tbl.border_top = Some(ColorRef::new("dk1"));
            whole_tbl.border_bottom = Some(ColorRef::new("dk1"));
            whole_tbl.border_inside_h = Some(ColorRef::new("dk1"));
            whole_tbl.border_inside_v = Some(ColorRef::new("dk1"));

            whole_tbl.text_color = Some(ColorRef::new("dk1"));
            last_row.border_top = Some(ColorRef::new("dk1"));
            last_row.fill = Some(ColorRef::new("dk1"));

            let accent_val = if !accent_name.is_empty() {
                resolve_accent_token(accent_name)
            } else {
                "dk1"
            };

            whole_tbl.border_left = Some(ColorRef::new(accent_val));
            whole_tbl.border_right = Some(ColorRef::new(accent_val));
            whole_tbl.border_top = Some(ColorRef::new(accent_val));
            whole_tbl.border_bottom = Some(ColorRef::new(accent_val));
            whole_tbl.border_inside_h = Some(ColorRef::new(accent_val));
            whole_tbl.border_inside_v = Some(ColorRef::new(accent_val));
            whole_tbl.fill = Some(ColorRef::new(accent_val));

            first_row.text_color = Some(ColorRef::new(accent_val));
            first_row.fill = Some(ColorRef::new(accent_val));
            band1_h.fill = Some(ColorRef::new(accent_val));
            band1_v.fill = Some(ColorRef::new(accent_val));

            first_row.fill = first_row.fill.map(|c| c.with_transform(ColorTransform::Tint(20000)));
            last_row.fill = last_row.fill.map(|c| c.with_transform(ColorTransform::Tint(20000)));
            whole_tbl.fill = whole_tbl.fill.map(|c| c.with_transform(ColorTransform::Tint(20000)));
            band1_h.fill = band1_h.fill.map(|c| c.with_transform(ColorTransform::Tint(40000)));
            band1_v.fill = band1_v.fill.map(|c| c.with_transform(ColorTransform::Tint(40000)));
        }

        // LOK 823-864行
        "Dark-Style-1" => {
            whole_tbl.text_color = Some(ColorRef::new("dk1"));
            first_row.text_color = Some(ColorRef::new("lt1"));
            first_row.border_bottom = Some(ColorRef::new("lt1"));
            first_col.border_right = Some(ColorRef::new("lt1"));
            last_col.border_left = Some(ColorRef::new("lt1"));
            first_row.fill = Some(ColorRef::new("dk1"));
            last_row.border_top = Some(ColorRef::new("lt1"));

            // LOK: accent_name非空用shade变换，空时用tint变换，accent_val本身
            // 也不同（dk1 vs 实际accent色）——两个变量都要跟着分支切换。
            let (accent_val, transform_kind): (&str, fn(u32) -> ColorTransform) =
                if !accent_name.is_empty() {
                    (resolve_accent_token(accent_name), ColorTransform::Shade as fn(u32) -> ColorTransform)
                } else {
                    ("dk1", ColorTransform::Tint as fn(u32) -> ColorTransform)
                };

            whole_tbl.fill = Some(ColorRef::new(accent_val));
            last_row.fill = Some(ColorRef::new(accent_val));
            first_col.fill = Some(ColorRef::new(accent_val));
            last_col.fill = Some(ColorRef::new(accent_val));
            band1_h.fill = Some(ColorRef::new(accent_val));
            band1_v.fill = Some(ColorRef::new(accent_val));

            whole_tbl.fill = whole_tbl.fill.map(|c| c.with_transform(transform_kind(20000)));
            band1_h.fill = band1_h.fill.map(|c| c.with_transform(transform_kind(40000)));
            band1_v.fill = band1_v.fill.map(|c| c.with_transform(transform_kind(40000)));
            last_col.fill = last_col.fill.map(|c| c.with_transform(transform_kind(60000)));
            first_col.fill = first_col.fill.map(|c| c.with_transform(transform_kind(60000)));
        }

        // LOK 865-899行
        "Dark-Style-2" => {
            last_row.border_top = Some(ColorRef::new("dk1"));

            whole_tbl.text_color = Some(ColorRef::new("dk1"));
            first_row.text_color = Some(ColorRef::new("lt1"));

            // LOK 874-881行：firstRow底色特殊规则——accent_name为空时用dk1；
            // 明确列出Accent1/3/5三种时分别错位取accent2/4/6（LOK源码原样
            // 如此，可能是刻意的"隔一个用一个"配色设计，非笔误，逐字核对
            // 过第876-881行确认），Accent2/4/6本身则没有对应分支（落空、
            // firstRowFillProperties 保持未设置状态，同 Themed-Style-2
            // 空分支同理，忠实复刻LOK这一不对称行为）。
            first_row.fill = if accent_name.is_empty() {
                Some(ColorRef::new("dk1"))
            } else if accent_name == "Accent1" {
                Some(ColorRef::new("accent2"))
            } else if accent_name == "Accent3" {
                Some(ColorRef::new("accent4"))
            } else if accent_name == "Accent5" {
                Some(ColorRef::new("accent6"))
            } else {
                None
            };

            let accent_val = if !accent_name.is_empty() {
                resolve_accent_token(accent_name)
            } else {
                "dk1"
            };

            whole_tbl.fill = Some(ColorRef::new(accent_val));
            last_row.fill = Some(ColorRef::new(accent_val));
            band1_h.fill = Some(ColorRef::new(accent_val));
            band1_v.fill = Some(ColorRef::new(accent_val));

            whole_tbl.fill = whole_tbl.fill.map(|c| c.with_transform(ColorTransform::Tint(20000)));
            band1_h.fill = band1_h.fill.map(|c| c.with_transform(ColorTransform::Tint(40000)));
            band1_v.fill = band1_v.fill.map(|c| c.with_transform(ColorTransform::Tint(40000)));
            last_row.fill = last_row.fill.map(|c| c.with_transform(ColorTransform::Tint(20000)));
        }

        _ => return None,
    }

    Some(TableStyle {
        style_id: style_id.to_string(),
        whole_tbl, first_row, first_col, last_row, last_col,
        band1_h, band1_v, band2_h, band2_v,
    })
}
