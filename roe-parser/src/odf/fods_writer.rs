//! FODS (Flat ODF Spreadsheet) 写出器
//! roe_model::calc::Workbook → FODS XML 字符串
//!
//! 设计要点（与 fods.rs parser 对称）：
//! - 空行/空列用 number-rows-repeated / number-columns-repeated 压缩
//! - 不做样式去重：每个有 CellFormat 的单元格生成一个唯一 inline <style:style>
//! - merged_cells → table:number-columns-spanned/number-rows-spanned（主单元格）
//!   + 区域内其余位置输出 <table:covered-table-cell>
//! - cond_formats/data_validations/auto_filter：parser 暂不支持读回，这里仍按
//!   ODF 规范写出对应标签（calcext: 命名空间），保证 LibreOffice 等外部软件可见，
//!   但 ROE 自身 roundtrip 测试不覆盖这部分（parse 端读不回来）

use roe_model::calc::{CellValue, MergedCell, Sheet, Workbook};
use std::collections::HashMap;
use std::fmt::Write as _;

pub struct FodsWriter;

impl FodsWriter {
    pub fn write(workbook: &Workbook) -> String {
        let mut out = String::with_capacity(4096);

        out.push_str(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
        out.push('\n');
        out.push_str(
            r#"<office:document
    xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
    xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
    xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
    xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0"
    xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0"
    xmlns:xlink="http://www.w3.org/1999/xlink"
    xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"
    xmlns:of="urn:oasis:names:tc:opendocument:xmlns:of:1.2"
    xmlns:calcext="urn:org:documentfoundation:names:experimental:calc:xmlns:calcext:1.0"
    office:mimetype="application/vnd.oasis.opendocument.spreadsheet">
"#,
        );

        // ── automatic-styles：每个 sheet 的每个有格式的单元格生成一个 inline style ──
        out.push_str("  <office:automatic-styles>\n");
        let mut style_names: HashMap<(usize, u32, u32), String> = HashMap::new();
        for (sheet_idx, sheet) in workbook.sheets.iter().enumerate() {
            for (&(row, col), fmt) in sheet.cell_formats.iter() {
                let name = format!("ce_s{sheet_idx}_r{row}_c{col}");
                write_cell_style(&mut out, &name, fmt);
                style_names.insert((sheet_idx, row, col), name);
            }
        }
        out.push_str("  </office:automatic-styles>\n");

        out.push_str("  <office:body>\n");
        out.push_str("    <office:spreadsheet>\n");

        // ── ODF dataValidation 写方向：具名规则 + 单元格反向引用 ──
        let (content_validations_xml, cell_validation_names) =
            build_content_validations(workbook);
        out.push_str(&content_validations_xml);

        for (sheet_idx, sheet) in workbook.sheets.iter().enumerate() {
            write_sheet(&mut out, sheet_idx, sheet, &style_names, &cell_validation_names);
        }

        out.push_str("    </office:spreadsheet>\n");
        out.push_str("  </office:body>\n");
        out.push_str("</office:document>\n");

        out
    }
}

// ──────────────────────────────────────────────
// Sheet 写出
// ──────────────────────────────────────────────

fn write_sheet(
    out: &mut String,
    sheet_idx: usize,
    sheet: &Sheet,
    style_names: &HashMap<(usize, u32, u32), String>,
    cell_validation_names: &HashMap<(usize, u32, u32), String>,
) {
    let _ = writeln!(
        out,
        r#"      <table:table table:name="{}">"#,
        xml_escape_attr(&sheet.name)
    );

    // table-column：按 col_widths 输出，连续相同宽度可压缩，先简单逐列输出
    // （列数通常不大，不强求压缩；若 col_widths 为空则跳过）
    // col_widths 现为 pt 语义（与 fods.rs parse_measure 统一，2026-06-18），
    // 写出 ODF cm 属性前需 pt -> cm 换算（÷28.3465）。
    for w in &sheet.col_widths {
        let _ = writeln!(
            out,
            r#"        <table:table-column table:column-width="{:.4}cm"/>"#,
            w / 28.3465
        );
    }

    // 找出涉及的最大行号（cells 中的最大行 + merged_cells 的 r2）
    let max_row = compute_max_row(sheet);
    let max_col = compute_max_col(sheet);

    // 构建 covered（被合并覆盖、非主单元格）位置集合
    let mut covered: HashMap<(u32, u32), &MergedCell> = HashMap::new();
    let mut merge_origin: HashMap<(u32, u32), &MergedCell> = HashMap::new();
    for m in &sheet.merged_cells {
        merge_origin.insert((m.r1, m.c1), m);
        for r in m.r1..=m.r2 {
            for c in m.c1..=m.c2 {
                if (r, c) != (m.r1, m.c1) {
                    covered.insert((r, c), m);
                }
            }
        }
    }

    let mut row = 0u32;
    while row <= max_row {
        // 判断该行是否整行为空（无 cell 值、无格式、不在 merge 范围内、不是 auto_filter/frozen 边界）
        if row_is_empty(sheet_idx, sheet, row, max_col, &covered, &merge_origin, cell_validation_names) {
            let mut repeat = 1u32;
            let mut next = row + 1;
            while next <= max_row
                && row_is_empty(sheet_idx, sheet, next, max_col, &covered, &merge_origin, cell_validation_names)
            {
                repeat += 1;
                next += 1;
            }
            if repeat > 1 {
                let _ = writeln!(
                    out,
                    r#"        <table:table-row table:number-rows-repeated="{}"/>"#,
                    repeat
                );
            } else {
                out.push_str("        <table:table-row/>\n");
            }
            row = next;
            continue;
        }

        write_row(out, sheet_idx, sheet, row, max_col, style_names, &covered, &merge_origin, cell_validation_names);
        row += 1;
    }

    write_extras(out, sheet);

    out.push_str("      </table:table>\n");
}

fn write_row(
    out: &mut String,
    sheet_idx: usize,
    sheet: &Sheet,
    row: u32,
    max_col: u32,
    style_names: &HashMap<(usize, u32, u32), String>,
    covered: &HashMap<(u32, u32), &MergedCell>,
    merge_origin: &HashMap<(u32, u32), &MergedCell>,
    cell_validation_names: &HashMap<(usize, u32, u32), String>,
) {
    // row_heights 同上，pt -> cm 换算后写出。
    let height_attr = sheet
        .row_heights
        .get(row as usize)
        .map(|rh| format!(r#" table:row-height="{:.4}cm""#, rh.current() / 28.3465))
        .unwrap_or_default();
    let _ = writeln!(out, r#"        <table:table-row{}>"#, height_attr);

    let mut col = 0u32;
    while col <= max_col {
        if let Some(m) = covered.get(&(row, col)) {
            // 被合并覆盖的非主单元格：用 covered-table-cell，跳到该合并区域结束列
            out.push_str("          <table:covered-table-cell/>\n");
            // 如果当前行是合并区域内某行，且这一行内连续多列都被同一 merge 覆盖，
            // 直接跳到 m.c2 之后，避免逐列重复判断（合并区域不跨越未覆盖列，安全跳转）
            col = m.c2.max(col) + 1;
            let _ = m; // 占位，避免 unused 警告（已在上面用到）
            continue;
        }

        let value = get_cell_value(sheet, row, col);
        let is_empty_cell = matches!(value, CellValue::Empty) && sheet.cell_formats.get(&(row, col)).is_none()
            && merge_origin.get(&(row, col)).is_none()
            && cell_validation_names.get(&(sheet_idx, row, col)).is_none();

        if is_empty_cell {
            // 连续空单元格压缩
            let mut repeat = 1u32;
            let mut next = col + 1;
            while next <= max_col
                && covered.get(&(row, next)).is_none()
                && matches!(get_cell_value(sheet, row, next), CellValue::Empty)
                && sheet.cell_formats.get(&(row, next)).is_none()
                && merge_origin.get(&(row, next)).is_none()
                && cell_validation_names.get(&(sheet_idx, row, next)).is_none()
            {
                repeat += 1;
                next += 1;
            }
            if repeat > 1 {
                let _ = writeln!(
                    out,
                    r#"          <table:table-cell table:number-columns-repeated="{}"/>"#,
                    repeat
                );
            } else {
                out.push_str("          <table:table-cell/>\n");
            }
            col = next;
            continue;
        }

        write_cell(out, sheet_idx, sheet, row, col, &value, style_names, merge_origin, cell_validation_names);
        col += 1;
    }

    out.push_str("        </table:table-row>\n");
}

fn write_cell(
    out: &mut String,
    sheet_idx: usize,
    _sheet: &Sheet,
    row: u32,
    col: u32,
    value: &CellValue,
    style_names: &HashMap<(usize, u32, u32), String>,
    merge_origin: &HashMap<(u32, u32), &MergedCell>,
    cell_validation_names: &HashMap<(usize, u32, u32), String>,
) {
    let style_attr = style_names
        .get(&(sheet_idx, row, col))
        .map(|n| format!(r#" table:style-name="{}""#, n))
        .unwrap_or_default();

    let span_attr = merge_origin
        .get(&(row, col))
        .map(|m| {
            format!(
                r#" table:number-columns-spanned="{}" table:number-rows-spanned="{}""#,
                m.c2 - m.c1 + 1,
                m.r2 - m.r1 + 1
            )
        })
        .unwrap_or_default();

    let validation_attr = cell_validation_names
        .get(&(sheet_idx, row, col))
        .map(|n| format!(r#" table:validation-name="{}""#, n))
        .unwrap_or_default();

    match value {
        CellValue::Empty => {
            let _ = writeln!(out, r#"          <table:table-cell{}{}{}/>"#, style_attr, span_attr, validation_attr);
        }
        CellValue::Number(n) => {
            let _ = writeln!(
                out,
                r#"          <table:table-cell{}{}{} office:value-type="float" office:value="{}"><text:p>{}</text:p></table:table-cell>"#,
                style_attr, span_attr, validation_attr, n, n
            );
        }
        CellValue::Text(s) => {
            let _ = writeln!(
                out,
                r#"          <table:table-cell{}{}{} office:value-type="string"><text:p>{}</text:p></table:table-cell>"#,
                style_attr, span_attr, validation_attr, xml_escape_text(s)
            );
        }
        CellValue::Formula { source, .. } => {
            let _ = writeln!(
                out,
                r#"          <table:table-cell{}{}{} table:formula="of:{}"><text:p></text:p></table:table-cell>"#,
                style_attr, span_attr, validation_attr, xml_escape_attr(source)
            );
        }
        CellValue::Bool(b) => {
            let _ = writeln!(
                out,
                r#"          <table:table-cell{}{}{} office:value-type="boolean" office:boolean-value="{}"><text:p>{}</text:p></table:table-cell>"#,
                style_attr, span_attr, validation_attr, b, b
            );
        }
        CellValue::Error(_) => {
            let _ = writeln!(
                out,
                r#"          <table:table-cell{}{}{} office:value-type="error"><text:p>#ERROR</text:p></table:table-cell>"#,
                style_attr, span_attr, validation_attr
            );
        }
        // ROOT-078（已知技术债，非本次范围）：ODS写出层暂不支持run级
        // 富文本格式往返，拼接全部run文本退化为普通字符串单元格写出，
        // 格式信息（如strike）会丢失。完整支持需要写出层能生成ODS的
        // <text:span text:style-name="...">分段结构，这是独立的、更大
        // 的工作量，本次仅确保不会因非穷尽匹配编译失败。
        CellValue::RichText(runs) => {
            let text: String = runs.iter().map(|r| r.text.as_str()).collect();
            let _ = writeln!(
                out,
                r#"          <table:table-cell{}{}{} office:value-type="string"><text:p>{}</text:p></table:table-cell>"#,
                style_attr, span_attr, validation_attr, xml_escape_text(&text)
            );
        }
    }
}

// ──────────────────────────────────────────────
// 单元格样式写出（CellFormat → <style:style>）
// ──────────────────────────────────────────────

fn write_cell_style(out: &mut String, name: &str, fmt: &roe_model::calc::CellFormat) {
    let _ = writeln!(
        out,
        r#"    <style:style style:name="{}" style:family="table-cell">"#,
        name
    );

    let mut text_props = String::new();
    if let Some(b) = fmt.bold {
        let _ = write!(text_props, r#" fo:font-weight="{}""#, if b { "bold" } else { "normal" });
    }
    if let Some(i) = fmt.italic {
        let _ = write!(text_props, r#" fo:font-style="{}""#, if i { "italic" } else { "normal" });
    }
    if let Some(u) = fmt.underline {
        let _ = write!(
            text_props,
            r#" style:text-underline-style="{}""#,
            if u { "solid" } else { "none" }
        );
    }
    if let Some(sz) = fmt.font_size {
        let _ = write!(text_props, r#" fo:font-size="{:.1}pt""#, sz);
    }
    if let Some(ref ff) = fmt.font_family {
        let _ = write!(text_props, r#" style:font-name="{}""#, xml_escape_attr(ff));
    }
    if let Some(ref fg) = fmt.fg_color {
        let _ = write!(text_props, r#" fo:color="{}""#, xml_escape_attr(fg));
    }
    if !text_props.is_empty() {
        let _ = writeln!(out, "      <style:text-properties{}/>", text_props);
    }

    let mut cell_props = String::new();
    if let Some(ref bg) = fmt.bg_color {
        let _ = write!(cell_props, r#" fo:background-color="{}""#, xml_escape_attr(bg));
    }
    if let Some(w) = fmt.wrap_text {
        let _ = write!(cell_props, r#" fo:wrap-option="{}""#, if w { "wrap" } else { "no-wrap" });
    }
    if let Some(ref av) = fmt.align_v {
        let v = match av.as_str() {
            "top" => "top",
            "bottom" => "bottom",
            _ => "middle",
        };
        let _ = write!(cell_props, r#" style:vertical-align="{}""#, v);
    }
    if !cell_props.is_empty() {
        let _ = writeln!(out, "      <style:table-cell-properties{}/>", cell_props);
    }

    if let Some(ref ah) = fmt.align_h {
        let _ = writeln!(
            out,
            r#"      <style:paragraph-properties fo:text-align="{}"/>"#,
            xml_escape_attr(ah)
        );
    }

    if let Some(ref nf) = fmt.num_format {
        // 简化：直接作为 style:data-style-name 不做单独 number:number-style 定义，
        // 改为内联在 cell-properties 之外写一条注释标注原始格式串，保证信息不丢失。
        let _ = writeln!(out, "      <!-- num_format: {} -->", xml_escape_text(nf));
    }

    out.push_str("    </style:style>\n");
}

// ──────────────────────────────────────────────
// cond_formats / data_validations / auto_filter（单向写出，parser 暂不读回）
// ──────────────────────────────────────────────

/// xlsx 风格 list formula1（单引号+逗号分隔，如 `"苹果,香蕉,橙子"`）转换成
/// ODF cell-content-is-in-list() 要求的分号分隔字面量语法。
/// 不是这种整体带引号格式时（比如 range 引用），原样返回，不强行转换。
fn xlsx_list_formula_to_odf(formula1: &str) -> String {
    let trimmed = formula1.trim();
    let inner = match trimmed.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        Some(inner) => inner,
        None => return formula1.to_string(),
    };
    inner
        .split(',')
        .map(|item| format!("\"{}\"", item.trim()))
        .collect::<Vec<_>>()
        .join(";")
}

/// 收集所有 sheet 的 data_validations，生成 (content-validations XML 块,
/// (sheet_idx,row,col)->validation table:name 映射表)。
/// ODF 是"具名规则+单元格反向引用"模型，与 xlsx 的"range 内联"相反。
fn build_content_validations(
    workbook: &Workbook,
) -> (String, HashMap<(usize, u32, u32), String>) {
    let mut cell_validation_names: HashMap<(usize, u32, u32), String> = HashMap::new();
    let mut rules_xml = String::new();
    let mut rule_seq = 0usize;

    for (sheet_idx, sheet) in workbook.sheets.iter().enumerate() {
        for rule in &sheet.data_validations {
            let formula1_for_odf = if rule.kind == "list" {
                xlsx_list_formula_to_odf(&rule.formula1)
            } else {
                rule.formula1.clone()
            };
            let condition = match super::odf_data_validation::build_odf_condition(
                &rule.kind,
                &rule.operator,
                &formula1_for_odf,
                rule.formula2.as_deref(),
            ) {
                Some(c) => c,
                None => continue,
            };

            let name = format!("dv_{}", rule_seq);
            rule_seq += 1;

            let allow_blank_attr = if rule.allow_blank { "true" } else { "false" };

            rules_xml.push_str(&format!(
                r#"      <table:content-validation table:name="{}" table:condition="{}" table:allow-empty-cell="{}">"#,
                xml_escape_attr(&name),
                xml_escape_attr(&condition),
                allow_blank_attr
            ));
            rules_xml.push('\n');

            if rule.show_error {
                let msg = rule.error_msg.as_deref().unwrap_or("");
                rules_xml.push_str(&format!(
                    r#"        <table:error-message table:display="true"><text:p>{}</text:p></table:error-message>"#,
                    xml_escape_text(msg)
                ));
                rules_xml.push('\n');
            }

            rules_xml.push_str("      </table:content-validation>\n");

            for r in rule.range.start.0..=rule.range.end.0 {
                for c in rule.range.start.1..=rule.range.end.1 {
                    cell_validation_names.insert((sheet_idx, r, c), name.clone());
                }
            }
        }
    }

    if rules_xml.is_empty() {
        return (String::new(), cell_validation_names);
    }

    let mut block = String::new();
    block.push_str("      <table:content-validations>\n");
    block.push_str(&rules_xml);
    block.push_str("      </table:content-validations>\n");
    (block, cell_validation_names)
}

fn write_extras(out: &mut String, sheet: &Sheet) {
    if !sheet.cond_formats.is_empty() {
        out.push_str("        <calcext:conditional-formats>\n");
        for rule in &sheet.cond_formats {
            match &rule.kind {
                roe_model::calc::ConditionalFormatKind::CellFormat { condition, .. } => {
                    let _ = writeln!(
                        out,
                        r#"          <calcext:conditional-format calcext:target-range-address="{}"><calcext:condition calcext:apply-style-name="" calcext:value="{}"/></calcext:conditional-format>"#,
                        range_to_str(&rule.range),
                        xml_escape_attr(condition)
                    );
                }
                roe_model::calc::ConditionalFormatKind::ColorScale { min_type, min_value, min_color, mid, max_type, max_value, max_color } => {
                    let _ = writeln!(
                        out,
                        r#"          <calcext:conditional-format calcext:target-range-address="{}">"#,
                        range_to_str(&rule.range)
                    );
                    out.push_str("            <calcext:color-scale>\n");
                    let _ = writeln!(
                        out,
                        r#"              <calcext:color-scale-entry calcext:type="{}" calcext:value="{}" calcext:color="{}"/>"#,
                        cf_type_to_odf(min_type),
                        xml_escape_attr(min_value.as_deref().unwrap_or("")),
                        xml_escape_attr(min_color)
                    );
                    if let Some((mid_type, mid_value, mid_color)) = mid {
                        let _ = writeln!(
                            out,
                            r#"              <calcext:color-scale-entry calcext:type="{}" calcext:value="{}" calcext:color="{}"/>"#,
                            cf_type_to_odf(mid_type),
                            xml_escape_attr(mid_value),
                            xml_escape_attr(mid_color)
                        );
                    }
                    let _ = writeln!(
                        out,
                        r#"              <calcext:color-scale-entry calcext:type="{}" calcext:value="{}" calcext:color="{}"/>"#,
                        cf_type_to_odf(max_type),
                        xml_escape_attr(max_value.as_deref().unwrap_or("")),
                        xml_escape_attr(max_color)
                    );
                    out.push_str("            </calcext:color-scale>\n");
                    out.push_str("          </calcext:conditional-format>\n");
                }
                roe_model::calc::ConditionalFormatKind::DataBar { min, max, bar_color, border_color: _, direction: _, bar_only, solid_fill, negative_fill_color: _, axis_color: _, axis_position: _ } => {
                    let _ = writeln!(
                        out,
                        r#"          <calcext:conditional-format calcext:target-range-address="{}">"#,
                        range_to_str(&rule.range)
                    );
                    let mut databar_attrs = String::new();
                    if *solid_fill {
                        // ODF: mbGradient=false 时才写 gradient="false"；ROE solid_fill=true 对应 LO mbGradient=false
                        let _ = write!(databar_attrs, r#" calcext:gradient="false""#);
                    }
                    if *bar_only {
                        let _ = write!(databar_attrs, r#" calcext:show-value="false""#);
                    }
                    let _ = writeln!(
                        out,
                        r#"            <calcext:data-bar calcext:positive-color="{}"{}>"#,
                        xml_escape_attr(bar_color),
                        databar_attrs
                    );
                    let _ = writeln!(
                        out,
                        r#"              <calcext:formatting-entry calcext:type="{}" calcext:value="{}"/>"#,
                        cf_type_to_odf(&min.kind),
                        xml_escape_attr(&min.val)
                    );
                    let _ = writeln!(
                        out,
                        r#"              <calcext:formatting-entry calcext:type="{}" calcext:value="{}"/>"#,
                        cf_type_to_odf(&max.kind),
                        xml_escape_attr(&max.val)
                    );
                    out.push_str("            </calcext:data-bar>\n");
                    out.push_str("          </calcext:conditional-format>\n");
                }
                roe_model::calc::ConditionalFormatKind::IconSet { icon_style, thresholds, percent: _, reverse: _, icons_only, tier_overrides: _ } => {
                    let _ = writeln!(
                        out,
                        r#"          <calcext:conditional-format calcext:target-range-address="{}">"#,
                        range_to_str(&rule.range)
                    );
                    let show_value_attr = if *icons_only { r#" calcext:show-value="false""# } else { "" };
                    let _ = writeln!(
                        out,
                        r#"            <calcext:icon-set calcext:icon-set-type="{}"{}>"#,
                        xml_escape_attr(icon_style),
                        show_value_attr
                    );
                    for th in thresholds {
                        let mut entry_attrs = String::new();
                        if !th.gte {
                            let _ = write!(entry_attrs, r#" calcext:greater-equal="false""#);
                        }
                        let _ = writeln!(
                            out,
                            r#"              <calcext:formatting-entry calcext:type="{}" calcext:value="{}"{}/>"#,
                            cf_type_to_odf(&th.kind),
                            xml_escape_attr(&th.val),
                            entry_attrs
                        );
                    }
                    out.push_str("            </calcext:icon-set>\n");
                    out.push_str("          </calcext:conditional-format>\n");
                }
            }
        }
        out.push_str("        </calcext:conditional-formats>\n");
    }

    // data_validations 真实导出已移到 build_content_validations()
    // （文档级 table:content-validations + 单元格 table:validation-name 引用），
    // 不再在这里写注释占位。

    if let Some(ref af) = sheet.auto_filter {
        let _ = writeln!(
            out,
            r#"        <table:database-range table:target-range-address="{}.A{}:.AMJ{}" table:display-filter-buttons="true"/>"#,
            xml_escape_attr(&sheet.name),
            af.header_row + 1,
            af.header_row + 1
        );
    }

    write_sparkline_groups(out, sheet);
    write_floating_objects(out, sheet);
}

/// 将 ROE 内存模型里的 CfValue.kind（沿用 xlsx cfvo type 语义："min"/"max"/"num"/
/// "percent"/"percentile"/"formula"）映射为 ODF calcext 扩展使用的字符串
/// （"minimum"/"maximum"/"number"/"percent"/"percentile"/"formula"）。
/// 参考 LibreOffice sc/source/filter/xml/xmlexprt.cxx::getCondFormatEntryType()
/// 的映射关系（未抄代码，只对照字符串常量）。
fn cf_type_to_odf(kind: &str) -> &'static str {
    match kind {
        "min" => "minimum",
        "max" => "maximum",
        "num" => "number",
        "percent" => "percent",
        "percentile" => "percentile",
        "formula" => "formula",
        _ => "number",
    }
}

fn cell_addr_to_str(sheet_name: &str, row: u32, col: u32) -> String {
    format!("{}.{}{}", xml_escape_attr(sheet_name), col_letter(col), row + 1)
}

fn sparkline_type_to_calcext(t: roe_model::calc::SparklineType) -> &'static str {
    use roe_model::calc::SparklineType::*;
    match t {
        Line => "line",
        Column => "column",
        Stacked => "stacked",
    }
}

fn display_empty_to_calcext(d: roe_model::calc::DisplayEmptyCellsAs) -> &'static str {
    use roe_model::calc::DisplayEmptyCellsAs::*;
    match d {
        Span => "span",
        Gap => "gap",
        Zero => "zero",
    }
}

fn axis_type_to_calcext(t: roe_model::calc::SparklineAxisType) -> &'static str {
    use roe_model::calc::SparklineAxisType::*;
    match t {
        Individual => "individual",
        Group => "group",
        Custom => "custom",
    }
}

/// 导出 sheet.sparkline_groups 为 calcext:sparkline-groups 标签。
/// 与 fods.rs::parse_sparkline_groups_from_fods_xml 的读取逻辑严格对称：
/// - line-width 带 "pt" 单位后缀（内存模型 line_weight 已是 pt，无需换算）
/// - cell-address / data-range 带 "SheetName." 前缀，复用现有 range_to_str /
///   新增的 cell_addr_to_str
/// - 颜色已经是 "#RRGGBB"，直接输出，不做转换
/// - 未解析的 *-complex-color 系列子标签本次也不写出，与解析侧刻意忽略的
///   原则一致（见 ROE_Sparkline_Design_v1.md 第2/6节、v115 交接文档）
fn write_sparkline_groups(out: &mut String, sheet: &Sheet) {
    if sheet.sparkline_groups.is_empty() {
        return;
    }
    out.push_str("        <calcext:sparkline-groups>
");
    for group in &sheet.sparkline_groups {
        let _ = writeln!(
            out,
            r#"          <calcext:sparkline-group calcext:type="{}" calcext:line-width="{}pt" calcext:date-axis="{}" calcext:display-empty-cells-as="{}" calcext:markers="{}" calcext:high="{}" calcext:low="{}" calcext:first="{}" calcext:last="{}" calcext:negative="{}" calcext:display-x-axis="{}" calcext:display-hidden="{}" calcext:min-axis-type="{}" calcext:max-axis-type="{}" calcext:right-to-left="{}"{}{}{}{}{}{}{}{}{}{}>"#,
            sparkline_type_to_calcext(group.sparkline_type),
            group.line_weight,
            group.date_axis,
            display_empty_to_calcext(group.display_empty_cells_as),
            group.markers,
            group.high,
            group.low,
            group.first,
            group.last,
            group.negative,
            group.display_x_axis,
            group.display_hidden,
            axis_type_to_calcext(group.min_axis_type),
            axis_type_to_calcext(group.max_axis_type),
            group.right_to_left,
            group.manual_max.map(|v| format!(r#" calcext:max-axis-value="{}""#, v)).unwrap_or_default(),
            group.manual_min.map(|v| format!(r#" calcext:min-axis-value="{}""#, v)).unwrap_or_default(),
            group.color_series.as_deref().map(|c| format!(r#" calcext:color-series="{}""#, c)).unwrap_or_default(),
            group.color_negative.as_deref().map(|c| format!(r#" calcext:color-negative="{}""#, c)).unwrap_or_default(),
            group.color_axis.as_deref().map(|c| format!(r#" calcext:color-axis="{}""#, c)).unwrap_or_default(),
            group.color_markers.as_deref().map(|c| format!(r#" calcext:color-markers="{}""#, c)).unwrap_or_default(),
            group.color_first.as_deref().map(|c| format!(r#" calcext:color-first="{}""#, c)).unwrap_or_default(),
            group.color_last.as_deref().map(|c| format!(r#" calcext:color-last="{}""#, c)).unwrap_or_default(),
            group.color_high.as_deref().map(|c| format!(r#" calcext:color-high="{}""#, c)).unwrap_or_default(),
            group.color_low.as_deref().map(|c| format!(r#" calcext:color-low="{}""#, c)).unwrap_or_default(),
        );
        for sparkline in &group.sparklines {
            let (row, col) = sparkline.cell;
            let data_range = sparkline.input_range.first()
                .map(|r| range_to_str(r))
                .unwrap_or_default();
            let _ = writeln!(
                out,
                r#"            <calcext:sparkline calcext:cell-address="{}" calcext:data-range="{}"/>"#,
                cell_addr_to_str(&sheet.name, row, col),
                data_range
            );
        }
        out.push_str("          </calcext:sparkline-group>
");
    }
    out.push_str("        </calcext:sparkline-groups>
");
}

fn range_to_str(r: &roe_model::calc::CellRange) -> String {
    format!(
        "{}.{}{}:{}.{}{}",
        xml_escape_attr(&r.sheet),
        col_letter(r.start.1),
        r.start.0 + 1,
        xml_escape_attr(&r.sheet),
        col_letter(r.end.1),
        r.end.0 + 1
    )
}

fn col_letter(mut col: u32) -> String {
    let mut s = String::new();
    loop {
        let rem = col % 26;
        s.insert(0, (b'A' + rem as u8) as char);
        if col < 26 {
            break;
        }
        col = col / 26 - 1;
    }
    s
}

// ──────────────────────────────────────────────
// 辅助：最大行/列、空行判断、取值
// ──────────────────────────────────────────────

fn compute_max_row(sheet: &Sheet) -> u32 {
    let mut max_row = 0u32;
    for col in sheet.cells.cols.values() {
        for block in &col.blocks {
            let end = block.start_row + block.values.len() as u32;
            if end > max_row {
                max_row = end;
            }
        }
    }
    for &(r, _) in sheet.cell_formats.keys() {
        max_row = max_row.max(r + 1);
    }
    for m in &sheet.merged_cells {
        max_row = max_row.max(m.r2 + 1);
    }
    for dv in &sheet.data_validations {
        max_row = max_row.max(dv.range.end.0 + 1);
    }
    max_row.saturating_sub(1).max(0)
}

fn compute_max_col(sheet: &Sheet) -> u32 {
    let mut max_col = sheet.cells.cols.keys().copied().max().unwrap_or(0);
    for &(_, c) in sheet.cell_formats.keys() {
        max_col = max_col.max(c);
    }
    for m in &sheet.merged_cells {
        max_col = max_col.max(m.c2);
    }
    for dv in &sheet.data_validations {
        max_col = max_col.max(dv.range.end.1);
    }
    max_col
}

fn get_cell_value(sheet: &Sheet, row: u32, col: u32) -> CellValue {
    sheet
        .cells
        .cols
        .get(&col)
        .and_then(|sc| {
            for block in &sc.blocks {
                if row >= block.start_row {
                    let idx = (row - block.start_row) as usize;
                    if idx < block.values.len() {
                        return Some(block.values[idx].clone());
                    }
                }
            }
            None
        })
        .unwrap_or(CellValue::Empty)
}

fn row_is_empty(
    sheet_idx: usize,
    sheet: &Sheet,
    row: u32,
    max_col: u32,
    covered: &HashMap<(u32, u32), &MergedCell>,
    merge_origin: &HashMap<(u32, u32), &MergedCell>,
    cell_validation_names: &HashMap<(usize, u32, u32), String>,
) -> bool {
    for col in 0..=max_col {
        if covered.contains_key(&(row, col)) || merge_origin.contains_key(&(row, col)) {
            return false;
        }
        if sheet.cell_formats.contains_key(&(row, col)) {
            return false;
        }
        if cell_validation_names.contains_key(&(sheet_idx, row, col)) {
            return false;
        }
        if !matches!(get_cell_value(sheet, row, col), CellValue::Empty) {
            return false;
        }
    }
    true
}

// ──────────────────────────────────────────────
// XML 转义
// ──────────────────────────────────────────────

fn xml_escape_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn xml_escape_attr(s: &str) -> String {
    xml_escape_text(s).replace('"', "&quot;")
}

// ──────────────────────────────────────────────
// 测试
// ──────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::odf::fods::FodsParser;
    use roe_model::calc::{CellFormat, CellRange, ConditionalFormatRule};
    use std::collections::HashMap as StdHashMap;

    fn empty_sheet(name: &str) -> Sheet {
        Sheet {
            col_style: vec![],
            tables: vec![],
            name: name.to_string(),
            cells: Default::default(),
            col_widths: vec![],
            row_heights: vec![],
            frozen_rows: 0,
            frozen_cols: 0,
        date1904: false, // ROOT-041: csv/odf场景无workbookPr概念，固定为1900系统
            zoom_scale: None,
            show_grid_lines: None,
            col_hidden: vec![],
            row_hidden: vec![],
            col_outline_level: vec![],
            row_outline_level: vec![],
            cell_formats: StdHashMap::new(),
            cond_formats: vec![],
            data_validations: vec![],
            auto_filter: None,
            merged_cells: vec![],
            pivot:        None,
            floating_charts: vec![],
            floating_stat_charts: vec![],
            floating_images:  vec![],
            floating_shapes: vec![],
            next_object_id: 0,
            default_col_width: 0.0,
            default_row_height: 0.0,
            sparkline_groups: vec![],
        }
    }

    fn set_value(sheet: &mut Sheet, row: u32, col: u32, value: CellValue) {
        let sc = sheet.cells.cols.entry(col).or_default();
        sc.blocks.push(roe_model::calc::CellBlock {
            start_row: row,
            values: smallvec::smallvec![value],
        });
    }

    #[test]
    fn test_roundtrip_basic_values() {
        let mut sheet = empty_sheet("Sheet1");
        set_value(&mut sheet, 0, 0, CellValue::Text("Hello".into()));
        set_value(&mut sheet, 0, 1, CellValue::Number(42.0));
        set_value(&mut sheet, 0, 2, CellValue::Bool(true));
        set_value(&mut sheet, 2, 0, CellValue::Number(-3.14));

        let wb = Workbook {
            sheets: vec![sheet],
            named_ranges: StdHashMap::new(),
            named_formulas: StdHashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        };

        let xml = FodsWriter::write(&wb);
        let parsed = FodsParser::parse(&xml).expect("roundtrip parse failed");

        assert_eq!(parsed.sheets.len(), 1);
        let s = &parsed.sheets[0];
        assert_eq!(s.name, "Sheet1");

        let v = get_cell_value(s, 0, 0);
        assert!(matches!(v, CellValue::Text(ref t) if t == "Hello"));
        let v = get_cell_value(s, 0, 1);
        assert!(matches!(v, CellValue::Number(n) if (n - 42.0).abs() < 1e-9));
        let v = get_cell_value(s, 0, 2);
        assert!(matches!(v, CellValue::Bool(true)));
        let v = get_cell_value(s, 2, 0);
        assert!(matches!(v, CellValue::Number(n) if (n - (-3.14)).abs() < 1e-6));
    }

    #[test]
    fn test_roundtrip_cell_format() {
        let mut sheet = empty_sheet("Sheet1");
        set_value(&mut sheet, 0, 0, CellValue::Text("Bold".into()));
        sheet.cell_formats.insert(
            (0, 0),
            CellFormat {
                bold: Some(true),
                italic: Some(false),
                fg_color: Some("#FF0000".into()),
                ..Default::default()
            },
        );

        let wb = Workbook {
            sheets: vec![sheet],
            named_ranges: StdHashMap::new(),
            named_formulas: StdHashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        };

        let xml = FodsWriter::write(&wb);
        // parser 目前不解析 style:style 回 cell_formats（parser 端尚未实现该方向），
        // 这里只验证 XML 结构本身合法可被 parse（不报错），且基础值仍正确还原。
        let parsed = FodsParser::parse(&xml).expect("roundtrip parse failed");
        let v = get_cell_value(&parsed.sheets[0], 0, 0);
        assert!(matches!(v, CellValue::Text(ref t) if t == "Bold"));

        // 检查生成的 XML 里确实含有对应的 style 标签（结构层面验证，非 roundtrip）
        assert!(xml.contains(r#"style:family="table-cell""#));
        assert!(xml.contains(r#"fo:font-weight="bold""#));
        assert!(xml.contains("fo:color=\"#FF0000\""));
    }

    #[test]
    fn test_roundtrip_merged_cells() {
        let mut sheet = empty_sheet("Sheet1");
        set_value(&mut sheet, 0, 0, CellValue::Text("Merged".into()));
        sheet.merged_cells.push(MergedCell { r1: 0, c1: 0, r2: 1, c2: 1 });

        let wb = Workbook {
            sheets: vec![sheet],
            named_ranges: StdHashMap::new(),
            named_formulas: StdHashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        };

        let xml = FodsWriter::write(&wb);
        assert!(xml.contains(r#"table:number-columns-spanned="2""#));
        assert!(xml.contains(r#"table:number-rows-spanned="2""#));
        assert!(xml.contains("covered-table-cell"));

        // 验证 parser 至少能正常解析通过（不保证 merged_cells 字段回填，
        // parser 端目前没有读取 spanned 属性的逻辑）
        let parsed = FodsParser::parse(&xml).expect("roundtrip parse failed");
        assert_eq!(parsed.sheets.len(), 1);
    }

    #[test]
    fn test_sparse_empty_compression() {
        let mut sheet = empty_sheet("Sheet1");
        set_value(&mut sheet, 0, 0, CellValue::Number(1.0));
        set_value(&mut sheet, 100, 50, CellValue::Number(2.0));

        let wb = Workbook {
            sheets: vec![sheet],
            named_ranges: StdHashMap::new(),
            named_formulas: StdHashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        };

        let xml = FodsWriter::write(&wb);
        // 期望空行被压缩成 number-rows-repeated，而不是逐行展开 100 行
        assert!(xml.contains("number-rows-repeated"));
        assert!(xml.contains("number-columns-repeated"));

        let parsed = FodsParser::parse(&xml).expect("roundtrip parse failed");
        let v = get_cell_value(&parsed.sheets[0], 0, 0);
        assert!(matches!(v, CellValue::Number(n) if (n - 1.0).abs() < 1e-9));
        let v = get_cell_value(&parsed.sheets[0], 100, 50);
        assert!(matches!(v, CellValue::Number(n) if (n - 2.0).abs() < 1e-9));
    }

    #[test]
    fn test_extras_written_but_not_roundtripped() {
        let mut sheet = empty_sheet("Sheet1");
        sheet.cond_formats.push(ConditionalFormatRule {
            range: CellRange { sheet: "Sheet1".into(), start: (0, 0), end: (5, 0) },
            kind: roe_model::calc::ConditionalFormatKind::CellFormat {
                condition: "gt:0".into(),
                format: CellFormat::default(),
            },
            stop_if_true: false,
        });

        let wb = Workbook {
            sheets: vec![sheet.clone()],
            named_ranges: StdHashMap::new(),
            named_formulas: StdHashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        };

        // write_extras 已接入 write_sheet 主流程（写在 </table:table> 之前）。
        // 单独调用同一函数验证内容合法。
        let mut out = String::new();
        write_extras(&mut out, &sheet);
        assert!(out.contains("calcext:conditional-format"));
        assert!(out.contains(r#"calcext:value="gt:0""#));

        // 再验证完整 write() 输出确实包含这部分（防止以后误删调用）
        let full_xml = FodsWriter::write(&wb);
        assert!(full_xml.contains("calcext:conditional-format"));
    }

    #[test]
    fn test_export_colorscale_to_calcext() {
        let mut sheet = empty_sheet("Sheet1");
        sheet.cond_formats.push(ConditionalFormatRule {
            range: CellRange { sheet: "Sheet1".into(), start: (0, 0), end: (1, 0) },
            kind: roe_model::calc::ConditionalFormatKind::ColorScale {
                min_type: "min".to_string(),
                min_value: None,
                min_color: "#F8696B".to_string(),
                mid: Some(("percentile".to_string(), "50".to_string(), "#FFEB84".to_string())),
                max_type: "max".to_string(),
                max_value: None,
                max_color: "#63BE7B".to_string(),
            },
            stop_if_true: false,
        });

        let mut out = String::new();
        write_extras(&mut out, &sheet);

        assert!(out.contains("calcext:color-scale"), "应包含 color-scale 标签, got:\n{}", out);
        assert!(out.contains(r#"calcext:type="minimum""#), "min阈值类型应映射为minimum, got:\n{}", out);
        assert!(out.contains(r#"calcext:type="maximum""#), "max阈值类型应映射为maximum, got:\n{}", out);
        assert!(out.contains(r#"calcext:type="percentile""#), "mid阈值类型应为percentile, got:\n{}", out);
        assert!(out.contains(r#"calcext:value="50""#), "mid阈值数值应为50, got:\n{}", out);
        assert!(out.contains("#F8696B"), "应包含min颜色, got:\n{}", out);
        assert!(out.contains("#63BE7B"), "应包含max颜色, got:\n{}", out);
        assert!(out.contains("#FFEB84"), "应包含mid颜色, got:\n{}", out);
        println!("colorscale export ✓");
    }

    #[test]
    fn test_export_databar_to_calcext() {
        let mut sheet = empty_sheet("Sheet1");
        sheet.cond_formats.push(ConditionalFormatRule {
            range: CellRange { sheet: "Sheet1".into(), start: (0, 0), end: (1, 0) },
            kind: roe_model::calc::ConditionalFormatKind::DataBar {
                min: roe_model::calc::CfValue { kind: "min".to_string(), val: String::new(), gte: true },
                max: roe_model::calc::CfValue { kind: "max".to_string(), val: String::new(), gte: true },
                bar_color: "#638EC6".to_string(),
                border_color: None,
                direction: roe_model::calc::BarDirection::Auto,
                bar_only: true,
                solid_fill: true,
                negative_fill_color: None,
                axis_color: None,
                axis_position: "automatic".to_string(),
            },
            stop_if_true: false,
        });

        let mut out = String::new();
        write_extras(&mut out, &sheet);

        assert!(out.contains("calcext:data-bar"), "应包含 data-bar 标签, got:\n{}", out);
        assert!(out.contains("calcext:positive-color=\"#638EC6\""), "应包含正色, got:\n{}", out);
        assert!(out.contains(r#"calcext:gradient="false""#), "solid_fill=true 应写 gradient=false, got:\n{}", out);
        assert!(out.contains(r#"calcext:show-value="false""#), "bar_only=true 应写 show-value=false, got:\n{}", out);
        assert!(out.contains(r#"calcext:type="minimum""#), "应包含minimum阈值, got:\n{}", out);
        assert!(out.contains(r#"calcext:type="maximum""#), "应包含maximum阈值, got:\n{}", out);
        println!("databar export ✓");
    }

    #[test]
    fn test_export_iconset_to_calcext() {
        let mut sheet = empty_sheet("Sheet1");
        sheet.cond_formats.push(ConditionalFormatRule {
            range: CellRange { sheet: "Sheet1".into(), start: (0, 0), end: (1, 0) },
            kind: roe_model::calc::ConditionalFormatKind::IconSet {
                icon_style: "3TrafficLights1".to_string(),
                thresholds: vec![
                    roe_model::calc::CfValue { kind: "percent".to_string(), val: "0".to_string(), gte: true },
                    roe_model::calc::CfValue { kind: "percent".to_string(), val: "33".to_string(), gte: true },
                    roe_model::calc::CfValue { kind: "percent".to_string(), val: "67".to_string(), gte: false },
                ],
                percent: true,
                reverse: false,
                icons_only: true,
                tier_overrides: None,
            },
            stop_if_true: false,
        });

        let mut out = String::new();
        write_extras(&mut out, &sheet);

        assert!(out.contains("calcext:icon-set"), "应包含 icon-set 标签, got:\n{}", out);
        assert!(out.contains(r#"calcext:icon-set-type="3TrafficLights1""#), "应包含图标集样式名, got:\n{}", out);
        assert!(out.contains(r#"calcext:show-value="false""#), "icons_only=true 应写 show-value=false, got:\n{}", out);
        assert!(out.contains(r#"calcext:value="33""#), "应包含阈值33, got:\n{}", out);
        assert!(out.contains(r#"calcext:greater-equal="false""#), "gte=false的阈值应写greater-equal=false, got:\n{}", out);
        println!("iconset export ✓");
    }

    // dataValidation 写方向测试（v103 新增）

    fn dv_rule(
        kind: &str,
        operator: &str,
        formula1: &str,
        formula2: Option<&str>,
        range: CellRange,
        show_error: bool,
        error_msg: Option<&str>,
    ) -> roe_model::calc::DataValidationRule {
        roe_model::calc::DataValidationRule {
            range,
            kind: kind.to_string(),
            operator: operator.to_string(),
            formula1: formula1.to_string(),
            formula2: formula2.map(|s| s.to_string()),
            allow_blank: true,
            show_error,
            error_msg: error_msg.map(|s| s.to_string()),
        }
    }

    #[test]
    fn test_xlsx_list_formula_to_odf_basic() {
        let out = xlsx_list_formula_to_odf("\"苹果,香蕉,橙子\"");
        assert_eq!(out, "\"苹果\";\"香蕉\";\"橙子\"");
    }

    #[test]
    fn test_xlsx_list_formula_to_odf_not_quoted_passthrough() {
        let out = xlsx_list_formula_to_odf("Sheet1.$A$1:$A$5");
        assert_eq!(out, "Sheet1.$A$1:$A$5");
    }

    #[test]
    fn test_xlsx_list_formula_to_odf_single_item() {
        let out = xlsx_list_formula_to_odf("\"仅一项\"");
        assert_eq!(out, "\"仅一项\"");
    }

    #[test]
    fn test_dv_write_whole_between() {
        let mut sheet = empty_sheet("Sheet1");
        sheet.data_validations.push(dv_rule(
            "whole", "between", "1", Some("100"),
            CellRange { sheet: "Sheet1".into(), start: (0, 0), end: (0, 0) },
            true, Some("请输入1到100之间的整数"),
        ));

        let wb = Workbook {
            sheets: vec![sheet],
            named_ranges: StdHashMap::new(),
            named_formulas: StdHashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        };

        let xml = FodsWriter::write(&wb);

        assert!(xml.contains("<table:content-validations>"), "got:\n{}", xml);
        assert!(xml.contains("table:condition=\"oooc:cell-content-is-whole-number() and cell-content-is-between(1,100)\""), "got:\n{}", xml);
        assert!(xml.contains("table:validation-name=\"dv_0\""), "got:\n{}", xml);
        assert!(xml.contains("请输入1到100之间的整数"), "got:\n{}", xml);

        FodsParser::parse(&xml).expect("dv write xml should parse");
    }

    #[test]
    fn test_dv_write_decimal_single_operator() {
        let mut sheet = empty_sheet("Sheet1");
        sheet.data_validations.push(dv_rule(
            "decimal", "greaterThan", "0", None,
            CellRange { sheet: "Sheet1".into(), start: (1, 1), end: (1, 1) },
            false, None,
        ));

        let wb = Workbook {
            sheets: vec![sheet],
            named_ranges: StdHashMap::new(),
            named_formulas: StdHashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        };

        let xml = FodsWriter::write(&wb);
        assert!(xml.contains("table:condition=\"oooc:cell-content-is-decimal-number() and cell-content()&gt;0\""), "got:\n{}", xml);
        FodsParser::parse(&xml).expect("xml should parse");
    }

    #[test]
    fn test_dv_write_text_length() {
        let mut sheet = empty_sheet("Sheet1");
        sheet.data_validations.push(dv_rule(
            "textLength", "lessThanOrEqual", "20", None,
            CellRange { sheet: "Sheet1".into(), start: (2, 0), end: (2, 0) },
            false, None,
        ));

        let wb = Workbook {
            sheets: vec![sheet],
            named_ranges: StdHashMap::new(),
            named_formulas: StdHashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        };

        let xml = FodsWriter::write(&wb);
        assert!(xml.contains("oooc:cell-content-text-length()&lt;=20"), "got:\n{}", xml);
        FodsParser::parse(&xml).expect("xml should parse");
    }

    #[test]
    fn test_dv_write_list_no_double_escape() {
        let mut sheet = empty_sheet("Sheet1");
        sheet.data_validations.push(dv_rule(
            "list", "", "\"苹果,香蕉,橙子\"", None,
            CellRange { sheet: "Sheet1".into(), start: (3, 0), end: (3, 0) },
            false, None,
        ));

        let wb = Workbook {
            sheets: vec![sheet],
            named_ranges: StdHashMap::new(),
            named_formulas: StdHashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        };

        let xml = FodsWriter::write(&wb);

        assert!(!xml.contains("&amp;quot;"), "should not double-escape, got:\n{}", xml);
        assert!(xml.contains("oooc:cell-content-is-in-list(&quot;苹果&quot;;&quot;香蕉&quot;;&quot;橙子&quot;)"), "got:\n{}", xml);

        FodsParser::parse(&xml).expect("xml should parse");
    }

    #[test]
    fn test_dv_write_multi_cell_same_rule_merges_rect() {
        let mut sheet = empty_sheet("Sheet1");
        sheet.data_validations.push(dv_rule(
            "whole", "greaterThan", "0", None,
            CellRange { sheet: "Sheet1".into(), start: (0, 0), end: (2, 2) },
            false, None,
        ));

        let wb = Workbook {
            sheets: vec![sheet],
            named_ranges: StdHashMap::new(),
            named_formulas: StdHashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        };

        let xml = FodsWriter::write(&wb);

        let rule_count = xml.matches("<table:content-validation ").count();
        assert_eq!(rule_count, 1, "got xml:\n{}", xml);

        let ref_count = xml.matches("table:validation-name=\"dv_0\"").count();
        assert!(ref_count >= 1, "got:\n{}", xml);

        FodsParser::parse(&xml).expect("xml should parse");
    }

    #[test]
    fn test_dv_write_show_error_true_but_no_message() {
        let mut sheet = empty_sheet("Sheet1");
        sheet.data_validations.push(dv_rule(
            "whole", "greaterThan", "0", None,
            CellRange { sheet: "Sheet1".into(), start: (0, 0), end: (0, 0) },
            true, None,
        ));

        let wb = Workbook {
            sheets: vec![sheet],
            named_ranges: StdHashMap::new(),
            named_formulas: StdHashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        };

        let xml = FodsWriter::write(&wb);
        assert!(xml.contains("<table:error-message table:display=\"true\"><text:p></text:p></table:error-message>"), "got:\n{}", xml);
        FodsParser::parse(&xml).expect("xml should parse");
    }

    #[test]
    fn test_dv_write_show_error_false_no_message_tag() {
        let mut sheet = empty_sheet("Sheet1");
        sheet.data_validations.push(dv_rule(
            "whole", "greaterThan", "0", None,
            CellRange { sheet: "Sheet1".into(), start: (0, 0), end: (0, 0) },
            false, None,
        ));

        let wb = Workbook {
            sheets: vec![sheet],
            named_ranges: StdHashMap::new(),
            named_formulas: StdHashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        };

        let xml = FodsWriter::write(&wb);
        assert!(!xml.contains("table:error-message"), "got:\n{}", xml);
        FodsParser::parse(&xml).expect("xml should parse");
    }

    #[test]
    fn test_dv_write_date_time_custom_none_skipped() {
        let mut sheet = empty_sheet("Sheet1");
        for kind in ["date", "time", "custom", "none"] {
            sheet.data_validations.push(dv_rule(
                kind, "greaterThan", "0", None,
                CellRange { sheet: "Sheet1".into(), start: (0, 0), end: (0, 0) },
                false, None,
            ));
        }

        let wb = Workbook {
            sheets: vec![sheet],
            named_ranges: StdHashMap::new(),
            named_formulas: StdHashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        };

        let xml = FodsWriter::write(&wb);
        assert!(!xml.contains("<table:content-validation "), "got:\n{}", xml);
        FodsParser::parse(&xml).expect("xml should parse");
    }

    // ─── 真实文件 round-trip 测试（ODF §5.2 阶段三，v186后续）────────────

    /// 按坐标比较两个 Workbook 在 writer 支持范围内的字段：cell 值
    /// （所有 sheet 的所有非空单元格，按坐标用 SparseGrid::get 比较，
    /// 不假设 block 布局一致）+ merged_cells（区域集合比较，忽略顺序）。
    /// 不比较 cond_formats/data_validations/auto_filter，因为 fods.rs
    /// parser 不解析这几个标签（文件头注释明确说明），写出后读回必然
    /// 是空/None，这是已知的范围外取舍，比较时主动跳过。
    fn assert_ods_workbook_match_within_writer_scope(
        expected: &Workbook,
        actual: &Workbook,
        path: &str,
    ) {
        assert_eq!(
            expected.sheets.len(), actual.sheets.len(),
            "{path}: sheet 数量不一致"
        );

        for (si, (exp_sheet, act_sheet)) in expected.sheets.iter().zip(actual.sheets.iter()).enumerate() {
            let s_path = format!("{path} sheet[{si}]");

            for (&col, column) in exp_sheet.cells.cols.iter() {
                for block in &column.blocks {
                    for (offset, exp_val) in block.values.iter().enumerate() {
                        let row = block.start_row + offset as u32;
                        if matches!(exp_val, CellValue::Empty) {
                            continue;
                        }
                        let act_val = act_sheet.cells.get(row, col);
                        assert_eq!(
                            format!("{exp_val:?}"), format!("{act_val:?}"),
                            "{s_path} cell(row={row},col={col}): 值不一致 exp={exp_val:?} act={act_val:?}"
                        );
                    }
                }
            }

            let mut exp_merges: Vec<String> = exp_sheet.merged_cells.iter().map(|m| format!("{m:?}")).collect();
            let mut act_merges: Vec<String> = act_sheet.merged_cells.iter().map(|m| format!("{m:?}")).collect();
            exp_merges.sort();
            act_merges.sort();
            assert_eq!(
                exp_merges, act_merges,
                "{s_path}: merged_cells 不一致"
            );
        }
    }

    fn run_real_file_roundtrip_ods(path: &str) {
        let data = std::fs::read(path)
            .unwrap_or_else(|e| panic!("读取样本文件失败 {path}: {e}"));

        let expected = if path.ends_with(".fods") {
            crate::RoeDocument::open_fods_bytes(&data)
        } else {
            crate::RoeDocument::open_odf_bytes(&data)
        }
        .unwrap_or_else(|e| panic!("首次 parse 失败 {path}: {e:?}"));
        let expected = match expected {
            crate::RoeDocument::Calc(w) => w,
            other => panic!("{path}: 不是 Calc 文档 ({other:?} variant)"),
        };

        let xml = FodsWriter::write(&expected);
        let actual = FodsParser::parse(&xml)
            .unwrap_or_else(|e| panic!("write→reparse 失败 {path}: {e:?}"));

        assert_ods_workbook_match_within_writer_scope(&expected, &actual, path);
    }

    #[test]
    fn test_real_file_roundtrip_tdf151046() {
        // 探测脚本 scan_ods_roundtrip_candidates 挑选：3 cells，
        // number+formula 混合，全 clean（无 cond_fmt/validation/filter），
        // 小体量基线样本。
        run_real_file_roundtrip_ods(
            "/home/yong/Projects/roe/test_samples/curated/ods/small__sc_data_ods__tdf151046.ods",
        );
    }

    #[test]
    fn test_real_file_roundtrip_tdf146795() {
        // 探测脚本挑选：18 cells，number/text/formula 三种类型都覆盖，
        // 全 clean，中等复杂度主力基线样本。
        run_real_file_roundtrip_ods(
            "/home/yong/Projects/roe/test_samples/curated/ods/small__sc_uicalc_data__tdf146795.ods",
        );
    }
}

// ──────────────────────────────────────────────
// floating_images / floating_shapes 写出
// ──────────────────────────────────────────────
//
// ODF draw:frame 定位方式与 OOXML 不同：不是相对单元格锚点，而是
// svg:x/svg:y/svg:width/svg:height 四个绝对长度属性（与 fods.rs parser
// 的 parse_frame_image/parse_shapes_charts 保持一致，单位用 cm，
// pt -> cm 换算沿用本文件既有的 ÷28.3465 惯例）。
//
// 图片写出用 office:binary-data base64 内嵌模式（与现有 fods.rs 解析器
// 对齐，不是 ODF Package 的 xlink:href 外部文件引用模式——后者是下一轮
// "真正 assets 落地" 任务的范围，本次不做，避免同时改 writer 和 parser）。
//
// floating_charts（图表）本次不处理，留待后续任务：ODF chart 写出需要
// 完整的内嵌 draw:object 子文档，复杂度显著高于图片/形状。
fn write_floating_objects(out: &mut String, sheet: &Sheet) {
    if sheet.floating_images.iter().all(|i| i.hidden) && sheet.floating_shapes.is_empty() {
        return;
    }
    out.push_str("        <table:shapes>\n");
    for img in &sheet.floating_images {
        if img.hidden {
            continue;
        }
        write_floating_image(out, img);
    }
    for shape in &sheet.floating_shapes {
        write_floating_shape(out, shape);
    }
    out.push_str("        </table:shapes>\n");
}

fn write_floating_image(out: &mut String, img: &roe_model::calc::FloatingImage) {
    let _ = writeln!(
        out,
        r#"        <draw:frame svg:x="{:.4}cm" svg:y="{:.4}cm" svg:width="{:.4}cm" svg:height="{:.4}cm"{}>"#,
        img.x_pt / 28.3465,
        img.y_pt / 28.3465,
        img.width_pt / 28.3465,
        img.height_pt / 28.3465,
        rotation_attr(img.rot),
    );
    out.push_str("          <draw:image>
");
    // DEVROOT-002修复：office:binary-data 此前缺少 mime-type 属性，
    // ODF标准要求消费者靠此属性判断如何解码base64——真实LibreOffice
    // 遇到缺失时不报错，静默降级为占位符渲染（纯色矩形，非应有图片
    // 内容），ROE自身roundtrip测试因不做命名空间/属性完整性校验而
    // 未能发现。写法参照 fodp_writer.rs::write_shape_image（已验证
    // 对LibreOffice正常工作）。
    let _ = write!(out, "            <office:binary-data mime-type=\"{}\">", xml_escape_attr(&img.mime));
    out.push_str(&img.data_base64);
    out.push_str("</office:binary-data>
");
    out.push_str("          </draw:image>
");
    out.push_str("        </draw:frame>
");
}

/// draw:frame 的旋转表达。ODF 标准做法是 draw:transform="rotate(...)"
/// （弧度制，围绕原点，需配合 translate 才能保持视觉位置不变），完整
/// 实现较复杂；本次采用近似简化——非零旋转时退化为不写 transform
/// （与 fodp_writer 里 Picture/PictureTiled fill 的"已知简化非遗漏"
/// 处理原则一致），仅在 rot 为 0 时不输出任何多余属性。ROE 自身
/// roundtrip 不受影响（parser 目前也不读 rot from ODF）。
fn rotation_attr(_rot: f32) -> &'static str {
    ""
}

fn write_floating_shape(out: &mut String, shape: &roe_model::calc::FloatingShape) {
    let (open_tag, close_tag, extra_attrs) = geometry_tag_calc(&shape.geometry);
    let fill_attr = fill_style_attr(shape.fill.as_ref());

    let _ = writeln!(
        out,
        r#"        <{} svg:x="{:.4}cm" svg:y="{:.4}cm" svg:width="{:.4}cm" svg:height="{:.4}cm"{}>"#,
        open_tag,
        shape.x_pt / 28.3465,
        shape.y_pt / 28.3465,
        shape.width_pt / 28.3465,
        shape.height_pt / 28.3465,
        extra_attrs,
    );
    if !fill_attr.is_empty() {
        let _ = writeln!(out, r#"          <style:style style:family="graphic"><style:graphic-properties{}/></style:style>"#, fill_attr);
    }
    if !shape.text_content.is_empty() {
        out.push_str("          <draw:text-box>
");
        for p in &shape.text_content {
            let text: String = p.runs.iter().map(|r| r.text.as_str()).collect();
            let _ = writeln!(out, "            <text:p>{}</text:p>", xml_escape_text(&text));
        }
        out.push_str("          </draw:text-box>
");
    }
    let _ = writeln!(out, "        </{}>", close_tag);
}

/// Calc 侧几何->ODF标签映射。与 fodp_writer.rs 的 geometry_tag 逻辑
/// 保持同一简化策略（非基础形状统一退化为 draw:custom-shape），
/// 独立成一份小函数而非跨 crate 内部件共享，避免 fods_writer.rs 对
/// fodp_writer.rs 产生不必要的内部依赖（两者是同级平行文件，风格
/// 一致但不强制耦合）。
fn geometry_tag_calc(geo: &roe_model::common::ShapeGeometry) -> (&'static str, &'static str, String) {
    use roe_model::common::ShapeGeometry;
    match geo {
        ShapeGeometry::Rect => ("draw:rect", "draw:rect", String::new()),
        ShapeGeometry::Ellipse => ("draw:ellipse", "draw:ellipse", String::new()),
        ShapeGeometry::Line => ("draw:line", "draw:line", String::new()),
        _ => ("draw:custom-shape", "draw:custom-shape", String::new()),
    }
}

/// FloatingShape.fill -> style:graphic-properties 内联属性字符串。
/// 与 fodp_writer::write_graphic_style_def 同一降级策略：Gradient 取
/// 首尾色近似，Picture/PictureTiled 降级为 none（本次 ODF 写出范围
/// 不含图片填充，避免二次读取图片数据做嵌套 base64）。
fn fill_style_attr(fill: Option<&roe_model::common::FillStyle>) -> String {
    use roe_model::common::FillStyle;
    match fill {
        Some(FillStyle::Solid(c)) => {
            format!(r##" draw:fill="solid" draw:fill-color="#{:02X}{:02X}{:02X}""##, c.r, c.g, c.b)
        }
        Some(FillStyle::Gradient { stops, .. }) => {
            let start = stops.first().map(|s| s.color)
                .unwrap_or(roe_model::common::Color { r: 200, g: 220, b: 245, a: 255 });
            format!(r##" draw:fill="solid" draw:fill-color="#{:02X}{:02X}{:02X}""##, start.r, start.g, start.b)
        }
        Some(FillStyle::None) => r#" draw:fill="none""#.to_string(),
        _ => String::new(),
    }
}
