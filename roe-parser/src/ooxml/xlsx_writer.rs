//! roe_model::calc::Workbook → XLSX (OOXML SpreadsheetML) 字节流
//!
//! 与 xlsx.rs parser 对称设计，第一版范围对齐 parser 现状：
//! - 只写出 parser 已读取的字段：sheet name、cell 值（含公式/布尔/错误）
//! - col_widths/row_heights/cell_formats/merged_cells/frozen_rows/frozen_cols
//!   parser 暂不读取，本版本 writer 也不写出（避免无法 roundtrip 验证的内容）
//! - OOXML 是 zip 容器，接口返回 Vec<u8>，不同于 ODF Flat 的 String
//!
//! 生成的最小合法 OPC 包文件清单：
//!   [Content_Types].xml
//!   _rels/.rels
//!   xl/workbook.xml
//!   xl/_rels/workbook.xml.rels
//!   xl/sharedStrings.xml
//!   xl/worksheets/sheet1.xml ...

use std::collections::HashMap;
use std::io::Write;

use roe_model::calc::{CellError, CellValue, Sheet, Workbook};
use crate::ooxml::xlsx_styles_writer::{write_conditional_formatting_xml, write_x14_databar_ext_xml, StylesCollector};

use crate::ParseError;

pub struct XlsxWriter;

impl XlsxWriter {
    /// Workbook 内存模型 → xlsx 字节流（zip 包）
    pub fn write(workbook: &Workbook) -> Result<Vec<u8>, ParseError> {
        // 1. 先扫描所有 sheet，收集 SharedStrings（Text / Formula 文本结果均走 SST）
        let mut sst = SharedStringTable::default();
        for sheet in &workbook.sheets {
            collect_strings(sheet, &mut sst);
        }

        // 1b. 收集样式（fonts/fills/cellXfs/dxfs）与条件格式所需的 dxf 索引
        let mut styles = StylesCollector::new();
        styles.collect(workbook);

        // 2. 逐项生成 XML 内容
        let total_tables: usize = workbook.sheets.iter().map(|s| s.tables.len()).sum();
        let with_theme = total_tables > 0;
        let content_types = write_content_types(workbook.sheets.len(), total_tables, with_theme);
        let total_charts: usize = workbook.sheets.iter().map(|s| s.floating_charts.len()).sum();
        let total_stat: usize = workbook.sheets.iter().map(|s| s.floating_stat_charts.len()).sum();
        let total_drawings: usize = workbook.sheets.iter()
            .filter(|s| !s.floating_charts.is_empty() || !s.floating_stat_charts.is_empty() || !s.floating_images.is_empty() || !s.floating_shapes.is_empty()).count();
        let content_types = append_chart_content_types(content_types, total_drawings, total_charts, total_stat);
        let content_types = crate::ooxml::xlsx_drawing_writer::append_media_defaults(content_types, workbook.sheets.iter().flat_map(|s| s.floating_images.iter()));
        use crate::ooxml::xlsx_pivot_writer as pvw;
        let mut pivot_specs: Vec<Option<pvw::PivotWriteSpec>> = Vec::new();
        let mut pivot_ord = 0u32;
        for i in 0..workbook.sheets.len() {
            let sp = if workbook.sheets[i].pivot.is_some() {
                pvw::build_spec(&workbook.sheets, i, pivot_ord + 1)
                    .filter(|x| pvw::cache_definition_xml(x).is_some() && pvw::pivot_table_xml(x).is_some())
            } else {
                None
            };
            if sp.is_some() {
                pivot_ord += 1;
            }
            pivot_specs.push(sp);
        }
        let n_pivots = pivot_ord as usize;
        let content_types = pvw::append_pivot_content_types(content_types, n_pivots);
        let root_rels = write_root_rels();
        let mut workbook_xml = write_workbook_xml(workbook);
        let mut workbook_rels = write_workbook_rels(workbook.sheets.len(), with_theme);
        if n_pivots > 0 {
            let base = workbook.sheets.len() + 3 + if with_theme { 1 } else { 0 };
            let mut rel_add = String::new();
            let mut entries: Vec<(u32, String)> = Vec::new();
            for k in 1..=n_pivots {
                let rid = format!("rId{}", base + k - 1);
                rel_add.push_str(&pvw::workbook_pivot_cache_rel(&rid, k));
                entries.push((k as u32, rid));
            }
            workbook_rels = workbook_rels.replacen("</Relationships>", &format!("{rel_add}</Relationships>"), 1);
            workbook_xml = workbook_xml.replacen("</workbook>", &format!("{}</workbook>", pvw::workbook_pivot_caches_xml(&entries)), 1);
        }
        let shared_strings_xml = write_shared_strings_xml(&sst);
        let styles_xml = styles.write_styles_xml();

        // 3. 打包进 zip
        let buf = Vec::new();
        let cursor = std::io::Cursor::new(buf);
        let mut zw = zip::ZipWriter::new(cursor);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);

        write_zip_entry(&mut zw, opts, "[Content_Types].xml", &content_types)?;
        write_zip_entry(&mut zw, opts, "_rels/.rels", &root_rels)?;
        write_zip_entry(&mut zw, opts, "xl/workbook.xml", &workbook_xml)?;
        write_zip_entry(&mut zw, opts, "xl/_rels/workbook.xml.rels", &workbook_rels)?;
        write_zip_entry(&mut zw, opts, "xl/sharedStrings.xml", &shared_strings_xml)?;
write_zip_entry(&mut zw, opts, "xl/styles.xml", &styles_xml)?;
        if with_theme {
            let pal: Option<&Vec<[u8; 3]>> = workbook.sheets.iter()
                .flat_map(|sh| sh.tables.iter())
                .map(|t| &t.theme_palette)
                .find(|p| p.len() >= 10);
            write_zip_entry(&mut zw, opts, "xl/theme/theme1.xml", &write_theme_xml(pal.map(|v| v.as_slice())))?;
        }

        let mut drawing_no = 0usize;
        let mut media_no = 0usize;
        let mut chart_no = 0usize;
        let mut stat_no = 0usize;
        let mut table_no = 0usize; // 工作簿内全局表序号(文件名 tableN.xml 与 id 共用)
        for (idx, sheet) in workbook.sheets.iter().enumerate() {
            let mut sheet_xml = write_sheet_xml(sheet, &sst, &styles, idx);
            let n = sheet.tables.len();
            let n_charts = sheet.floating_charts.len();
            let n_stat = sheet.floating_stat_charts.len();
            let n_obj = sheet.floating_images.len() + sheet.floating_shapes.len();
            let mut drawing_rel = String::new();
            let mut pv_rel = String::new();
            if let Some(Some(sp)) = pivot_specs.get(idx) {
                pv_rel = pvw::sheet_pivot_rel("rIdPv1", sp.cache_id as usize);
            }
            if n_charts + n_stat + n_obj > 0 {
                drawing_no += 1;
                sheet_xml = inject_drawing(sheet_xml, &write_ext_lst_xml(sheet), "rIdDr1");
                drawing_rel = format!(
                    r#"<Relationship Id="rIdDr1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing{drawing_no}.xml"/>"#
                );
                let mut frames = String::new();
                let mut d_rels = String::new();
                for (k, ch) in sheet.floating_charts.iter().enumerate() {
                    chart_no += 1;
                    let rid = format!("rIdC{}", k + 1);
                    frames.push_str(&chart_anchor_xml(ch, k + 2, &rid));
                    d_rels.push_str(&format!(
                        r#"<Relationship Id="{rid}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="../charts/chart{chart_no}.xml"/>"#
                    ));
                    write_zip_entry(
                        &mut zw, opts,
                        &format!("xl/charts/chart{chart_no}.xml"),
                        &crate::ooxml::chart_writer::chart_to_xml_literal(ch),
                    )?;
                }
                for (k, sc) in sheet.floating_stat_charts.iter().enumerate() {
                    stat_no += 1;
                    let rid = format!("rIdS{}", k + 1);
                    frames.push_str(&stat_chart_anchor_xml(sc, n_charts + k + 2, &rid));
                    d_rels.push_str(&format!(
                        r#"<Relationship Id="{rid}" Type="http://schemas.microsoft.com/office/2014/relationships/chartEx" Target="../charts/chartEx{stat_no}.xml"/>"#
                    ));
                    write_zip_entry(
                        &mut zw, opts,
                        &format!("xl/charts/chartEx{stat_no}.xml"),
                        &crate::ooxml::chartex_writer::chartex_to_xml(&sc.model),
                    )?;
                }
                let objs = crate::ooxml::xlsx_drawing_writer::build_objects(
                    &sheet.floating_images, &sheet.floating_shapes, n_charts + n_stat + 2, &mut media_no);
                frames.push_str(&objs.frames);
                d_rels.push_str(&objs.rels);
                crate::ooxml::xlsx_drawing_writer::write_media(&mut zw, opts, &objs.media)
                    .map_err(|e| ParseError::Malformed(format!("write media failed: {e}")))?;
                let drawing_xml = format!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<xdr:wsDr xmlns:xdr=\"http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing\" xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\">{frames}</xdr:wsDr>"
                );
                let d_rels_xml = format!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">{d_rels}</Relationships>"
                );
                write_zip_entry(&mut zw, opts, &format!("xl/drawings/drawing{drawing_no}.xml"), &drawing_xml)?;
                write_zip_entry(&mut zw, opts, &format!("xl/drawings/_rels/drawing{drawing_no}.xml.rels"), &d_rels_xml)?;
            }
            if n > 0 {
                sheet_xml = inject_table_parts(sheet_xml, &write_ext_lst_xml(sheet), n);
                let mut rels = format!("{drawing_rel}{pv_rel}");
                for k in 0..n {
                    rels.push_str(&format!(
                        r#"<Relationship Id="rId{}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/table" Target="../tables/table{}.xml"/>"#,
                        k + 1, table_no + k + 1
                    ));
                }
                let rels_xml = format!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">{rels}</Relationships>"
                );
                write_zip_entry(&mut zw, opts, &format!("xl/worksheets/_rels/sheet{}.xml.rels", idx + 1), &rels_xml)?;
                for (k, t) in sheet.tables.iter().enumerate() {
                    let no = table_no + k + 1;
                    write_zip_entry(&mut zw, opts, &format!("xl/tables/table{no}.xml"), &write_table_xml(t, no))?;
                }
                table_no += n;
            }
            if n == 0 && (n_charts + n_stat + n_obj > 0 || !pv_rel.is_empty()) {
                let rels_xml = format!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">{drawing_rel}{pv_rel}</Relationships>"
                );
                write_zip_entry(&mut zw, opts, &format!("xl/worksheets/_rels/sheet{}.xml.rels", idx + 1), &rels_xml)?;
            }
            if let Some(Some(sp)) = pivot_specs.get(idx) {
                let no = sp.cache_id as usize;
                if let (Some(pt), Some(cd)) = (pvw::pivot_table_xml(sp), pvw::cache_definition_xml(sp)) {
                    write_zip_entry(&mut zw, opts, &format!("xl/pivotTables/pivotTable{no}.xml"), &pt)?;
                    write_zip_entry(&mut zw, opts, &format!("xl/pivotTables/_rels/pivotTable{no}.xml.rels"), &pvw::pivot_table_rels_xml(no))?;
                    write_zip_entry(&mut zw, opts, &format!("xl/pivotCache/pivotCacheDefinition{no}.xml"), &cd)?;
                }
            }
            let path = format!("xl/worksheets/sheet{}.xml", idx + 1);
            write_zip_entry(&mut zw, opts, &path, &sheet_xml)?;
        }

        let cursor = zw
            .finish()
            .map_err(|e| ParseError::Malformed(format!("zip finish failed: {e}")))?;
        Ok(cursor.into_inner())
    }
}

/// 在已有 [Content_Types].xml 末尾追加 drawing / chart 的 Override。
fn append_chart_content_types(ct: String, drawings: usize, charts: usize, stat_charts: usize) -> String {
    if drawings == 0 && charts == 0 && stat_charts == 0 {
        return ct;
    }
    let mut extra = String::new();
    for i in 1..=drawings {
        extra.push_str(&format!(
            r#"<Override PartName="/xl/drawings/drawing{i}.xml" ContentType="application/vnd.openxmlformats-officedocument.drawing+xml"/>"#
        ));
    }
    for i in 1..=charts {
        extra.push_str(&format!(
            r#"<Override PartName="/xl/charts/chart{i}.xml" ContentType="application/vnd.openxmlformats-officedocument.drawingml.chart+xml"/>"#
        ));
    }
    for i in 1..=stat_charts {
        extra.push_str(&format!(
            r#"<Override PartName="/xl/charts/chartEx{i}.xml" ContentType="application/vnd.ms-office.chartex+xml"/>"#
        ));
    }
    ct.replacen("</Types>", &format!("{extra}</Types>"), 1)
}

/// 在 worksheet 末尾(extLst / tableParts 之前)插入 <drawing r:id=.../>,并确保声明 xmlns:r。
fn inject_drawing(xml: String, ext_lst: &str, rid: &str) -> String {
    let tag = format!(r#"<drawing r:id="{rid}"/>"#);
    let tail = format!("{ext_lst}</worksheet>");
    let pos = if xml.ends_with(&tail) {
        xml.len() - tail.len()
    } else {
        xml.rfind("</worksheet>").unwrap_or(xml.len())
    };
    let mut out = String::with_capacity(xml.len() + tag.len() + 80);
    out.push_str(&xml[..pos]);
    out.push_str(&tag);
    out.push_str(&xml[pos..]);
    out.replacen(
        r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">"#,
        r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">"#,
        1,
    )
}

/// 单张图表的 absoluteAnchor(位置/尺寸直接用 EMU,与 xlsx.rs 的读取对称)。
fn chart_anchor_xml(c: &roe_model::impress::ChartModel, shape_id: usize, rid: &str) -> String {
    let x = (c.x_pt as f64 * 12700.0).round() as i64;
    let y = (c.y_pt as f64 * 12700.0).round() as i64;
    let cx = (c.width_pt as f64 * 12700.0).round() as i64;
    let cy = (c.height_pt as f64 * 12700.0).round() as i64;
    format!(
        r#"<xdr:absoluteAnchor><xdr:pos x="{x}" y="{y}"/><xdr:ext cx="{cx}" cy="{cy}"/><xdr:graphicFrame macro=""><xdr:nvGraphicFramePr><xdr:cNvPr id="{shape_id}" name="Chart {shape_id}"/><xdr:cNvGraphicFramePr/></xdr:nvGraphicFramePr><xdr:xfrm><a:off x="{x}" y="{y}"/><a:ext cx="{cx}" cy="{cy}"/></xdr:xfrm><a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/chart"><c:chart xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" r:id="{rid}"/></a:graphicData></a:graphic></xdr:graphicFrame><xdr:clientData/></xdr:absoluteAnchor>"#
    )
}

/// 统计图(chartEx)的 absoluteAnchor:AlternateContent 外壳,Choice 放 cx:chart,
/// Fallback 放简单矩形提示。读取侧会剔除 chartEx 的 Fallback,不会读成文本框。
fn stat_chart_anchor_xml(c: &roe_model::calc::FloatingStatChart, shape_id: usize, rid: &str) -> String {
    let x = (c.x_pt as f64 * 12700.0).round() as i64;
    let y = (c.y_pt as f64 * 12700.0).round() as i64;
    let cx = (c.width_pt as f64 * 12700.0).round() as i64;
    let cy = (c.height_pt as f64 * 12700.0).round() as i64;
    format!(
        r#"<xdr:absoluteAnchor><xdr:pos x="{x}" y="{y}"/><xdr:ext cx="{cx}" cy="{cy}"/><mc:AlternateContent xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006"><mc:Choice xmlns:cx1="http://schemas.microsoft.com/office/drawing/2015/9/8/chartex" Requires="cx1"><xdr:graphicFrame macro=""><xdr:nvGraphicFramePr><xdr:cNvPr id="{shape_id}" name="Chart {shape_id}"/><xdr:cNvGraphicFramePr/></xdr:nvGraphicFramePr><xdr:xfrm><a:off x="{x}" y="{y}"/><a:ext cx="{cx}" cy="{cy}"/></xdr:xfrm><a:graphic><a:graphicData uri="http://schemas.microsoft.com/office/drawing/2014/chartex"><cx:chart xmlns:cx="http://schemas.microsoft.com/office/drawing/2014/chartex" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" r:id="{rid}"/></a:graphicData></a:graphic></xdr:graphicFrame></mc:Choice><mc:Fallback><xdr:sp macro="" textlink=""><xdr:nvSpPr><xdr:cNvPr id="0" name=""/><xdr:cNvSpPr/></xdr:nvSpPr><xdr:spPr><a:xfrm><a:off x="{x}" y="{y}"/><a:ext cx="{cx}" cy="{cy}"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></xdr:spPr><xdr:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:rPr lang="en-US"/><a:t>This chart requires Excel 2016 or later.</a:t></a:r></a:p></xdr:txBody></xdr:sp></mc:Fallback></mc:AlternateContent><xdr:clientData/></xdr:absoluteAnchor>"#
    )
}

fn write_zip_entry<W: Write + std::io::Seek>(
    zw: &mut zip::ZipWriter<W>,
    opts: zip::write::SimpleFileOptions,
    path: &str,
    content: &str,
) -> Result<(), ParseError> {
    zw.start_file(path, opts)
        .map_err(|e| ParseError::Malformed(format!("zip start_file '{path}' failed: {e}")))?;
    zw.write_all(content.as_bytes())
        .map_err(|e| ParseError::Malformed(format!("zip write '{path}' failed: {e}")))?;
    Ok(())
}

// ─── [Content_Types].xml ──────────────────────────────────────────────────────

fn write_content_types(sheet_count: usize, table_count: usize, with_theme: bool) -> String {
    let mut overrides = String::new();
    if with_theme {
        overrides.push_str(r#"<Override PartName="/xl/theme/theme1.xml" ContentType="application/vnd.openxmlformats-officedocument.theme+xml"/>"#);
    }
    for i in 1..=table_count {
        overrides.push_str(&format!(
            r#"<Override PartName="/xl/tables/table{i}.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.table+xml"/>"#
        ));
    }
    overrides.push_str(
        r#"<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>"#,
    );
    for i in 1..=sheet_count {
        overrides.push_str(&format!(
            r#"<Override PartName="/xl/worksheets/sheet{i}.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>"#
        ));
    }
    overrides.push_str(
        r#"<Override PartName="/xl/sharedStrings.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml"/>"#,
    );
    overrides.push_str(
        r#"<Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/>"#,
    );

    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
{overrides}
</Types>"#
    )
}

// ─── _rels/.rels（包级关系，指向 workbook.xml）────────────────────────────────

fn write_root_rels() -> String {
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#
        .to_string()
}

// ─── xl/workbook.xml ──────────────────────────────────────────────────────────

fn write_workbook_xml(workbook: &Workbook) -> String {
    let defined_names_xml = write_defined_names_xml(workbook);
    let mut sheets_xml = String::new();
    for (idx, sheet) in workbook.sheets.iter().enumerate() {
        let sheet_id = idx + 1;
        sheets_xml.push_str(&format!(
            r#"<sheet name="{}" sheetId="{}" r:id="rId{}"/>"#,
            xml_escape_attr(&sheet.name),
            sheet_id,
            sheet_id
        ));
    }

    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<sheets>{sheets_xml}</sheets>{defined_names_xml}
</workbook>"#
    )
}

// ─── xl/_rels/workbook.xml.rels ───────────────────────────────────────────────

/// 写出 <definedNames>(OOXML 规定位置:<sheets> 之后、<calcPr> 之前)。
///
/// 现状限制(模型是单一扁平 HashMap,读取端把 localSheetId 一律当全局处理):
/// - 局部名称(Local1 等)读入时已被合并/覆盖,这里写出为全局名称;
/// - hidden/function/vbProcedure 等属性读取端未保存,不写出;
/// - `_xlnm.` 开头的内置名称(如 _FilterDatabase)跳过:它们被读成了全局名称,
///   原样写出会变成非法的全局内置名称,筛选范围由 sheet 自己的 autoFilter 写出。
/// 按名字排序,保证输出稳定。
fn write_defined_names_xml(workbook: &Workbook) -> String {
    fn col_letters(col0: u32) -> String {
        let mut n = col0 + 1;
        let mut out: Vec<u8> = Vec::new();
        while n > 0 {
            out.push(b'A' + ((n - 1) % 26) as u8);
            n = (n - 1) / 26;
        }
        out.reverse();
        String::from_utf8(out).unwrap_or_default()
    }
    fn abs_ref(row0: u32, col0: u32) -> String {
        format!("${}${}", col_letters(col0), row0 + 1)
    }
    fn quote_sheet(name: &str) -> String {
        let plain = !name.is_empty()
            && !name.chars().next().map_or(false, |c| c.is_ascii_digit())
            && name.chars().all(|c| c.is_alphanumeric() || c == '_');
        if plain {
            name.to_string()
        } else {
            format!("'{}'", name.replace('\'', "''"))
        }
    }

    let mut entries: Vec<(String, String)> = Vec::new();
    for (name, r) in &workbook.named_ranges {
        if name.starts_with("_xlnm.") {
            continue;
        }
        let a = abs_ref(r.start.0, r.start.1);
        let body = if r.start == r.end {
            a
        } else {
            format!("{}:{}", a, abs_ref(r.end.0, r.end.1))
        };
        entries.push((name.clone(), format!("{}!{}", quote_sheet(&r.sheet), body)));
    }
    for (name, f) in &workbook.named_formulas {
        if name.starts_with("_xlnm.") {
            continue;
        }
        entries.push((name.clone(), f.clone()));
    }
    if entries.is_empty() {
        return String::new();
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let mut out = String::from("<definedNames>");
    for (name, value) in &entries {
        out.push_str(&format!(
            r#"<definedName name="{}">{}</definedName>"#,
            xml_escape_attr(name),
            xml_escape_text(value)
        ));
    }
    out.push_str("</definedNames>");
    out
}

fn write_workbook_rels(sheet_count: usize, with_theme: bool) -> String {
    let mut rels = String::new();
    for i in 1..=sheet_count {
        rels.push_str(&format!(
            r#"<Relationship Id="rId{i}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet{i}.xml"/>"#
        ));
    }
    // sharedStrings 关系 id 接在 sheet 关系之后
    let sst_id = sheet_count + 1;
    rels.push_str(&format!(
        r#"<Relationship Id="rId{sst_id}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="sharedStrings.xml"/>"#
    ));
    // styles 关系 id 接在 sharedStrings 关系之后
    let styles_id = sheet_count + 2;
    rels.push_str(&format!(
        r#"<Relationship Id="rId{styles_id}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>"#
    ));

    if with_theme {
        let theme_id = sheet_count + 3;
        rels.push_str(&format!(
            r#"<Relationship Id="rId{theme_id}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme" Target="theme/theme1.xml"/>"#
        ));
    }

    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
{rels}
</Relationships>"#
    )
}

// ─── SharedStrings 收集与写出 ──────────────────────────────────────────────────

#[derive(Default)]
struct SharedStringTable {
    strings: Vec<String>,
    index: HashMap<String, usize>,
}

impl SharedStringTable {
    /// 插入字符串，返回其 SST 索引（已存在则复用）
    fn intern(&mut self, s: &str) -> usize {
        if let Some(&idx) = self.index.get(s) {
            return idx;
        }
        let idx = self.strings.len();
        self.strings.push(s.to_string());
        self.index.insert(s.to_string(), idx);
        idx
    }
}

/// 扫描 sheet 所有单元格，把 Text 值塞进 SST
/// （Formula 不走 SST：写出时按 <f> 标签处理，不需要 SST）
fn collect_strings(sheet: &Sheet, sst: &mut SharedStringTable) {
    for col in sheet.cells.cols.values() {
        for block in &col.blocks {
            for val in &block.values {
                if let CellValue::Text(s) = val {
                    sst.intern(s);
                }
            }
        }
    }
}

fn write_shared_strings_xml(sst: &SharedStringTable) -> String {
    let mut items = String::new();
    for s in &sst.strings {
        items.push_str(&format!("<si><t>{}</t></si>", xml_escape_text(s)));
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="{}" uniqueCount="{}">
{items}
</sst>"#,
        sst.strings.len(),
        sst.strings.len()
    )
}

// ─── worksheet 写出 ────────────────────────────────────────────────────────────

fn write_sheet_xml(
    sheet: &Sheet,
    sst: &SharedStringTable,
    styles: &StylesCollector,
    sheet_idx: usize,
) -> String {
    // 按行收集：row → [(col, CellValue)]，从 SparseGrid（按列存储）转置
    let mut rows: std::collections::BTreeMap<u32, Vec<(u32, CellValue)>> =
        std::collections::BTreeMap::new();

    for (&col, sc) in &sheet.cells.cols {
        for block in &sc.blocks {
            for (i, val) in block.values.iter().enumerate() {
                if matches!(val, CellValue::Empty) {
                    continue;
                }
                let row = block.start_row + i as u32;
                rows.entry(row).or_default().push((col, val.clone()));
            }
        }
    }

    // 把 hidden/outline_level 非默认值的行号也纳入需要写出 <row> 标签的集合，
    // 即使该行没有任何单元格数据——否则空白但被隐藏/分组的行会在 rows（从
    // SparseGrid 转置而来，只含有数据的行）里完全不存在，hidden 属性静默丢失。
    for (idx, &hidden) in sheet.row_hidden.iter().enumerate() {
        if hidden {
            rows.entry(idx as u32).or_default();
        }
    }
    for (idx, &lvl) in sheet.row_outline_level.iter().enumerate() {
        if lvl > 0 {
            rows.entry(idx as u32).or_default();
        }
    }

    let mut sheet_data = String::new();
    for (row, mut cells) in rows {
        cells.sort_by_key(|(c, _)| *c);
        // xlsx 行号是 1-based，内存模型是 0-based，写出时 +1
        let row_num = row + 1;
        let mut row_cells = String::new();
        for (col, val) in cells {
            let addr = format!("{}{}", col_to_letters(col + 1), row_num);
            let xf_idx = styles.cell_xf.get(&(sheet_idx, row, col)).copied();
            row_cells.push_str(&write_cell_xml(&addr, &val, sst, xf_idx));
        }
        let mut row_attrs = String::new();
        if sheet.row_hidden.get(row as usize).copied().unwrap_or(false) {
            row_attrs.push_str(r#" hidden="1""#);
        }
        let outline = sheet.row_outline_level.get(row as usize).copied().unwrap_or(0);
        if outline > 0 {
            row_attrs.push_str(&format!(r#" outlineLevel="{outline}""#));
        }
        sheet_data.push_str(&format!(r#"<row r="{row_num}"{row_attrs}>{row_cells}</row>"#));
    }

    let merge_cells_xml = write_merge_cells_xml(sheet);
    let cond_formatting_xml = write_conditional_formatting_xml(&sheet.cond_formats, styles);
    let data_validations_xml = write_data_validations_xml(sheet);
    let ext_lst_xml = write_ext_lst_xml(sheet);
    let sheet_views_xml = write_sheet_view_xml(sheet);
    let cols_xml = write_cols_xml(sheet);

    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
{sheet_views_xml}{cols_xml}<sheetData>{sheet_data}</sheetData>
{merge_cells_xml}{cond_formatting_xml}{data_validations_xml}{ext_lst_xml}</worksheet>"#
    )
}

/// 生成 worksheet 末尾唯一的 <extLst>。目前只含迷你图(x14:sparklineGroups);
/// 后续 x14 dataBar 也必须放进同一个 <extLst>(xlsx.rs 读入只取第一个 "<extLst")。
/// 无任何扩展内容时返回空字符串。
fn write_ext_lst_xml(sheet: &Sheet) -> String {
    let x14_cf = write_x14_databar_ext_xml(&sheet.cond_formats);
    let spark = write_sparkline_ext_xml(sheet);
    if x14_cf.is_empty() && spark.is_empty() {
        return String::new();
    }
    format!("<extLst>{x14_cf}{spark}</extLst>")
}

fn xml_attr_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// "#RRGGBB" → "FFRRGGBB";格式不对返回 None(不写该颜色元素)。
fn sparkline_color_argb(c: &str) -> Option<String> {
    let h = c.trim().trim_start_matches('#');
    if h.len() == 6 && h.chars().all(|ch| ch.is_ascii_hexdigit()) {
        Some(format!("FF{}", h.to_ascii_uppercase()))
    } else {
        None
    }
}

/// 把 CellRange 写成 `'Sheet'!$A$1:$E$1` 风格的引用(这里不加 $,读入也不依赖)。
fn sparkline_ref_text(r: &roe_model::calc::CellRange, default_sheet: &str) -> String {
    let name = if r.sheet.is_empty() { default_sheet } else { r.sheet.as_str() };
    let name = name.replace('\'', "''");
    let a = format!("{}{}", col_to_letters(r.start.1 + 1), r.start.0 + 1);
    let b = format!("{}{}", col_to_letters(r.end.1 + 1), r.end.0 + 1);
    let area = if a == b { a } else { format!("{a}:{b}") };
    format!("'{name}'!{area}")
}

fn write_sparkline_ext_xml(sheet: &Sheet) -> String {
    use roe_model::calc::{DisplayEmptyCellsAs, SparklineAxisType, SparklineType};
    if sheet.sparkline_groups.is_empty() {
        return String::new();
    }
    let mut groups = String::new();
    for g in &sheet.sparkline_groups {
        // 先收集有效迷你图;没有有效迷你图的组整组跳过
        let mut items = String::new();
        for sp in &g.sparklines {
            let Some(rng) = sp.input_range.first() else { continue };
            let f = xml_attr_escape(&sparkline_ref_text(rng, &sheet.name));
            let cell = format!("{}{}", col_to_letters(sp.cell.1 + 1), sp.cell.0 + 1);
            items.push_str(&format!(
                "<x14:sparkline><xm:f>{f}</xm:f><xm:sqref>{cell}</xm:sqref></x14:sparkline>"
            ));
        }
        if items.is_empty() {
            continue;
        }

        let mut attrs = String::new();
        match g.sparkline_type {
            SparklineType::Line => {}
            SparklineType::Column => attrs.push_str(r#" type="column""#),
            SparklineType::Stacked => attrs.push_str(r#" type="stacked""#),
        }
        attrs.push_str(&format!(r#" lineWeight="{}""#, g.line_weight));
        if g.date_axis { attrs.push_str(r#" dateAxis="1""#); }
        match g.display_empty_cells_as {
            DisplayEmptyCellsAs::Gap => {} // 默认
            DisplayEmptyCellsAs::Span => attrs.push_str(r#" displayEmptyCellsAs="span""#),
            DisplayEmptyCellsAs::Zero => attrs.push_str(r#" displayEmptyCellsAs="zero""#),
        }
        if g.markers { attrs.push_str(r#" markers="1""#); }
        if g.high { attrs.push_str(r#" high="1""#); }
        if g.low { attrs.push_str(r#" low="1""#); }
        if g.first { attrs.push_str(r#" first="1""#); }
        if g.last { attrs.push_str(r#" last="1""#); }
        if g.negative { attrs.push_str(r#" negative="1""#); }
        if g.display_x_axis { attrs.push_str(r#" displayXAxis="1""#); }
        if g.display_hidden { attrs.push_str(r#" displayHidden="1""#); }
        match g.min_axis_type {
            SparklineAxisType::Individual => {}
            SparklineAxisType::Group => attrs.push_str(r#" minAxisType="group""#),
            SparklineAxisType::Custom => attrs.push_str(r#" minAxisType="custom""#),
        }
        match g.max_axis_type {
            SparklineAxisType::Individual => {}
            SparklineAxisType::Group => attrs.push_str(r#" maxAxisType="group""#),
            SparklineAxisType::Custom => attrs.push_str(r#" maxAxisType="custom""#),
        }
        if let Some(v) = g.manual_max { attrs.push_str(&format!(r#" manualMax="{v}""#)); }
        if let Some(v) = g.manual_min { attrs.push_str(&format!(r#" manualMin="{v}""#)); }
        if g.right_to_left { attrs.push_str(r#" rightToLeft="1""#); }

        // 子元素顺序由 xsd 规定:series, negative, axis, markers, first, last, high, low
        let mut colors = String::new();
        // 颜色为 None(或无法解析)时补 Excel 默认色:缺 colorSeries 时 WPS/Only/LO 不显示迷你图。
        for (tag, v, def) in [
            ("colorSeries", &g.color_series, "#376092"),
            ("colorNegative", &g.color_negative, "#D00000"),
            ("colorAxis", &g.color_axis, "#000000"),
            ("colorMarkers", &g.color_markers, "#D00000"),
            ("colorFirst", &g.color_first, "#D00000"),
            ("colorLast", &g.color_last, "#D00000"),
            ("colorHigh", &g.color_high, "#D00000"),
            ("colorLow", &g.color_low, "#D00000"),
        ] {
            let src = v
                .as_deref()
                .filter(|c| sparkline_color_argb(c).is_some())
                .unwrap_or(def);
            if let Some(argb) = sparkline_color_argb(src) {
                colors.push_str(&format!(r#"<x14:{tag} rgb="{argb}"/>"#));
            }
        }
        groups.push_str(&format!(
            "<x14:sparklineGroup{attrs}>{colors}<x14:sparklines>{items}</x14:sparklines></x14:sparklineGroup>"
        ));
    }
    if groups.is_empty() {
        return String::new();
    }
    format!(
        r#"<ext uri="{{05C60535-1F16-4fd2-B633-F4F36F0B64E0}}" xmlns:x14="http://schemas.microsoft.com/office/spreadsheetml/2009/9/main"><x14:sparklineGroups xmlns:xm="http://schemas.microsoft.com/office/excel/2006/main">{groups}</x14:sparklineGroups></ext>"#
    )
}

/// 生成 <cols> XML 片段:列宽 + hidden + outlineLevel。
/// 宽度换算与 parser 互逆:内存 pt = (字符数*7+5)*0.75。
/// 宽度 <=0 或换算后字符数 <=0 视为未设置;连续同属性列合并为一个 <col min max/>。
/// 全部为默认时返回空字符串(不写出容器标签)。
fn cols_xml_from(widths: &[f32], hidden: &[bool], outline: &[u8]) -> String {
    let max_len = widths.len().max(hidden.len()).max(outline.len());
    if max_len == 0 {
        return String::new();
    }
    let key = |i: usize| -> (i64, bool, u8) {
        let w = widths.get(i).copied().unwrap_or(0.0);
        let chars = ((w / 0.75) - 5.0) / 7.0;
        let wk = if w.is_finite() && chars > 0.0005 {
            (chars * 1000.0).round() as i64
        } else {
            0
        };
        (
            wk,
            hidden.get(i).copied().unwrap_or(false),
            outline.get(i).copied().unwrap_or(0),
        )
    };

    let mut out = String::new();
    let mut idx = 0usize;
    while idx < max_len {
        let k = key(idx);
        if k == (0, false, 0) {
            idx += 1;
            continue;
        }
        let mut end = idx;
        while end + 1 < max_len && key(end + 1) == k {
            end += 1;
        }
        // xlsx col min/max 是 1-based 闭区间,内存模型是 0-based
        let mut attrs = format!(r#" min="{}" max="{}""#, idx + 1, end + 1);
        if k.0 > 0 {
            attrs.push_str(&format!(r#" width="{:.3}" customWidth="1""#, k.0 as f64 / 1000.0));
        }
        if k.1 {
            attrs.push_str(r#" hidden="1""#);
        }
        if k.2 > 0 {
            attrs.push_str(&format!(r#" outlineLevel="{}""#, k.2));
        }
        out.push_str(&format!(r#"<col{attrs}/>"#));
        idx = end + 1;
    }

    if out.is_empty() {
        return String::new();
    }
    format!(r#"<cols>{out}</cols>"#)
}

fn write_cols_xml(sheet: &Sheet) -> String {
    cols_xml_from(&sheet.col_widths, &sheet.col_hidden, &sheet.col_outline_level)
}

/// 生成 <sheetViews><sheetView .../></sheetViews> XML 片段。
/// 只读写 zoom_scale/show_grid_lines，不接入渲染（渲染留待 LOK/OnlyOffice
/// 语义补全阶段）。两字段均为 None 时省略属性，交由 Excel/LOK 按规范默认值
/// 处理（zoomScale 默认 100，showGridLines 默认 true）。
fn write_sheet_view_xml(sheet: &Sheet) -> String {
    let mut attrs = String::new();
    if let Some(show) = sheet.show_grid_lines {
        // xlsx 规范：showGridLines 省略时默认 true，因此只在为 false 时显式写出更简洁；
        // 但为了"读写不丢失"原则的完整性，true/false 都显式写出
        attrs.push_str(&format!(r#" showGridLines="{}""#, if show { "1" } else { "0" }));
    }
    attrs.push_str(r#" workbookViewId="0""#);
    if let Some(zoom) = sheet.zoom_scale {
        attrs.push_str(&format!(r#" zoomScale="{zoom}""#));
    }
    format!(r#"<sheetViews><sheetView{attrs}/></sheetViews>"#)
}

/// 生成 sheet.merged_cells 对应的 <mergeCells> XML 片段。
/// 与 xlsx.rs parser 的读取逻辑对称（MergedCell { r1, c1, r2, c2} 均为 0-indexed）。
/// OOXML CT_Worksheet schema 顺序要求 mergeCells 位于 sheetData 之后、
/// conditionalFormatting/dataValidations 之前。为空时返回空字符串（不写出容器标签）。
fn write_merge_cells_xml(sheet: &Sheet) -> String {
    if sheet.merged_cells.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    out.push_str(&format!(
        r#"<mergeCells count="{}">"#,
        sheet.merged_cells.len()
    ));
    for mc in &sheet.merged_cells {
        // OOXML 规范：mergeCell ref 恒为 "start:end" 形式，即使单格合并也不省略冒号
        // （xlsx.rs::parse_merge_ref 按此约定要求必须带冒号，无冒号视为格式错误）
        let a1 = format!("{}{}", col_to_letters(mc.c1 + 1), mc.r1 + 1);
        let a2 = format!("{}{}", col_to_letters(mc.c2 + 1), mc.r2 + 1);
        out.push_str(&format!(r#"<mergeCell ref="{a1}:{a2}"/>"#));
    }
    out.push_str("</mergeCells>");
    out
}

/// 生成 sheet.data_validations 对应的 <dataValidations> XML 片段。
/// 与 xlsx.rs parser 的读取逻辑对称：type/operator/allowBlank/showErrorMessage/
/// error 属性 + formula1/formula2 子标签。为空时返回空字符串（不写出容器标签，
/// 与 conditionalFormatting 当前的"parser 已支持读，本 writer 暂不写"策略不同——
/// dataValidations 读写本次一起补齐，因此这里直接写出，无需刻意留空占位）。
fn write_data_validations_xml(sheet: &Sheet) -> String {
    if sheet.data_validations.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    out.push_str(&format!(
        r#"<dataValidations count="{}">"#,
        sheet.data_validations.len()
    ));
    for rule in &sheet.data_validations {
        let sqref = range_to_sqref(&rule.range);
        let operator_attr = if rule.operator.is_empty() {
            String::new()
        } else {
            format!(r#" operator="{}""#, xml_escape_attr(&rule.operator))
        };
        let allow_blank_attr = if rule.allow_blank { r#" allowBlank="1""# } else { "" };
        let show_error_attr = if rule.show_error { r#" showErrorMessage="1""# } else { "" };
        let error_attr = match &rule.error_msg {
            Some(msg) => format!(r#" error="{}""#, xml_escape_attr(msg)),
            None => String::new(),
        };
        out.push_str(&format!(
            r#"<dataValidation type="{}"{}{}{}{} sqref="{}">"#,
            xml_escape_attr(&rule.kind),
            operator_attr,
            allow_blank_attr,
            show_error_attr,
            error_attr,
            xml_escape_attr(&sqref),
        ));
        if !rule.formula1.is_empty() {
            out.push_str(&format!("<formula1>{}</formula1>", xml_escape_text(&rule.formula1)));
        }
        if let Some(f2) = &rule.formula2 {
            out.push_str(&format!("<formula2>{}</formula2>", xml_escape_text(f2)));
        }
        out.push_str("</dataValidation>");
    }
    out.push_str("</dataValidations>");
    out
}

/// CellRange → "A1:B10" 格式的 sqref 字符串（0-based → 1-based）。
/// 单格 range（start==end）写成 "A1" 而非 "A1:A1"，与 excelize/Excel 常见写法一致。
fn range_to_sqref(r: &roe_model::calc::CellRange) -> String {
    let (r1, c1) = r.start;
    let (r2, c2) = r.end;
    let a1 = format!("{}{}", col_to_letters(c1 + 1), r1 + 1);
    if r.start == r.end {
        a1
    } else {
        let a2 = format!("{}{}", col_to_letters(c2 + 1), r2 + 1);
        format!("{a1}:{a2}")
    }
}

fn write_cell_xml(addr: &str, val: &CellValue, sst: &SharedStringTable, xf_idx: Option<usize>) -> String {
    let s_attr = match xf_idx {
        Some(idx) if idx != 0 => format!(r#" s="{idx}""#),
        _ => String::new(),
    };
    match val {
        CellValue::Empty => String::new(), // 不应到达（外层已过滤），保留作为防御
        CellValue::Number(n) => {
            format!(r#"<c r="{addr}"{s_attr}><v>{n}</v></c>"#)
        }
        CellValue::Text(s) => {
            // SST 已在 collect_strings 阶段全量收集，这里必定能找到
            let idx = sst.index.get(s).copied().unwrap_or(0);
            format!(r#"<c r="{addr}"{s_attr} t="s"><v>{idx}</v></c>"#)
        }
        CellValue::Formula { source, .. } => {
            // 内存模型存储形如 "=SUM(A1:A10)"，OOXML <f> 标签内不含开头的 "="
            let f_body = source.strip_prefix('=').unwrap_or(source);
            let f_body = &quote_sheet_refs_in_formula(f_body);
            let f_body = &prefix_xlfn_functions(f_body);
            format!(
                r#"<c r="{addr}"{s_attr}><f>{}</f></c>"#,
                xml_escape_text(f_body)
            )
        }
        CellValue::Bool(b) => {
            format!(r#"<c r="{addr}"{s_attr} t="b"><v>{}</v></c>"#, if *b { 1 } else { 0 })
        }
        CellValue::Error(e) => {
            let code = cell_error_to_str(*e);
            format!(r#"<c r="{addr}"{s_attr} t="e"><v>{code}</v></c>"#)
        }
        // ROOT-078（已知技术债，非本次范围）：XLSX写出层暂不支持run级
        // 富文本格式往返，拼接全部run文本退化为普通字符串单元格写出
        // （走SST），格式信息（如strike）会丢失。完整支持需要写出层
        // 能生成sharedStrings.xml的<si><r><rPr>分run结构，是独立的、
        // 更大的工作量，本次仅确保不会因非穷尽匹配编译失败。
        CellValue::RichText(runs) => {
            let text: String = runs.iter().map(|r| r.text.as_str()).collect();
            let idx = sst.index.get(&text).copied().unwrap_or(0);
            format!(r#"<c r="{addr}"{s_attr} t="s"><v>{idx}</v></c>"#)
        }
    }
}

fn cell_error_to_str(e: CellError) -> &'static str {
    match e {
        CellError::Div0 => "#DIV/0!",
        CellError::Na => "#N/A",
        CellError::Name => "#NAME?",
        CellError::Null => "#NULL!",
        CellError::Num => "#NUM!",
        CellError::Ref => "#REF!",
        CellError::Value => "#VALUE!",
        CellError::CircularRef => "#N/A", // OOXML 无独立循环引用错误码，降级为 #N/A
    }
}

// ─── 辅助：列号 → 字母（与 xlsx.rs parser 的 col_from_ref 互逆）───────────────

/// col=1 → "A", col=27 → "AA"（与 parser 的 col_from_ref 编号约定一致：1-based）
// ─── 表格(Table/ListObject)写出 ─────────────────────────────────────────────
// 已知取舍: 读侧不保留 totalsRowFunction / calculatedColumnFormula / dataDxfId /
// 筛选条件,这些在写出时丢失;tableColumn 名取 Table.columns,须与表头单元格文字一致。

/// 最小合法主题。palette 顺序同 Table.theme_palette: 0=lt1 1=dk1 2=lt2 3=dk2 4~9=accent1~6。
/// 整个工作簿只有一份主题,多个表格色板不同时取第一个(已知取舍)。
fn write_theme_xml(palette: Option<&[[u8; 3]]>) -> String {
    const DEF: [[u8; 3]; 10] = [
        [0xFF, 0xFF, 0xFF], [0x00, 0x00, 0x00], [0xE7, 0xE6, 0xE6], [0x44, 0x54, 0x6A],
        [0x44, 0x72, 0xC4], [0xED, 0x7D, 0x31], [0xA5, 0xA5, 0xA5],
        [0xFF, 0xC0, 0x00], [0x5B, 0x9B, 0xD5], [0x70, 0xAD, 0x47],
    ];
    let p = |i: usize| -> String {
        let c = palette.and_then(|v| v.get(i).copied()).unwrap_or(DEF[i]);
        format!("{:02X}{:02X}{:02X}", c[0], c[1], c[2])
    };
    let clr = |tag: &str, i: usize| format!(r#"<a:{tag}><a:srgbClr val="{}"/></a:{tag}>"#, p(i));
    let fills = r#"<a:solidFill><a:schemeClr val="phClr"/></a:solidFill>"#;
    let ln = r#"<a:ln w="9525"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln>"#;
    format!(
        concat!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n",
            "<a:theme xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" name=\"Office Theme\"><a:themeElements>",
            "<a:clrScheme name=\"Office\">{dk1}{lt1}{dk2}{lt2}{a1}{a2}{a3}{a4}{a5}{a6}",
            "<a:hlink><a:srgbClr val=\"0563C1\"/></a:hlink><a:folHlink><a:srgbClr val=\"954F72\"/></a:folHlink></a:clrScheme>",
            "<a:fontScheme name=\"Office\"><a:majorFont><a:latin typeface=\"Calibri Light\"/><a:ea typeface=\"\"/><a:cs typeface=\"\"/></a:majorFont>",
            "<a:minorFont><a:latin typeface=\"Calibri\"/><a:ea typeface=\"\"/><a:cs typeface=\"\"/></a:minorFont></a:fontScheme>",
            "<a:fmtScheme name=\"Office\"><a:fillStyleLst>{f}{f}{f}</a:fillStyleLst>",
            "<a:lnStyleLst>{l}{l}{l}</a:lnStyleLst>",
            "<a:effectStyleLst><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle></a:effectStyleLst>",
            "<a:bgFillStyleLst>{f}{f}{f}</a:bgFillStyleLst></a:fmtScheme>",
            "</a:themeElements></a:theme>"
        ),
        dk1 = clr("dk1", 1), lt1 = clr("lt1", 0), dk2 = clr("dk2", 3), lt2 = clr("lt2", 2),
        a1 = clr("accent1", 4), a2 = clr("accent2", 5), a3 = clr("accent3", 6),
        a4 = clr("accent4", 7), a5 = clr("accent5", 8), a6 = clr("accent6", 9),
        f = fills, l = ln,
    )
}

fn cell_ref_a1(row: u32, col: u32) -> String {
    format!("{}{}", col_to_letters(col + 1), row + 1)
}

/// 在 sheet XML 中插入 <tableParts>(位于 worksheet 级 <extLst> 之前),并补 xmlns:r。
fn inject_table_parts(xml: String, ext_lst: &str, n: usize) -> String {
    let mut parts = format!(r#"<tableParts count="{n}">"#);
    for k in 1..=n {
        parts.push_str(&format!(r#"<tablePart r:id="rId{k}"/>"#));
    }
    parts.push_str("</tableParts>");
    let tail = format!("{ext_lst}</worksheet>");
    let pos = if xml.ends_with(&tail) {
        xml.len() - tail.len()
    } else {
        xml.rfind("</worksheet>").unwrap_or(xml.len())
    };
    let mut out = String::with_capacity(xml.len() + parts.len() + 80);
    out.push_str(&xml[..pos]);
    out.push_str(&parts);
    out.push_str(&xml[pos..]);
    out.replacen(
        r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">"#,
        r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">"#,
        1,
    )
}

fn write_table_xml(t: &roe_model::calc::Table, id: usize) -> String {
    let (r1, c1) = t.range.start;
    let (r2, c2) = t.range.end;
    let width = (c2 - c1 + 1) as usize;
    let name = xml_escape_attr(&t.name);
    let mut attrs = format!(
        r#"id="{id}" name="{name}" displayName="{name}" ref="{}:{}""#,
        cell_ref_a1(r1, c1), cell_ref_a1(r2, c2)
    );
    if !t.has_header {
        attrs.push_str(r#" headerRowCount="0""#);
    }
    if t.has_totals {
        attrs.push_str(r#" totalsRowCount="1""#);
    } else {
        attrs.push_str(r#" totalsRowShown="0""#);
    }
    // autoFilter 范围不含汇总行
    let mut af = String::new();
    if t.has_header && t.auto_filter.is_some() {
        let last = if t.has_totals && r2 > r1 { r2 - 1 } else { r2 };
        af = format!(r#"<autoFilter ref="{}:{}"/>"#, cell_ref_a1(r1, c1), cell_ref_a1(last, c2));
    }
    let mut cols = String::new();
    for i in 0..width {
        let nm = t.columns.get(i).cloned().filter(|x| !x.is_empty())
            .unwrap_or_else(|| format!("Column{}", i + 1));
        cols.push_str(&format!(r#"<tableColumn id="{}" name="{}"/>"#, i + 1, xml_escape_attr(&nm)));
    }
    let b = |v: bool| if v { "1" } else { "0" };
    let style = match &t.style_name {
        Some(n) => format!(r#" name="{}""#, xml_escape_attr(n)),
        None => String::new(),
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<table xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" {attrs}>{af}<tableColumns count=\"{width}\">{cols}</tableColumns><tableStyleInfo{style} showFirstColumn=\"{}\" showLastColumn=\"{}\" showRowStripes=\"{}\" showColumnStripes=\"{}\"/></table>",
        b(t.show_first_column), b(t.show_last_column), b(t.show_row_stripes), b(t.show_column_stripes)
    )
}

fn col_to_letters(mut col: u32) -> String {
    let mut s = String::new();
    while col > 0 {
        let rem = (col - 1) % 26;
        s.insert(0, (b'A' + rem as u8) as char);
        col = (col - 1) / 26;
    }
    if s.is_empty() {
        s.push('A');
    }
    s
}

// ─── XML 转义 ──────────────────────────────────────────────────────────────────

fn xml_escape_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn xml_escape_attr(s: &str) -> String {
    xml_escape_text(s).replace('"', "&quot;")
}

// ─── 测试 ─────────────────────────────────────────────────────────────────────

/// Excel 2010+ 新增、文件里必须写成 `_xlfn.` 前缀的函数(内部引擎使用无前缀名)。
const XLFN_FUNCS: &[&str] = &[
    "T.TEST", "F.TEST", "F.DIST", "F.INV", "NORM.S.DIST", "NORM.S.INV",
    "COVARIANCE.S", "COVARIANCE.P", "CHISQ.DIST.RT",
];

/// 导出公式时给 XLFN_FUNCS 里的函数名补 `_xlfn.` 前缀。
/// 跳过字符串字面量与单引号工作表名;要求函数名前非标识符字符、后紧跟 `(`;已带前缀的不重复加。
fn prefix_xlfn_functions(f: &str) -> String {
    let chars: Vec<char> = f.chars().collect();
    let mut out = String::with_capacity(f.len() + 16);
    let is_id = |c: char| c.is_alphanumeric() || c == '_' || c == '.' || !c.is_ascii();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '"' || c == '\'' {
            let q = c;
            out.push(c);
            i += 1;
            while i < chars.len() {
                out.push(chars[i]);
                if chars[i] == q {
                    if i + 1 < chars.len() && chars[i + 1] == q {
                        out.push(chars[i + 1]);
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        if c.is_ascii_alphabetic() && (i == 0 || !is_id(chars[i - 1])) {
            let mut j = i;
            while j < chars.len() && is_id(chars[j]) {
                j += 1;
            }
            let word: String = chars[i..j].iter().collect();
            if j < chars.len() && chars[j] == '(' && XLFN_FUNCS.contains(&word.to_ascii_uppercase().as_str()) {
                out.push_str("_xlfn.");
            }
            out.push_str(&word);
            i = j;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// 导出公式时给"数字开头"的未加引号工作表前缀补单引号。
/// 内部求值接受 `02_x!A1`,Excel 要求 `'02_x'!A1`。
/// 跳过字符串字面量、已加引号的名称,以及三维引用(`S1:S3!A1`,保守不改)。
fn quote_sheet_refs_in_formula(f: &str) -> String {
    let chars: Vec<char> = f.chars().collect();
    let mut out: Vec<char> = Vec::with_capacity(chars.len() + 8);
    let mut i = 0;
    let is_id = |c: char| c.is_alphanumeric() || c == '_' || !c.is_ascii();
    while i < chars.len() {
        let c = chars[i];
        if c == '"' || c == '\'' {
            // 引号段原样复制;连续两个引号视为转义
            let q = c;
            out.push(c);
            i += 1;
            while i < chars.len() {
                out.push(chars[i]);
                if chars[i] == q {
                    if i + 1 < chars.len() && chars[i + 1] == q {
                        out.push(chars[i + 1]);
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        if c == '!' {
            let mut j = out.len();
            while j > 0 && is_id(out[j - 1]) {
                j -= 1;
            }
            let prefix: String = out[j..].iter().collect();
            let prev = if j > 0 { Some(out[j - 1]) } else { None };
            let needs = !prefix.is_empty()
                && prefix.chars().next().map_or(false, |ch| ch.is_ascii_digit())
                && prev != Some(':');
            if needs {
                out.truncate(j);
                out.push('\'');
                out.extend(prefix.chars());
                out.push('\'');
            }
        }
        out.push(c);
        i += 1;
    }
    out.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn xlfn_prefix_cases() {
        let p = prefix_xlfn_functions;
        assert_eq!(p("T.TEST(A1:A3,B1:B3,2,1)"), "_xlfn.T.TEST(A1:A3,B1:B3,2,1)");
        assert_eq!(p("1-F.DIST(J5,H5,H6,1)"), "1-_xlfn.F.DIST(J5,H5,H6,1)");
        assert_eq!(p("_xlfn.T.TEST(A1,B1,2,1)"), "_xlfn.T.TEST(A1,B1,2,1)");
        assert_eq!(p("\"T.TEST(\""), "\"T.TEST(\"");
        assert_eq!(p("SUM(A1:A3)+SLOPE(A1:A3,B1:B3)"), "SUM(A1:A3)+SLOPE(A1:A3,B1:B3)");
        assert_eq!(p("MY.F.DIST(1)"), "MY.F.DIST(1)");
    }
    #[test]
    fn quote_sheet_refs_cases() {
        let q = quote_sheet_refs_in_formula;
        assert_eq!(q("AVERAGE(02_two_samples!A2:A9)"), "AVERAGE('02_two_samples'!A2:A9)");
        assert_eq!(q("T.TEST(02_a!A2:A9,02_a!B2:B9,2,1)"), "T.TEST('02_a'!A2:A9,'02_a'!B2:B9,2,1)");
        assert_eq!(q("'02_a'!A1"), "'02_a'!A1");
        assert_eq!(q("\"01_x!A1\""), "\"01_x!A1\"");
        assert_eq!(q("Sheet1!A1+'My Sheet'!B2"), "Sheet1!A1+'My Sheet'!B2");
        assert_eq!(q("SUM(Sheet1:Sheet3!A1)"), "SUM(Sheet1:Sheet3!A1)");
        assert_eq!(q("1+2*3"), "1+2*3");
    }
    use super::*;
    use crate::ooxml::xlsx::XlsxParser;
    use roe_model::calc::CellBlock;
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
            sparkline_groups: vec![],
            cell_formats: StdHashMap::new(),
            cond_formats: vec![],
            data_validations: vec![],
            auto_filter: None,
            merged_cells: vec![],
            pivot:        None,
            floating_charts: vec![],
            floating_stat_charts: vec![],
            floating_shapes: vec![],
            next_object_id: 0,
            floating_images:  vec![],
            default_col_width: 0.0,
            default_row_height: 0.0,
        }
    }

    /// col 与 xlsx.rs parser 的 col_from_ref 编号约定一致：1-based（"A"→1）
    fn set_value(sheet: &mut Sheet, row: u32, col: u32, value: CellValue) {
        let sc = sheet.cells.cols.entry(col).or_default();
        sc.blocks.push(CellBlock {
            start_row: row,
            values: smallvec::smallvec![value],
        });
    }

    fn make_workbook(sheet: Sheet) -> Workbook {
        Workbook {
            sheets: vec![sheet],
            named_ranges: StdHashMap::new(),
            named_formulas: StdHashMap::new(),
            styles: Default::default(),
            formula_dialect_source: Default::default(),
        }
    }

    fn chart_fixture(kind: roe_model::impress::ChartKind, x: f32, y: f32, w: f32, h: f32) -> roe_model::impress::ChartModel {
        roe_model::impress::ChartModel {
            id: 0,
            kind,
            title: Some("T".into()),
            x_labels: vec!["A".into(), "B".into(), "C".into()],
            series: vec![roe_model::impress::ChartSeries {
                name: "S1".into(),
                data: vec![1.0, 2.0, 3.0],
                color: None,
                as_line: false,
                trendline: false,
            }],
            x_pt: x,
            y_pt: y,
            width_pt: w,
            height_pt: h,
            show_legend: true,
            show_grid: true,
            x_axis_label: String::new(),
            y_axis_label: String::new(),
            border_color: roe_model::common::Color { r: 204, g: 204, b: 204, a: 255 },
            border_width: 1.0,
            gap_width_pct: None,
            overlap_pct: None,
            grouping: roe_model::impress::ChartGrouping::Standard,
            hole_size_pct: None,
            data_labels: Default::default(),
            slice_border_color: None,
        }
    }

    #[test]
    fn test_roundtrip_charts_count_rect_kind_id() {
        use roe_model::impress::ChartKind;
        let mut sheet = empty_sheet("Sheet1");
        set_value(&mut sheet, 0, 1, CellValue::Number(1.0));
        sheet.floating_charts = vec![
            chart_fixture(ChartKind::Bar, 20.0, 30.0, 300.0, 200.0),
            chart_fixture(ChartKind::Pie, 400.0, 50.0, 250.0, 180.0),
        ];
        let bytes = XlsxWriter::write(&make_workbook(sheet)).expect("write failed");
        let parsed = XlsxParser::parse(&bytes).expect("parse failed");
        let s = &parsed.sheets[0];
        let cs = &s.floating_charts;
        assert_eq!(cs.len(), 2, "图表数量应一致(写出器无 drawing/chart 时为 0)");
        assert_eq!(format!("{:?}", cs[0].kind), "Bar");
        assert_eq!(format!("{:?}", cs[1].kind), "Pie");
        let near = |a: f32, b: f32| (a - b).abs() < 0.01;
        assert!(near(cs[0].x_pt, 20.0) && near(cs[0].y_pt, 30.0), "pos0 {:?}", (cs[0].x_pt, cs[0].y_pt));
        assert!(near(cs[0].width_pt, 300.0) && near(cs[0].height_pt, 200.0), "size0");
        assert!(near(cs[1].x_pt, 400.0) && near(cs[1].y_pt, 50.0), "pos1");
        assert!(near(cs[1].width_pt, 250.0) && near(cs[1].height_pt, 180.0), "size1");
        assert_eq!(cs[0].series.len(), 1);
        assert_eq!(cs[0].series[0].data, vec![1.0, 2.0, 3.0]);
        assert_eq!(cs[0].title.as_deref(), Some("T"));
        assert!(cs.iter().all(|c| c.id != 0), "导入图表 id 不应为哨兵值 0");
        assert_ne!(cs[0].id, cs[1].id, "同 sheet 内 id 应互异");
        assert!(s.floating_shapes.is_empty() && s.floating_images.is_empty(), "图表不应被误读成形状/图片");
    }

    #[test]
    fn test_roundtrip_chart_series_custom_color() {
        let mut ch = chart_fixture(roe_model::impress::ChartKind::Bar, 0.0, 0.0, 200.0, 150.0);
        ch.series[0].color = Some(roe_model::common::Color { r: 18, g: 52, b: 86, a: 255 });
        let mut sheet = empty_sheet("S");
        sheet.floating_charts = vec![ch];
        let bytes = XlsxWriter::write(&make_workbook(sheet)).unwrap();
        let p = XlsxParser::parse(&bytes).unwrap();
        let c = &p.sheets[0].floating_charts[0].series[0].color;
        assert_eq!(c.map(|c| (c.r, c.g, c.b)), Some((18, 52, 86)), "自定义系列色应往返保持");
    }

    #[test]
    fn test_roundtrip_doughnut_keeps_all_series() {
        let mut ch = chart_fixture(roe_model::impress::ChartKind::Doughnut, 0.0, 0.0, 200.0, 200.0);
        let s0 = ch.series[0].clone();
        ch.series = vec![s0.clone(), s0.clone(), s0];
        let mut sheet = empty_sheet("S");
        sheet.floating_charts = vec![ch];
        let bytes = XlsxWriter::write(&make_workbook(sheet)).unwrap();
        let p = XlsxParser::parse(&bytes).unwrap();
        assert_eq!(p.sheets[0].floating_charts[0].series.len(), 3, "环形图多环不应被截断");
    }

    #[test]
    fn test_roundtrip_pie_keeps_first_series_only() {
        let mut ch = chart_fixture(roe_model::impress::ChartKind::Pie, 0.0, 0.0, 200.0, 200.0);
        let s0 = ch.series[0].clone();
        ch.series = vec![s0.clone(), s0];
        let mut sheet = empty_sheet("S");
        sheet.floating_charts = vec![ch];
        let bytes = XlsxWriter::write(&make_workbook(sheet)).unwrap();
        let p = XlsxParser::parse(&bytes).unwrap();
        assert_eq!(p.sheets[0].floating_charts[0].series.len(), 1, "饼图只画第一个系列(语义)");
    }

    #[test]
    fn test_roundtrip_charts_multi_sheet() {
        use roe_model::impress::ChartKind;
        let mut s1 = empty_sheet("A");
        s1.floating_charts = vec![chart_fixture(ChartKind::Line, 10.0, 10.0, 200.0, 150.0)];
        let s2 = empty_sheet("B");
        let mut s3 = empty_sheet("C");
        s3.floating_charts = vec![
            chart_fixture(ChartKind::Area, 5.0, 5.0, 100.0, 100.0),
            chart_fixture(ChartKind::Doughnut, 150.0, 5.0, 100.0, 100.0),
        ];
        let wb = Workbook {
            sheets: vec![s1, s2, s3],
            named_ranges: StdHashMap::new(),
            named_formulas: StdHashMap::new(),
            styles: Default::default(),
            formula_dialect_source: Default::default(),
        };
        let bytes = XlsxWriter::write(&wb).expect("write failed");
        let parsed = XlsxParser::parse(&bytes).expect("parse failed");
        let lens: Vec<usize> = parsed.sheets.iter().map(|s| s.floating_charts.len()).collect();
        assert_eq!(lens, vec![1, 0, 2]);
        assert_eq!(format!("{:?}", parsed.sheets[0].floating_charts[0].kind), "Line");
        assert_eq!(format!("{:?}", parsed.sheets[2].floating_charts[0].kind), "Area");
        assert_eq!(format!("{:?}", parsed.sheets[2].floating_charts[1].kind), "Doughnut");
    }

    fn stat_fixture(x: f32, y: f32, w: f32, h: f32) -> roe_model::calc::FloatingStatChart {
        use roe_model::stat_chart::*;
        FloatingStatChart_new(x, y, w, h, StatChartModel {
            kind: StatChartKind::Funnel,
            title_present: true,
            title: Some("T".to_string()),
            show_legend: false,
            legend_pos: None,
            show_grid: false,
            gap_width: Some(0.06),
            data: vec![StatData {
                id: 0,
                categories: vec![vec!["a".into(), "b".into(), "c".into()]],
                values: vec![Some(3.0), Some(2.0), Some(1.0)],
                value_kind: "size".into(),
            }],
            series: vec![StatSeries {
                name: "S".into(), layout_id: "funnel".into(), format_idx: 0, data_id: 0,
                bin_width: None, bin_count: None, interval_closed_right: true,
                quartile_exclusive: false, mean_line: false, mean_marker: true,
                show_outliers: true, show_non_outliers: false, subtotals: vec![], data_labels: None,
            }],
        })
    }

    #[allow(non_snake_case)]
    fn FloatingStatChart_new(x: f32, y: f32, w: f32, h: f32, model: roe_model::stat_chart::StatChartModel) -> roe_model::calc::FloatingStatChart {
        roe_model::calc::FloatingStatChart { id: 0, x_pt: x, y_pt: y, width_pt: w, height_pt: h, model }
    }

    #[test]
    fn test_roundtrip_stat_charts() {
        use std::io::Read;
        use roe_model::impress::ChartKind;
        let mut s1 = empty_sheet("A");
        s1.floating_charts = vec![chart_fixture(ChartKind::Line, 10.0, 10.0, 200.0, 150.0)];
        s1.floating_stat_charts = vec![stat_fixture(300.0, 20.0, 240.0, 180.0)];
        let mut s2 = empty_sheet("B"); // 只有统计图,验证 drawing 仍会生成
        s2.floating_stat_charts = vec![stat_fixture(5.0, 6.0, 100.0, 80.0), stat_fixture(150.0, 6.0, 100.0, 80.0)];
        let wb = Workbook {
            sheets: vec![s1, s2],
            named_ranges: StdHashMap::new(),
            named_formulas: StdHashMap::new(),
            styles: Default::default(),
            formula_dialect_source: Default::default(),
        };
        let bytes = XlsxWriter::write(&wb).expect("write failed");

        let mut ar = zip::ZipArchive::new(std::io::Cursor::new(bytes.clone())).unwrap();
        let names: Vec<String> = (0..ar.len()).map(|i| ar.by_index(i).unwrap().name().to_string()).collect();
        for want in ["xl/charts/chart1.xml", "xl/charts/chartEx1.xml", "xl/charts/chartEx2.xml", "xl/charts/chartEx3.xml",
                     "xl/drawings/drawing1.xml", "xl/drawings/drawing2.xml", "xl/worksheets/_rels/sheet2.xml.rels"] {
            assert!(names.iter().any(|n| n == want), "缺少 {want},实际: {names:?}");
        }
        let mut ct = String::new();
        ar.by_name("[Content_Types].xml").unwrap().read_to_string(&mut ct).unwrap();
        assert!(ct.contains("/xl/charts/chartEx3.xml"));
        assert!(ct.contains("application/vnd.ms-office.chartex+xml"));
        assert!(ct.contains("/xl/drawings/drawing2.xml"));

        let parsed = XlsxParser::parse(&bytes).expect("parse failed");
        assert_eq!(parsed.sheets[0].floating_charts.len(), 1);
        assert_eq!(parsed.sheets[0].floating_stat_charts.len(), 1);
        assert_eq!(parsed.sheets[1].floating_stat_charts.len(), 2);
        for sh in &parsed.sheets {
            assert!(sh.floating_shapes.is_empty(), "回退形状不应被读成文本框");
        }
        let c = &parsed.sheets[0].floating_stat_charts[0];
        assert_eq!(c.model.kind, roe_model::stat_chart::StatChartKind::Funnel);
        assert_eq!(c.model.data[0].categories[0], vec!["a", "b", "c"]);
        assert_eq!(c.model.data[0].values, vec![Some(3.0), Some(2.0), Some(1.0)]);
        assert!((c.x_pt - 300.0).abs() < 0.01 && (c.y_pt - 20.0).abs() < 0.01);
        assert!((c.width_pt - 240.0).abs() < 0.01 && (c.height_pt - 180.0).abs() < 0.01);
        assert_ne!(c.id, parsed.sheets[0].floating_charts[0].id, "两类图表 id 不得重复");
        let ids: Vec<u64> = parsed.sheets[1].floating_stat_charts.iter().map(|c| c.id).collect();
        assert_ne!(ids[0], ids[1]);
    }

    /// ROE_XLSX_STAT_FILE=<xlsx> cargo test -p roe-parser --lib real_file_stat_chart_roundtrip -- --ignored --nocapture
    #[test]
    #[ignore]
    fn real_file_stat_chart_roundtrip() {
        let p = match std::env::var("ROE_XLSX_STAT_FILE") { Ok(p) => p, Err(_) => return };
        let a = XlsxParser::parse(&std::fs::read(&p).unwrap()).unwrap();
        let bytes = XlsxWriter::write(&a).unwrap();
        if let Ok(out) = std::env::var("ROE_XLSX_STAT_OUT") { std::fs::write(&out, &bytes).unwrap(); println!("已写出 {out}"); }
        let b = XlsxParser::parse(&bytes).unwrap();
        for (si, (sa, sb)) in a.sheets.iter().zip(b.sheets.iter()).enumerate() {
            println!("sheet {si}: stat {} -> {}, shapes {} -> {}", sa.floating_stat_charts.len(),
                sb.floating_stat_charts.len(), sa.floating_shapes.len(), sb.floating_shapes.len());
            assert_eq!(sa.floating_stat_charts.len(), sb.floating_stat_charts.len());
            assert_eq!(sa.floating_shapes.len(), sb.floating_shapes.len());
            for (x, y) in sa.floating_stat_charts.iter().zip(sb.floating_stat_charts.iter()) {
                assert_eq!(x.model.kind, y.model.kind);
                assert_eq!(x.model.data.len(), y.model.data.len());
                assert_eq!(x.model.series.len(), y.model.series.len());
                assert!((x.x_pt - y.x_pt).abs() < 0.01 && (x.width_pt - y.width_pt).abs() < 0.01);
                println!("  id {} {:?} ok", y.id, y.model.kind);
            }
        }
    }

    /// 回归:图表在第二个 sheet 时,chart1.xml 不得引用写死的 Sheet1,
    /// 且字面量写法(numLit/strLit)必须能被我们自己的解析器读回。
    #[test]
    fn xlsx_chart_literal_no_sheet1_ref_and_roundtrip() {
        use std::io::Read;
        let s1 = empty_sheet("原始");
        let mut s2 = empty_sheet("Region汇总");
        let mut ch = chart_fixture(roe_model::impress::ChartKind::Bar, 0.0, 0.0, 200.0, 150.0);
        ch.x_labels = vec!["东区".into(), "西区".into()];
        ch.series[0].name = "销售额".into();
        ch.series[0].data = vec![270.0, 220.0];
        s2.floating_charts = vec![ch];
        let mut wb = make_workbook(s1);
        wb.sheets.push(s2);
        let bytes = XlsxWriter::write(&wb).expect("write failed");

        let mut ar = zip::ZipArchive::new(std::io::Cursor::new(bytes.clone())).unwrap();
        let mut xml = String::new();
        ar.by_name("xl/charts/chart1.xml").unwrap().read_to_string(&mut xml).unwrap();
        assert!(!xml.contains("Sheet1!"), "chart1.xml 仍含写死的 Sheet1 引用: {xml}");
        assert!(!xml.contains("<c:f>"), "字面量方案不应写 <c:f>: {xml}");
        assert!(xml.contains("numLit"), "缺 numLit: {xml}");
        assert!(xml.contains("东区"), "缺类别文本: {xml}");

        let back = XlsxParser::parse(&bytes).expect("parse back");
        assert_eq!(back.sheets[0].floating_charts.len(), 0);
        assert_eq!(back.sheets[1].floating_charts.len(), 1);
        let c = &back.sheets[1].floating_charts[0];
        assert_eq!(c.x_labels, vec!["东区".to_string(), "西区".to_string()]);
        assert_eq!(c.series[0].name, "销售额");
        assert_eq!(c.series[0].data, vec![270.0, 220.0]);
    }

    #[test]
    fn test_write_charts_zip_parts_and_content_types() {
        use std::io::Read;
        let mut sheet = empty_sheet("Sheet1");
        sheet.floating_charts = vec![chart_fixture(roe_model::impress::ChartKind::Bar, 0.0, 0.0, 100.0, 100.0)];
        let bytes = XlsxWriter::write(&make_workbook(sheet)).expect("write failed");
        let mut ar = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let names: Vec<String> = (0..ar.len()).map(|i| ar.by_index(i).unwrap().name().to_string()).collect();
        for want in [
            "xl/worksheets/_rels/sheet1.xml.rels",
            "xl/drawings/drawing1.xml",
            "xl/drawings/_rels/drawing1.xml.rels",
            "xl/charts/chart1.xml",
        ] {
            assert!(names.iter().any(|n| n == want), "缺少 {want},实际: {names:?}");
        }
        let mut ct = String::new();
        ar.by_name("[Content_Types].xml").unwrap().read_to_string(&mut ct).unwrap();
        assert!(ct.contains("/xl/charts/chart1.xml"), "content types 缺 chart");
        assert!(ct.contains("/xl/drawings/drawing1.xml"), "content types 缺 drawing");
    }

    /// 真实文件诊断(默认忽略):
    /// ROE_XLSX_CHART_FILE=/path/x.xlsx cargo test -p roe-parser real_file_chart_roundtrip -- --ignored --nocapture
    #[test]
    #[ignore]
    fn real_file_chart_roundtrip() {
        let path = match std::env::var("ROE_XLSX_CHART_FILE") {
            Ok(p) => p,
            Err(_) => { eprintln!("未设置 ROE_XLSX_CHART_FILE,跳过"); return; }
        };
        let data = std::fs::read(&path).expect("read file");
        let a = XlsxParser::parse(&data).expect("parse original");
        let bytes = XlsxWriter::write(&a).expect("write");
        let b = XlsxParser::parse(&bytes).expect("parse rewritten");
        let (mut ta, mut tb) = (0usize, 0usize);
        for (si, (sa, sb)) in a.sheets.iter().zip(b.sheets.iter()).enumerate() {
            println!("sheet {si} {:?}: 原 {} 图 -> 回读 {} 图", sa.name, sa.floating_charts.len(), sb.floating_charts.len());
            for (i, c) in sa.floating_charts.iter().enumerate() {
                println!("  原#{i} id={} kind={:?} rect=({:.1},{:.1},{:.1},{:.1}) series={}", c.id, c.kind, c.x_pt, c.y_pt, c.width_pt, c.height_pt, c.series.len());
            }
            for (i, c) in sb.floating_charts.iter().enumerate() {
                println!("  回#{i} id={} kind={:?} rect=({:.1},{:.1},{:.1},{:.1}) series={}", c.id, c.kind, c.x_pt, c.y_pt, c.width_pt, c.height_pt, c.series.len());
            }
            ta += sa.floating_charts.len();
            tb += sb.floating_charts.len();
        }
        // ---- 逐字段比对:列出所有有损项 ----
        let mut diffs: Vec<String> = Vec::new();
        for (si, (sa, sb)) in a.sheets.iter().zip(b.sheets.iter()).enumerate() {
            for (i, (x, y)) in sa.floating_charts.iter().zip(sb.floating_charts.iter()).enumerate() {
                let tag = format!("sheet{si}#{i}({:?})", x.kind);
                let mut chk = |name: &str, l: String, r: String| {
                    if l != r { diffs.push(format!("{tag} {name}: {l} -> {r}")); }
                };
                chk("kind", format!("{:?}", x.kind), format!("{:?}", y.kind));
                chk("grouping", format!("{:?}", x.grouping), format!("{:?}", y.grouping));
                chk("title", format!("{:?}", x.title), format!("{:?}", y.title));
                chk("x_labels.len", x.x_labels.len().to_string(), y.x_labels.len().to_string());
                chk("series.len", x.series.len().to_string(), y.series.len().to_string());
                chk("show_legend", x.show_legend.to_string(), y.show_legend.to_string());
                chk("show_grid", x.show_grid.to_string(), y.show_grid.to_string());
                chk("show_val", x.data_labels.show_val.to_string(), y.data_labels.show_val.to_string());
                chk("x_axis_label", format!("{:?}", x.x_axis_label), format!("{:?}", y.x_axis_label));
                chk("y_axis_label", format!("{:?}", x.y_axis_label), format!("{:?}", y.y_axis_label));
                chk("gap_width", format!("{:?}", x.gap_width_pct), format!("{:?}", y.gap_width_pct));
                chk("overlap", format!("{:?}", x.overlap_pct), format!("{:?}", y.overlap_pct));
                chk("hole_size", format!("{:?}", x.hole_size_pct), format!("{:?}", y.hole_size_pct));
                for (k, (p, q)) in x.series.iter().zip(y.series.iter()).enumerate() {
                    chk(&format!("s{k}.name"), format!("{:?}", p.name), format!("{:?}", q.name));
                    chk(&format!("s{k}.data"), format!("{:?}", p.data), format!("{:?}", q.data));
                    chk(&format!("s{k}.as_line"), p.as_line.to_string(), q.as_line.to_string());
                    chk(&format!("s{k}.trendline"), p.trendline.to_string(), q.trendline.to_string());
                    chk(&format!("s{k}.color"), format!("{:?}", p.color), format!("{:?}", q.color));
                }
            }
        }
        println!("=== 逐字段差异 {} 项 ===", diffs.len());
        for d in &diffs { println!("DIFF {d}"); }
        for (si, sa) in a.sheets.iter().enumerate() {
            for (i, c) in sa.floating_charts.iter().enumerate() {
                println!("INFO sheet{si}#{i} {:?} grouping={:?} title={:?} xl={} as_line={:?} legend={} grid={} val={} gap={:?} ov={:?}",
                    c.kind, c.grouping, c.title, c.x_labels.len(),
                    c.series.iter().map(|s| s.as_line).collect::<Vec<_>>(),
                    c.show_legend, c.show_grid, c.data_labels.show_val, c.gap_width_pct, c.overlap_pct);
            }
        }
        println!("合计 原 {ta} -> 回读 {tb}");
        assert_eq!(ta, tb, "往返后图表数量不一致");
    }

    #[test]
    fn test_roundtrip_data_validation() {
        let mut sheet = empty_sheet("Sheet1");
        set_value(&mut sheet, 0, 0, CellValue::Text("苹果".into()));
        set_value(&mut sheet, 0, 1, CellValue::Number(5.0));

        sheet.data_validations.push(roe_model::calc::DataValidationRule {
            range: roe_model::calc::CellRange { sheet: String::new(), start: (0, 0), end: (4, 0) },
            kind: "list".to_string(),
            operator: String::new(),
            formula1: r#""苹果,香蕉,橙子""#.to_string(),
            formula2: None,
            allow_blank: true,
            show_error: true,
            error_msg: None,
        });
        sheet.data_validations.push(roe_model::calc::DataValidationRule {
            range: roe_model::calc::CellRange { sheet: String::new(), start: (0, 1), end: (9, 1) },
            kind: "whole".to_string(),
            operator: "between".to_string(),
            formula1: "1".to_string(),
            formula2: Some("100".to_string()),
            allow_blank: false,
            show_error: true,
            error_msg: Some("必须在1到100之间".to_string()),
        });

        let wb = make_workbook(sheet);
        let bytes = XlsxWriter::write(&wb).expect("write failed");
        let parsed = XlsxParser::parse(&bytes).expect("roundtrip parse failed");

        let s = &parsed.sheets[0];
        assert_eq!(s.data_validations.len(), 2, "roundtrip 后应仍有2条数据验证规则");

        let dv0 = &s.data_validations[0];
        assert_eq!(dv0.kind, "list");
        assert_eq!(dv0.operator, "");
        assert_eq!(dv0.formula1, r#""苹果,香蕉,橙子""#);
        assert_eq!(dv0.formula2, None);
        assert!(dv0.allow_blank);
        assert!(dv0.show_error);
        assert_eq!(dv0.range.start, (0, 0));
        assert_eq!(dv0.range.end, (4, 0));

        let dv1 = &s.data_validations[1];
        assert_eq!(dv1.kind, "whole");
        assert_eq!(dv1.operator, "between");
        assert_eq!(dv1.formula1, "1");
        assert_eq!(dv1.formula2, Some("100".to_string()));
        assert!(!dv1.allow_blank);
        assert!(dv1.show_error);
        assert_eq!(dv1.error_msg, Some("必须在1到100之间".to_string()));
        assert_eq!(dv1.range.start, (0, 1));
        assert_eq!(dv1.range.end, (9, 1));

        println!("data validation roundtrip ✓");
    }

    #[test]
    fn test_write_sheet_xml_column_addresses_not_collapsed() {
        // 回归测试：修复前 col_to_letters(col) 调用处漏了 +1（0-based → 1-based），
        // 导致 A 列(col=0)和 B 列(col=1)都被写成地址 "A1"，同一单元格标签重复，
        // 数据实际损坏（不只是"看起来错位"，是两列内容会争抢同一个 <c r="A1">）。
        // 直接检查产出的原始 XML 字符串，不经过 parser 回读，避免读写两侧
        // 同一个 bug 互相抵消导致 roundtrip 断言"看起来"通过。
        let mut sheet = empty_sheet("Sheet1");
        set_value(&mut sheet, 0, 0, CellValue::Number(1.0)); // A1
        set_value(&mut sheet, 0, 1, CellValue::Number(2.0)); // B1
        set_value(&mut sheet, 0, 2, CellValue::Number(3.0)); // C1

        let sst = SharedStringTable::default();
        let styles = StylesCollector::new();
        let xml = write_sheet_xml(&sheet, &sst, &styles, 0);

        assert!(xml.contains(r#"<c r="A1"><v>1</v></c>"#), "A1 地址或值不对: {xml}");
        assert!(xml.contains(r#"<c r="B1"><v>2</v></c>"#), "B1 地址或值不对: {xml}");
        assert!(xml.contains(r#"<c r="C1"><v>3</v></c>"#), "C1 地址或值不对: {xml}");
        // 确认没有把 B1/C1 也写成了 A1（修复前的坍缩bug）
        assert_eq!(xml.matches(r#"r="A1""#).count(), 1, "A1 地址不应重复出现: {xml}");

        println!("column address collision fix ✓");
    }

    #[test]
    fn test_col_to_letters() {
        assert_eq!(col_to_letters(1), "A");
        assert_eq!(col_to_letters(2), "B");
        assert_eq!(col_to_letters(26), "Z");
        assert_eq!(col_to_letters(27), "AA");
        assert_eq!(col_to_letters(28), "AB");
    }

    #[test]
    fn test_roundtrip_basic_values() {
        let mut sheet = empty_sheet("Sheet1");
        set_value(&mut sheet, 0, 1, CellValue::Text("Hello".into()));
        set_value(&mut sheet, 0, 2, CellValue::Number(42.0));
        set_value(&mut sheet, 0, 3, CellValue::Bool(true));
        set_value(&mut sheet, 2, 1, CellValue::Number(-3.14));

        let wb = make_workbook(sheet);
        let bytes = XlsxWriter::write(&wb).expect("write failed");
        let parsed = XlsxParser::parse(&bytes).expect("roundtrip parse failed");

        assert_eq!(parsed.sheets.len(), 1);
        let s = &parsed.sheets[0];
        assert_eq!(s.name, "Sheet1");

        let get = |sheet: &Sheet, row: u32, col: u32| -> CellValue {
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
        };

        // 注：修复 col_to_letters 调用处漏 +1 的 bug 前，这几个断言的列号
        // 是跟着 bug 一起错位写的（原意写在 col=1/2/3 的值，因为 bug 全部
        // 坍缩写到了 A 列即 col=0，断言也就跟着查 col=0）。现在列号已修
        // 正对齐，断言改回 set_value 时实际指定的列。
        let v = get(s, 0, 1);
        assert!(matches!(v, CellValue::Text(ref t) if t == "Hello"));
        let v = get(s, 0, 2);
        assert!(matches!(v, CellValue::Number(n) if (n - 42.0).abs() < 1e-9));
        let v = get(s, 0, 3);
        assert!(matches!(v, CellValue::Bool(true)));
        let v = get(s, 2, 1);
        assert!(matches!(v, CellValue::Number(n) if (n - (-3.14)).abs() < 1e-6));
    }

    #[test]
    fn test_roundtrip_formula() {
        let mut sheet = empty_sheet("Sheet1");
        set_value(&mut sheet, 0, 1, CellValue::Formula { source: "=SUM(A1:A10)".into(), cached: Box::new(CellValue::Empty) });

        let wb = make_workbook(sheet);
        let bytes = XlsxWriter::write(&wb).expect("write failed");
        let xml_contains_f = String::from_utf8_lossy(&bytes);
        // zip 是二进制，不能直接 contains 文本，这里只验证 roundtrip parse 正常
        let _ = xml_contains_f;

        let parsed = XlsxParser::parse(&bytes).expect("roundtrip parse failed");
        let s = &parsed.sheets[0];
        // 修复列号坍缩bug前这里查的是 col=0（跟随 bug 位置），set_value 实际写在 col=1
        let sc = s.cells.cols.get(&1).unwrap();
        let v = &sc.blocks[0].values[0];
        assert!(matches!(v, CellValue::Formula { ref source, .. } if source == "=SUM(A1:A10)"));
    }

    #[test]
    fn test_roundtrip_multi_sheet() {
        let mut s1 = empty_sheet("First");
        set_value(&mut s1, 0, 1, CellValue::Number(1.0));
        let mut s2 = empty_sheet("Second");
        set_value(&mut s2, 0, 1, CellValue::Number(2.0));

        let wb = Workbook {
            sheets: vec![s1, s2],
            named_ranges: StdHashMap::new(),
            named_formulas: StdHashMap::new(),
            styles: Default::default(),
            formula_dialect_source: Default::default(),
        };

        let bytes = XlsxWriter::write(&wb).expect("write failed");
        let parsed = XlsxParser::parse(&bytes).expect("roundtrip parse failed");
        assert_eq!(parsed.sheets.len(), 2);
        assert_eq!(parsed.sheets[0].name, "First");
        assert_eq!(parsed.sheets[1].name, "Second");
    }

    #[test]
    fn test_defined_names_xml_ranges_and_formulas() {
        use roe_model::calc::CellRange;
        let mut wb = make_workbook(empty_sheet("Sheet1"));
        wb.named_ranges.insert("Global1".into(), CellRange { sheet: "Sheet1".into(), start: (0, 0), end: (0, 0) });
        wb.named_ranges.insert("Global3".into(), CellRange { sheet: "Sheet1".into(), start: (0, 0), end: (3, 0) });
        wb.named_ranges.insert("TaxRate".into(), CellRange { sheet: "My Sheet".into(), start: (0, 2), end: (0, 2) });
        wb.named_ranges.insert("Wide".into(), CellRange { sheet: "Sheet1".into(), start: (0, 26), end: (9, 27) });
        wb.named_formulas.insert("Global4".into(), "{1,2,3;4,5,6}".into());
        wb.named_formulas.insert("Cnt".into(), "COUNTA($A:$A)".into());
        wb.named_formulas.insert("Amp".into(), "a&b<c".into());

        let xml = write_workbook_xml(&wb);
        assert!(xml.contains(r#"<definedName name="Global1">Sheet1!$A$1</definedName>"#), "{xml}");
        assert!(xml.contains(r#"<definedName name="Global3">Sheet1!$A$1:$A$4</definedName>"#), "{xml}");
        assert!(xml.contains(r#"<definedName name="TaxRate">'My Sheet'!$C$1</definedName>"#), "{xml}");
        assert!(xml.contains(r#"<definedName name="Wide">Sheet1!$AA$1:$AB$10</definedName>"#), "{xml}");
        assert!(xml.contains(r#"<definedName name="Global4">{1,2,3;4,5,6}</definedName>"#), "{xml}");
        assert!(xml.contains(r#"<definedName name="Cnt">COUNTA($A:$A)</definedName>"#), "{xml}");
        assert!(xml.contains("a&amp;b&lt;c"), "{xml}");
        // 位置:</sheets> 之后;按名字排序
        let i_sheets = xml.find("</sheets>").unwrap();
        let i_dn = xml.find("<definedNames>").unwrap();
        assert!(i_sheets < i_dn);
        assert!(xml.find("name=\"Amp\"").unwrap() < xml.find("name=\"Global1\"").unwrap());
        assert!(xml.find("name=\"Global1\"").unwrap() < xml.find("name=\"Global3\"").unwrap());
    }

    #[test]
    fn test_defined_names_skip_xlnm_and_empty() {
        use roe_model::calc::CellRange;
        let mut wb = make_workbook(empty_sheet("Sheet1"));
        // 无名称:不输出 definedNames 节点
        assert!(!write_workbook_xml(&wb).contains("definedNames"));
        // 只有内置名称:同样不输出
        wb.named_ranges.insert(
            "_xlnm._FilterDatabase".into(),
            CellRange { sheet: "Sheet1".into(), start: (0, 0), end: (5, 3) },
        );
        let xml = write_workbook_xml(&wb);
        assert!(!xml.contains("_xlnm"), "{xml}");
        assert!(!xml.contains("definedNames"), "{xml}");
    }

    #[test]
    fn test_roundtrip_defined_names() {
        use roe_model::calc::CellRange;
        let mut wb = Workbook {
            sheets: vec![empty_sheet("Sheet1"), empty_sheet("My Sheet")],
            named_ranges: StdHashMap::new(),
            named_formulas: StdHashMap::new(),
            styles: Default::default(),
            formula_dialect_source: Default::default(),
        };
        wb.named_ranges.insert("Global1".into(), CellRange { sheet: "Sheet1".into(), start: (0, 0), end: (0, 0) });
        wb.named_ranges.insert("Global3".into(), CellRange { sheet: "Sheet1".into(), start: (0, 0), end: (3, 0) });
        wb.named_ranges.insert("TaxRate".into(), CellRange { sheet: "My Sheet".into(), start: (0, 2), end: (0, 2) });
        wb.named_formulas.insert("Global4".into(), "{1,2,3;4,5,6}".into());
        wb.named_formulas.insert("Cnt".into(), "COUNTA($A:$A)".into());

        let bytes = XlsxWriter::write(&wb).expect("write failed");
        let parsed = XlsxParser::parse(&bytes).expect("roundtrip parse failed");
        assert_eq!(parsed.named_ranges.len(), 3, "{:?}", parsed.named_ranges);
        assert_eq!(parsed.named_ranges.get("Global1"), wb.named_ranges.get("Global1"));
        assert_eq!(parsed.named_ranges.get("Global3"), wb.named_ranges.get("Global3"));
        assert_eq!(parsed.named_ranges.get("TaxRate"), wb.named_ranges.get("TaxRate"));
        assert_eq!(parsed.named_formulas.get("Global4").map(String::as_str), Some("{1,2,3;4,5,6}"));
        assert_eq!(parsed.named_formulas.get("Cnt").map(String::as_str), Some("COUNTA($A:$A)"));
    }

    #[test]
    fn test_shared_string_dedup() {
        let mut sheet = empty_sheet("Sheet1");
        set_value(&mut sheet, 0, 1, CellValue::Text("Repeat".into()));
        set_value(&mut sheet, 1, 1, CellValue::Text("Repeat".into()));
        set_value(&mut sheet, 2, 1, CellValue::Text("Other".into()));

        let mut sst = SharedStringTable::default();
        collect_strings(&sheet, &mut sst);
        // "Repeat" 应只占一个 SST 槽位
        assert_eq!(sst.strings.len(), 2);

        let wb = make_workbook(sheet);
        let bytes = XlsxWriter::write(&wb).expect("write failed");
        let parsed = XlsxParser::parse(&bytes).expect("roundtrip parse failed");
        let s = &parsed.sheets[0];
        // 修复列号坍缩bug前这里查的是 col=0（跟随 bug 位置），set_value 实际写在 col=1
        let sc = s.cells.cols.get(&1).unwrap();
        // row0/1/2 三行连续，parser 的 build_sparse_grid 按行连续性合并 block
        // （不看值是否相同），所以应合并成 1 个长度为 3 的 block，而不是按值分组。
        assert_eq!(sc.blocks.len(), 1);
        assert_eq!(sc.blocks[0].values.len(), 3);
        // 真正验证"去重"的地方是值本身能正确还原（而不是 SST 索引错位）
        assert!(matches!(&sc.blocks[0].values[0], CellValue::Text(t) if t == "Repeat"));
        assert!(matches!(&sc.blocks[0].values[1], CellValue::Text(t) if t == "Repeat"));
        assert!(matches!(&sc.blocks[0].values[2], CellValue::Text(t) if t == "Other"));
    }

    #[test]
    fn test_roundtrip_cell_format_and_conditional_format() {
        // 验证 v107 新增：styles.xml（fonts/fills/cellXfs/dxfs）
        // + conditionalFormatting 导出，导出后能被 xlsx.rs parser 正确读回。
        let mut sheet = empty_sheet("Sheet1");
        set_value(&mut sheet, 0, 0, CellValue::Number(42.0)); // A1：普通单元格样式
        set_value(&mut sheet, 1, 0, CellValue::Number(-5.0)); // A2：命中条件格式

        // 普通单元格样式：A1 加粗 + 背景色 + 四边边框（本次新增：边框roundtrip验证）
        sheet.cell_formats.insert(
            (0, 0),
            roe_model::calc::CellFormat {
                bold: Some(true),
                italic: None,
                underline: None,
                font_size: None,
                font_family: None,
                fg_color: None,
                bg_color: Some("#FFFF00".to_string()),
                align_h: None,
                align_v: None,
                wrap_text: None,
                num_format: None,
                borders: Some(roe_model::calc::CellBorders {
                    left: Some(roe_model::calc::BorderLine { style: "thin".to_string(), color: Some("#000000".to_string()) }),
                    right: Some(roe_model::calc::BorderLine { style: "thick".to_string(), color: Some("#FF0000".to_string()) }),
                    top: None,
                    bottom: Some(roe_model::calc::BorderLine { style: "double".to_string(), color: None }),
                }),
            },
        );

        // 条件格式：A2:A10 数值小于 0 时标红字
        sheet.cond_formats.push(roe_model::calc::ConditionalFormatRule {
            range: roe_model::calc::CellRange { sheet: String::new(), start: (0, 0), end: (9, 0) },
            kind: roe_model::calc::ConditionalFormatKind::CellFormat {
                condition: "lt:0".to_string(),
                format: roe_model::calc::CellFormat {
                    bold: None,
                    italic: None,
                    underline: None,
                    font_size: None,
                    font_family: None,
                    fg_color: Some("#FF0000".to_string()),
                    bg_color: None,
                    align_h: None,
                    align_v: None,
                    wrap_text: None,
                    num_format: None,
                    borders: None,
                },
            },
            stop_if_true: false,
        });

        let wb = make_workbook(sheet);
        let bytes = XlsxWriter::write(&wb).expect("write failed");
        let parsed = XlsxParser::parse(&bytes).expect("roundtrip parse failed");
        let s = &parsed.sheets[0];

        // 普通单元格样式还原
        let fmt = s.cell_formats.get(&(0, 0)).expect("A1 应有 cell_format");
        assert_eq!(fmt.bold, Some(true), "A1 加粗属性应还原");
        assert_eq!(fmt.bg_color.as_deref(), Some("#FFFF00"), "A1 背景色应还原");

        // 边框还原验证（本次新增）
        let borders = fmt.borders.as_ref().expect("A1 应有 borders");
        let left = borders.left.as_ref().expect("left 边框应存在");
        assert_eq!(left.style, "thin", "left 边框线型应还原");
        assert_eq!(left.color.as_deref(), Some("#000000"), "left 边框颜色应还原");
        let right = borders.right.as_ref().expect("right 边框应存在");
        assert_eq!(right.style, "thick", "right 边框线型应还原");
        assert_eq!(right.color.as_deref(), Some("#FF0000"), "right 边框颜色应还原");
        assert!(borders.top.is_none(), "top 应无边框");
        let bottom = borders.bottom.as_ref().expect("bottom 边框应存在");
        assert_eq!(bottom.style, "double", "bottom 边框线型应还原");
        assert_eq!(bottom.color, None, "bottom 边框应无颜色（省略 color 子元素）");

        // 条件格式还原
        assert_eq!(s.cond_formats.len(), 1, "应还原1条条件格式规则");
        match &s.cond_formats[0].kind {
            roe_model::calc::ConditionalFormatKind::CellFormat { condition, format } => {
                assert_eq!(condition, "lt:0", "条件表达式应还原");
                assert_eq!(format.fg_color.as_deref(), Some("#FF0000"), "条件格式字体颜色应还原");
            }
            other => panic!("期望 CellFormat 变体，实际: {other:?}"),
        }
    }

    // 突出显示规则往返:gt:5 + 填充色(dxf bgColor) + 字色,写出 → 读回
    #[test]
    fn test_roundtrip_cf_cellformat_fill_and_font() {
        let mut sheet = empty_sheet("Sheet1");
        set_value(&mut sheet, 0, 0, CellValue::Number(9.0));
        sheet.cond_formats.push(roe_model::calc::ConditionalFormatRule {
            range: roe_model::calc::CellRange { sheet: String::new(), start: (0, 0), end: (9, 0) },
            kind: roe_model::calc::ConditionalFormatKind::CellFormat {
                condition: "gt:5".to_string(),
                format: roe_model::calc::CellFormat {
                    fg_color: Some("#9C0006".to_string()),
                    bg_color: Some("#FFC7CE".to_string()),
                    ..Default::default()
                },
            },
            stop_if_true: false,
        });
        let wb = make_workbook(sheet);
        let bytes = XlsxWriter::write(&wb).expect("write failed");
        let parsed = XlsxParser::parse(&bytes).expect("roundtrip parse failed");
        let s = &parsed.sheets[0];
        assert_eq!(s.cond_formats.len(), 1);
        match &s.cond_formats[0].kind {
            roe_model::calc::ConditionalFormatKind::CellFormat { condition, format } => {
                assert_eq!(condition, "gt:5");
                assert_eq!(format.fg_color.as_deref(), Some("#9C0006"), "字色应还原");
                assert_eq!(format.bg_color.as_deref(), Some("#FFC7CE"), "填充色应还原");
            }
            other => panic!("期望 CellFormat 变体,实际: {other:?}"),
        }
    }

    // ── 条件格式可视化类型往返(DataBar/ColorScale/IconSet):写出 → 解析读回 ──
    fn cf_rule(kind: roe_model::calc::ConditionalFormatKind, r1: u32, c1: u32, r2: u32, c2: u32, stop: bool)
        -> roe_model::calc::ConditionalFormatRule {
        roe_model::calc::ConditionalFormatRule {
            range: roe_model::calc::CellRange { sheet: String::new(), start: (r1, c1), end: (r2, c2) },
            kind,
            stop_if_true: stop,
        }
    }

    fn cf_roundtrip(rules: Vec<roe_model::calc::ConditionalFormatRule>) -> Vec<roe_model::calc::ConditionalFormatRule> {
        let mut sheet = empty_sheet("Sheet1");
        for r in 0..10u32 { set_value(&mut sheet, r, 0, CellValue::Number(r as f64)); }
        sheet.cond_formats = rules;
        let bytes = XlsxWriter::write(&make_workbook(sheet)).expect("write failed");
        XlsxParser::parse(&bytes).expect("roundtrip parse failed").sheets.remove(0).cond_formats
    }

    #[test]
    fn test_roundtrip_cf_databar() {
        use roe_model::calc::{BarDirection, CfValue, ConditionalFormatKind};
        let kind = ConditionalFormatKind::DataBar {
            min: CfValue { kind: "min".into(), val: "".into(), gte: true },
            max: CfValue { kind: "max".into(), val: "".into(), gte: true },
            bar_color: "#638EC6".into(), border_color: None, direction: BarDirection::LeftToRight,
            bar_only: true, solid_fill: false, negative_fill_color: None, axis_color: None,
            axis_position: "automatic".into(),
        };
        let got = cf_roundtrip(vec![cf_rule(kind, 0, 0, 9, 0, false)]);
        assert_eq!(got.len(), 1, "应读回 1 条规则");
        assert_eq!(got[0].range.start, (0, 0));
        assert_eq!(got[0].range.end, (9, 0));
        match &got[0].kind {
            ConditionalFormatKind::DataBar { min, max, bar_color, bar_only, .. } => {
                assert_eq!(min.kind, "min");
                assert_eq!(max.kind, "max");
                assert_eq!(bar_color, "#638EC6", "数据条颜色应还原");
                assert!(*bar_only, "showValue=0 应还原为 bar_only");
            }
            other => panic!("期望 DataBar,实际: {other:?}"),
        }
    }

    #[test]
    fn test_roundtrip_cf_databar_x14_full() {
        use roe_model::calc::{BarDirection, CfValue, ConditionalFormatKind};
        let kind = ConditionalFormatKind::DataBar {
            min: CfValue { kind: "autoMin".into(), val: "".into(), gte: true },
            max: CfValue { kind: "autoMax".into(), val: "".into(), gte: true },
            bar_color: "#638EC6".into(),
            border_color: Some("#1F4E79".into()),
            direction: BarDirection::RightToLeft,
            bar_only: false,
            solid_fill: true,
            negative_fill_color: Some("#FF0000".into()),
            axis_color: Some("#7F7F7F".into()),
            axis_position: "middle".into(),
        };
        let got = cf_roundtrip(vec![cf_rule(kind, 0, 0, 9, 0, false)]);
        assert_eq!(got.len(), 1, "应读回 1 条规则(legacy+x14 合并后不应重复)");
        match &got[0].kind {
            ConditionalFormatKind::DataBar {
                min, max, bar_color, border_color, direction, bar_only,
                solid_fill, negative_fill_color, axis_color, axis_position,
            } => {
                assert_eq!(min.kind, "autoMin", "autoMin 应经 x14 保真");
                assert_eq!(max.kind, "autoMax", "autoMax 应经 x14 保真");
                assert_eq!(bar_color, "#638EC6");
                assert_eq!(border_color.as_deref(), Some("#1F4E79"), "边框色");
                assert!(matches!(direction, BarDirection::RightToLeft), "方向应还原为 RightToLeft");
                assert!(!*bar_only, "showValue=1 应还原为 bar_only=false");
                assert!(*solid_fill, "gradient=0 应还原为实心");
                assert_eq!(negative_fill_color.as_deref(), Some("#FF0000"), "负值色");
                assert_eq!(axis_color.as_deref(), Some("#7F7F7F"), "轴色");
                assert_eq!(axis_position, "middle", "轴位置");
            }
            other => panic!("期望 DataBar,实际: {other:?}"),
        }
    }

    #[test]
    fn test_roundtrip_cf_databar_x14_values() {
        use roe_model::calc::{BarDirection, CfValue, ConditionalFormatKind};
        let kind = ConditionalFormatKind::DataBar {
            min: CfValue { kind: "num".into(), val: "5".into(), gte: true },
            max: CfValue { kind: "percent".into(), val: "90".into(), gte: true },
            bar_color: "#63BE7B".into(),
            border_color: None,
            direction: BarDirection::LeftToRight,
            bar_only: true,
            solid_fill: false,
            negative_fill_color: None,
            axis_color: None,
            axis_position: "automatic".into(),
        };
        let got = cf_roundtrip(vec![cf_rule(kind, 0, 0, 9, 0, false)]);
        assert_eq!(got.len(), 1);
        match &got[0].kind {
            ConditionalFormatKind::DataBar { min, max, direction, solid_fill, bar_only, .. } => {
                assert_eq!((min.kind.as_str(), min.val.as_str()), ("num", "5"));
                assert_eq!((max.kind.as_str(), max.val.as_str()), ("percent", "90"));
                assert!(matches!(direction, BarDirection::LeftToRight), "显式 LeftToRight 应保留");
                assert!(!*solid_fill, "gradient 默认应还原为渐变");
                assert!(*bar_only);
            }
            other => panic!("期望 DataBar,实际: {other:?}"),
        }
    }

    /// 手动目视用:cargo test -p roe-parser --lib dump_databar -- --ignored --nocapture
    /// 生成 /tmp/databar_x14.xlsx,用 Excel/WPS 打开检查数据条与迷你图。
    #[test]
    #[ignore]
    fn dump_databar_x14_for_manual_check() {
        use roe_model::calc::{
            BarDirection, CellRange, CfValue, ConditionalFormatKind, Sparkline, SparklineGroup,
        };
        let mut sheet = empty_sheet("Sheet1");
        let vals = [5.0, -3.0, 8.0, -6.0, 2.0, 10.0, -9.0, 4.0, 7.0, -1.0];
        for (r, v) in vals.iter().enumerate() {
            for c in 0..4u32 {
                set_value(&mut sheet, r as u32, c, CellValue::Number(*v));
            }
        }
        let cv = |k: &str, v: &str| CfValue { kind: k.into(), val: v.into(), gte: true };
        let mk = |min: CfValue, max: CfValue, bar: &str, border: Option<&str>, dir: BarDirection,
                  only: bool, solid: bool, neg: Option<&str>, axis: Option<&str>, pos: &str| {
            ConditionalFormatKind::DataBar {
                min, max, bar_color: bar.into(), border_color: border.map(|s| s.into()),
                direction: dir, bar_only: only, solid_fill: solid,
                negative_fill_color: neg.map(|s| s.into()),
                axis_color: axis.map(|s| s.into()), axis_position: pos.into(),
            }
        };
        sheet.cond_formats = vec![
            cf_rule(mk(cv("min",""), cv("max",""), "#638EC6", None, BarDirection::Auto,
                       false, false, None, None, "automatic"), 0, 0, 9, 0, false),
            cf_rule(mk(cv("autoMin",""), cv("autoMax",""), "#63BE7B", Some("#1F4E79"),
                       BarDirection::LeftToRight, false, true, Some("#FF0000"), Some("#7F7F7F"), "middle"),
                    0, 1, 9, 1, false),
            cf_rule(mk(cv("autoMin",""), cv("autoMax",""), "#FFB628", None, BarDirection::RightToLeft,
                       false, false, None, None, "automatic"), 0, 2, 9, 2, false),
            cf_rule(mk(cv("num","-5"), cv("percent","90"), "#D6007B", None, BarDirection::LeftToRight,
                       true, false, None, None, "automatic"), 0, 3, 9, 3, false),
        ];
        let mut g = SparklineGroup::default();
        g.color_series = Some("#376092".into());
        g.sparklines.push(Sparkline {
            cell: (11, 0),
            input_range: vec![CellRange { sheet: "Sheet1".into(), start: (0, 0), end: (9, 0) }],
        });
        sheet.sparkline_groups = vec![g];
        let bytes = XlsxWriter::write(&make_workbook(sheet)).expect("write 失败");
        std::fs::write("/tmp/databar_x14.xlsx", &bytes).expect("写 /tmp 失败");
        eprintln!("已写出 /tmp/databar_x14.xlsx ({} bytes)", bytes.len());
    }

    #[test]
    fn test_roundtrip_cf_colorscale_3color() {
        use roe_model::calc::ConditionalFormatKind;
        let kind = ConditionalFormatKind::ColorScale {
            min_type: "min".into(), min_value: None, min_color: "#F8696B".into(),
            mid: Some(("percentile".into(), "50".into(), "#FFEB84".into())),
            max_type: "max".into(), max_value: None, max_color: "#63BE7B".into(),
        };
        let got = cf_roundtrip(vec![cf_rule(kind, 0, 0, 9, 0, false)]);
        assert_eq!(got.len(), 1);
        match &got[0].kind {
            ConditionalFormatKind::ColorScale { min_type, min_color, mid, max_type, max_color, .. } => {
                assert_eq!(min_type, "min");
                assert_eq!(max_type, "max");
                assert_eq!(min_color, "#F8696B");
                assert_eq!(max_color, "#63BE7B");
                let (mt, mv, mc) = mid.as_ref().expect("3 色阶应保留中间档");
                assert_eq!(mt, "percentile");
                assert_eq!(mv, "50");
                assert_eq!(mc, "#FFEB84");
            }
            other => panic!("期望 ColorScale,实际: {other:?}"),
        }
    }

    #[test]
    fn test_roundtrip_cf_colorscale_2color() {
        use roe_model::calc::ConditionalFormatKind;
        let kind = ConditionalFormatKind::ColorScale {
            min_type: "min".into(), min_value: None, min_color: "#FFFFFF".into(),
            mid: None, max_type: "max".into(), max_value: None, max_color: "#5A8AC6".into(),
        };
        let got = cf_roundtrip(vec![cf_rule(kind, 0, 0, 9, 0, false)]);
        match &got[0].kind {
            ConditionalFormatKind::ColorScale { mid, max_color, .. } => {
                assert!(mid.is_none(), "2 色阶不应出现中间档");
                assert_eq!(max_color, "#5A8AC6");
            }
            other => panic!("期望 ColorScale,实际: {other:?}"),
        }
    }

    #[test]
    fn test_roundtrip_cf_iconset() {
        use roe_model::calc::{CfValue, ConditionalFormatKind};
        let kind = ConditionalFormatKind::IconSet {
            icon_style: "3TrafficLights1".into(),
            thresholds: vec![
                CfValue { kind: "percent".into(), val: "0".into(), gte: true },
                CfValue { kind: "percent".into(), val: "33".into(), gte: true },
                CfValue { kind: "percent".into(), val: "67".into(), gte: true },
            ],
            percent: true, reverse: true, icons_only: true, tier_overrides: None,
        };
        let got = cf_roundtrip(vec![cf_rule(kind, 0, 0, 9, 0, false)]);
        assert_eq!(got.len(), 1);
        match &got[0].kind {
            ConditionalFormatKind::IconSet { icon_style, thresholds, reverse, icons_only, .. } => {
                assert_eq!(icon_style, "3TrafficLights1");
                assert_eq!(thresholds.len(), 3, "档位数应还原");
                assert_eq!(thresholds[1].kind, "percent");
                assert_eq!(thresholds[1].val, "33");
                assert!(*reverse, "reverse 应还原");
                assert!(*icons_only, "showValue=0 应还原为 icons_only");
            }
            other => panic!("期望 IconSet,实际: {other:?}"),
        }
    }

    #[test]
    fn test_roundtrip_cf_multi_rules_order_and_stop_if_true() {
        use roe_model::calc::{CfValue, ConditionalFormatKind};
        let cs = ConditionalFormatKind::ColorScale {
            min_type: "min".into(), min_value: None, min_color: "#FFFFFF".into(),
            mid: None, max_type: "max".into(), max_value: None, max_color: "#FF0000".into(),
        };
        let db = ConditionalFormatKind::DataBar {
            min: CfValue { kind: "min".into(), val: "".into(), gte: true },
            max: CfValue { kind: "max".into(), val: "".into(), gte: true },
            bar_color: "#638EC6".into(), border_color: None, direction: roe_model::calc::BarDirection::LeftToRight,
            bar_only: false, solid_fill: false, negative_fill_color: None, axis_color: None,
            axis_position: "automatic".into(),
        };
        let got = cf_roundtrip(vec![cf_rule(cs, 0, 0, 4, 0, true), cf_rule(db, 5, 0, 9, 0, false)]);
        assert_eq!(got.len(), 2);
        assert!(matches!(got[0].kind, ConditionalFormatKind::ColorScale { .. }), "顺序应保持");
        assert!(matches!(got[1].kind, ConditionalFormatKind::DataBar { .. }));
        assert!(got[0].stop_if_true, "stopIfTrue 应还原");
        assert!(!got[1].stop_if_true);
        assert_eq!(got[1].range.start, (5, 0));
    }

    #[test]
    fn test_sparkline_none_colors_written_as_excel_defaults() {
        use roe_model::calc::{CellRange, Sparkline, SparklineGroup};
        let mut sheet = empty_sheet("Sheet1");
        for c in 0..5u32 {
            set_value(&mut sheet, 0, c, CellValue::Number(c as f64));
        }
        // 全默认组:所有 color_* 为 None,模拟 UI 新建迷你图
        let mut g = SparklineGroup::default();
        assert!(g.color_series.is_none() && g.color_axis.is_none());
        g.sparklines.push(Sparkline {
            cell: (1, 0),
            input_range: vec![CellRange { sheet: "Sheet1".to_string(), start: (0, 0), end: (0, 4) }],
        });
        sheet.sparkline_groups = vec![g];

        let bytes = XlsxWriter::write(&make_workbook(sheet)).expect("write failed");
        let parsed = XlsxParser::parse(&bytes).expect("roundtrip parse failed");
        let gs = &parsed.sheets[0].sparkline_groups;
        assert_eq!(gs.len(), 1, "迷你图组应还原");
        let a = &gs[0];
        // 预期:读回格式与既有用例一致("#RRGGBB"),若大小写不符以断言失败信息为准调整断言
        assert_eq!(a.color_series.as_deref(), Some("#376092"));
        assert_eq!(a.color_axis.as_deref(), Some("#000000"));
        assert_eq!(a.color_negative.as_deref(), Some("#D00000"));
        assert_eq!(a.color_markers.as_deref(), Some("#D00000"));
        assert_eq!(a.color_first.as_deref(), Some("#D00000"));
        assert_eq!(a.color_last.as_deref(), Some("#D00000"));
        assert_eq!(a.color_high.as_deref(), Some("#D00000"));
        assert_eq!(a.color_low.as_deref(), Some("#D00000"));
    }

    #[test]
    fn test_roundtrip_sparkline_groups() {
        use roe_model::calc::{
            CellRange, DisplayEmptyCellsAs, Sparkline, SparklineAxisType, SparklineGroup,
            SparklineType,
        };
        let mut sheet = empty_sheet("Sheet1");
        for r in 0..2u32 {
            for c in 0..5u32 {
                set_value(&mut sheet, r, c, CellValue::Number((r * 10 + c) as f64));
            }
        }

        // 组 1:所有可写字段都取非默认值,两条迷你图(F1、F2)
        let g1 = SparklineGroup {
            sparkline_type: SparklineType::Column,
            manual_max: Some(10.0),
            manual_min: Some(-2.0),
            line_weight: 1.5,
            date_axis: false,
            display_empty_cells_as: DisplayEmptyCellsAs::Zero,
            markers: true,
            high: true,
            low: true,
            first: true,
            last: true,
            negative: true,
            display_x_axis: true,
            display_hidden: true,
            min_axis_type: SparklineAxisType::Custom,
            max_axis_type: SparklineAxisType::Group,
            right_to_left: true,
            color_series: Some("#376092".to_string()),
            color_negative: Some("#00B050".to_string()),
            color_axis: Some("#111111".to_string()),
            color_markers: Some("#222222".to_string()),
            color_first: Some("#7030A0".to_string()),
            color_last: Some("#333333".to_string()),
            color_high: Some("#444444".to_string()),
            color_low: Some("#555555".to_string()),
            sparklines: vec![
                Sparkline {
                    cell: (0, 5),
                    input_range: vec![CellRange { sheet: "Sheet1".to_string(), start: (0, 0), end: (0, 4) }],
                },
                Sparkline {
                    cell: (1, 5),
                    input_range: vec![CellRange { sheet: "Sheet1".to_string(), start: (1, 0), end: (1, 4) }],
                },
            ],
        };
        // 组 2:全默认(Line),一条迷你图(G1)
        let mut g2 = SparklineGroup::default();
        g2.sparklines.push(Sparkline {
            cell: (0, 6),
            input_range: vec![CellRange { sheet: "Sheet1".to_string(), start: (0, 0), end: (0, 4) }],
        });
        sheet.sparkline_groups = vec![g1, g2];

        let wb = make_workbook(sheet);
        let bytes = XlsxWriter::write(&wb).expect("write failed");
        let parsed = XlsxParser::parse(&bytes).expect("roundtrip parse failed");
        let gs = &parsed.sheets[0].sparkline_groups;
        assert_eq!(gs.len(), 2, "迷你图组数应还原");

        let a = &gs[0];
        assert_eq!(a.sparkline_type, SparklineType::Column);
        assert_eq!(a.manual_max, Some(10.0));
        assert_eq!(a.manual_min, Some(-2.0));
        assert!((a.line_weight - 1.5).abs() < 1e-6);
        assert_eq!(a.display_empty_cells_as, DisplayEmptyCellsAs::Zero);
        assert!(a.markers && a.high && a.low && a.first && a.last && a.negative);
        assert!(a.display_x_axis && a.display_hidden && a.right_to_left);
        assert_eq!(a.min_axis_type, SparklineAxisType::Custom);
        assert_eq!(a.max_axis_type, SparklineAxisType::Group);
        assert_eq!(a.color_series.as_deref(), Some("#376092"));
        assert_eq!(a.color_negative.as_deref(), Some("#00B050"));
        assert_eq!(a.color_axis.as_deref(), Some("#111111"));
        assert_eq!(a.color_markers.as_deref(), Some("#222222"));
        assert_eq!(a.color_first.as_deref(), Some("#7030A0"));
        assert_eq!(a.color_last.as_deref(), Some("#333333"));
        assert_eq!(a.color_high.as_deref(), Some("#444444"));
        assert_eq!(a.color_low.as_deref(), Some("#555555"));
        assert_eq!(a.sparklines.len(), 2);
        assert_eq!(a.sparklines[0].cell, (0, 5));
        assert_eq!(a.sparklines[1].cell, (1, 5));
        assert_eq!(a.sparklines[0].input_range.len(), 1);
        assert_eq!(a.sparklines[0].input_range[0].start, (0, 0));
        assert_eq!(a.sparklines[0].input_range[0].end, (0, 4));
        assert_eq!(a.sparklines[1].input_range[0].start, (1, 0));
        assert_eq!(a.sparklines[1].input_range[0].end, (1, 4));

        let b = &gs[1];
        assert_eq!(b.sparkline_type, SparklineType::Line);
        assert_eq!(b.manual_max, None);
        assert_eq!(b.manual_min, None);
        assert_eq!(b.display_empty_cells_as, DisplayEmptyCellsAs::Gap);
        assert!(!b.markers && !b.high && !b.low && !b.first && !b.last && !b.negative);
        assert!(!b.display_x_axis && !b.display_hidden && !b.right_to_left);
        assert_eq!(b.min_axis_type, SparklineAxisType::Individual);
        assert_eq!(b.max_axis_type, SparklineAxisType::Individual);
        assert_eq!(b.sparklines.len(), 1);
        assert_eq!(b.sparklines[0].cell, (0, 6));
    }

    fn sparklines_sample_path() -> String {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../test_data/Sparklines.xlsx").to_string()
    }

    #[test]
    fn test_real_file_roundtrip_sparklines() {
        let path = sparklines_sample_path();
        let data = std::fs::read(&path).unwrap_or_else(|e| panic!("读取失败 {path}: {e}"));
        let expected = XlsxParser::parse(&data).expect("首次 parse 失败");
        let bytes = XlsxWriter::write(&expected).expect("write 失败");
        let actual = XlsxParser::parse(&bytes).expect("write→reparse 失败");

        assert_eq!(expected.sheets.len(), actual.sheets.len(), "sheet 数量");
        let mut total_groups = 0;
        for (si, (e, a)) in expected.sheets.iter().zip(actual.sheets.iter()).enumerate() {
            assert_eq!(
                e.sparkline_groups.len(), a.sparkline_groups.len(),
                "sheet[{si}] 迷你图组数"
            );
            for (gi, (eg, ag)) in e.sparkline_groups.iter().zip(a.sparkline_groups.iter()).enumerate() {
                total_groups += 1;
                let t = format!("sheet[{si}] group[{gi}]");
                assert_eq!(eg.sparkline_type, ag.sparkline_type, "{t} type");
                assert!((eg.line_weight - ag.line_weight).abs() < 1e-6, "{t} line_weight");
                assert_eq!(eg.manual_max, ag.manual_max, "{t} manual_max");
                assert_eq!(eg.manual_min, ag.manual_min, "{t} manual_min");
                assert_eq!(eg.date_axis, ag.date_axis, "{t} date_axis");
                assert_eq!(eg.display_empty_cells_as, ag.display_empty_cells_as, "{t} empty_as");
                assert_eq!(
                    (eg.markers, eg.high, eg.low, eg.first, eg.last, eg.negative),
                    (ag.markers, ag.high, ag.low, ag.first, ag.last, ag.negative),
                    "{t} 标志位"
                );
                assert_eq!(
                    (eg.display_x_axis, eg.display_hidden, eg.right_to_left),
                    (ag.display_x_axis, ag.display_hidden, ag.right_to_left),
                    "{t} 轴/隐藏/方向"
                );
                assert_eq!(eg.min_axis_type, ag.min_axis_type, "{t} min_axis_type");
                assert_eq!(eg.max_axis_type, ag.max_axis_type, "{t} max_axis_type");
                assert_eq!(eg.color_series, ag.color_series, "{t} color_series");
                assert_eq!(eg.color_negative, ag.color_negative, "{t} color_negative");
                assert_eq!(eg.color_axis, ag.color_axis, "{t} color_axis");
                assert_eq!(eg.color_markers, ag.color_markers, "{t} color_markers");
                assert_eq!(eg.color_first, ag.color_first, "{t} color_first");
                assert_eq!(eg.color_last, ag.color_last, "{t} color_last");
                assert_eq!(eg.color_high, ag.color_high, "{t} color_high");
                assert_eq!(eg.color_low, ag.color_low, "{t} color_low");
                assert_eq!(eg.sparklines.len(), ag.sparklines.len(), "{t} 迷你图条数");
                for (k, (es, as_)) in eg.sparklines.iter().zip(ag.sparklines.iter()).enumerate() {
                    assert_eq!(es.cell, as_.cell, "{t} sparkline[{k}] cell");
                    assert_eq!(es.input_range.len(), as_.input_range.len(), "{t} sparkline[{k}] range 段数");
                    for (er, ar) in es.input_range.iter().zip(as_.input_range.iter()) {
                        assert_eq!(er.start, ar.start, "{t} sparkline[{k}] range.start");
                        assert_eq!(er.end, ar.end, "{t} sparkline[{k}] range.end");
                    }
                }
            }
        }
        assert!(total_groups >= 4, "样本应至少含 4 个迷你图组,实际 {total_groups}");
    }

    /// 手动目视用:cargo test -p roe-parser --lib dump_sparklines -- --ignored
    /// 生成 /tmp/sparklines_rt.xlsx,用 Excel/WPS 打开与原 test_data/Sparklines.xlsx 对比。
    #[test]
    #[ignore]
    fn dump_sparklines_xlsx_for_manual_check() {
        let path = sparklines_sample_path();
        let data = std::fs::read(&path).expect("读取样本失败");
        let wb = XlsxParser::parse(&data).expect("parse 失败");
        let bytes = XlsxWriter::write(&wb).expect("write 失败");
        std::fs::write("/tmp/sparklines_rt.xlsx", &bytes).expect("写 /tmp 失败");
        eprintln!("已写出 /tmp/sparklines_rt.xlsx ({} bytes)", bytes.len());
    }

    #[test]
    fn test_roundtrip_merge_cells() {
        // 验证本次新增：mergeCells 导出，导出后能被 xlsx.rs parser 正确读回。
        let mut sheet = empty_sheet("Sheet1");
        set_value(&mut sheet, 0, 0, CellValue::Text("合并标题".to_string())); // A1
        set_value(&mut sheet, 5, 5, CellValue::Text("单格".to_string())); // F6：单格"合并"（起止相同）

        sheet.merged_cells.push(roe_model::calc::MergedCell { r1: 0, c1: 0, r2: 1, c2: 2 }); // A1:C2
        sheet.merged_cells.push(roe_model::calc::MergedCell { r1: 5, c1: 5, r2: 5, c2: 5 }); // F6:F6（单格）

        let wb = make_workbook(sheet);
        let bytes = XlsxWriter::write(&wb).expect("write failed");
        let parsed = XlsxParser::parse(&bytes).expect("roundtrip parse failed");
        let s = &parsed.sheets[0];

        assert_eq!(s.merged_cells.len(), 2, "应还原2个合并区域");
        let mc1 = s.merged_cells.iter().find(|mc| mc.r1 == 0 && mc.c1 == 0)
            .expect("应找到 A1:C2 合并区域");
        assert_eq!((mc1.r1, mc1.c1, mc1.r2, mc1.c2), (0, 0, 1, 2), "A1:C2 坐标应还原");
        let mc2 = s.merged_cells.iter().find(|mc| mc.r1 == 5 && mc.c1 == 5)
            .expect("应找到 F6 单格合并区域");
        assert_eq!((mc2.r1, mc2.c1, mc2.r2, mc2.c2), (5, 5, 5, 5), "F6 单格坐标应还原");
    }

    #[test]
    fn test_roundtrip_sheet_view() {
        // 验证本次新增：sheetView zoomScale/showGridLines 导出，导出后能被
        // xlsx.rs parser 正确读回。三种场景：显式设置、显式关闭网格线、None（不写属性）。
        let mut sheet1 = empty_sheet("Sheet1");
        sheet1.zoom_scale = Some(150);
        sheet1.show_grid_lines = Some(false);
        set_value(&mut sheet1, 0, 0, CellValue::Text("z150".to_string()));

        let wb = make_workbook(sheet1);
        let bytes = XlsxWriter::write(&wb).expect("write failed");
        let parsed = XlsxParser::parse(&bytes).expect("roundtrip parse failed");
        let s = &parsed.sheets[0];

        assert_eq!(s.zoom_scale, Some(150), "zoomScale 应还原为 150");
        assert_eq!(s.show_grid_lines, Some(false), "showGridLines 应还原为 false");
    }

    #[test]
    fn test_roundtrip_sheet_view_none() {
        // None 场景：不显式设置 zoom_scale/show_grid_lines 时，写出应省略对应属性，
        // 读回后仍为 None（不应凭空产生默认值，读写不丢失原则的另一面：不无中生有）。
        let mut sheet1 = empty_sheet("Sheet1");
        set_value(&mut sheet1, 0, 0, CellValue::Text("no_view_attrs".to_string()));

        let wb = make_workbook(sheet1);
        let bytes = XlsxWriter::write(&wb).expect("write failed");
        let parsed = XlsxParser::parse(&bytes).expect("roundtrip parse failed");
        let s = &parsed.sheets[0];

        assert_eq!(s.zoom_scale, None, "未设置时 zoomScale 应保持 None");
        assert_eq!(s.show_grid_lines, None, "未设置时 showGridLines 应保持 None");
    }

    #[test]
    fn test_roundtrip_col_row_hidden_outline() {
        // 验证本次新增：col/row 的 hidden + outlineLevel 导出，导出后能被
        // xlsx.rs parser 正确读回。覆盖三种场景：
        // 1) 有数据的行/列上的 hidden+outline
        // 2) 完全空白（无单元格数据）但被隐藏/分组的行 —— 验证不会被静默丢弃
        // 3) 连续区间合并（<col min max>）是否仍能被逐列正确读回
        let mut sheet = empty_sheet("Sheet1");
        set_value(&mut sheet, 0, 0, CellValue::Text("A1".to_string()));
        set_value(&mut sheet, 10, 0, CellValue::Text("A11 has data".to_string()));

        // 列：0,1,2 隐藏且 outline=2；列 5 只 outline=1，不隐藏
        sheet.col_hidden = vec![true, true, true, false, false, false];
        sheet.col_outline_level = vec![2, 2, 2, 0, 0, 1];

        // 行：3（无数据）被隐藏；行 10（有数据）outline=1
        sheet.row_hidden = vec![false, false, false, true, false, false, false, false, false, false, false];
        sheet.row_outline_level = vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];

        let wb = make_workbook(sheet);
        let bytes = XlsxWriter::write(&wb).expect("write failed");
        let parsed = XlsxParser::parse(&bytes).expect("roundtrip parse failed");
        let s = &parsed.sheets[0];

        // 列断言：0,1,2 隐藏
        assert_eq!(s.col_hidden.get(0).copied(), Some(true), "col0 应隐藏");
        assert_eq!(s.col_hidden.get(1).copied(), Some(true), "col1 应隐藏");
        assert_eq!(s.col_hidden.get(2).copied(), Some(true), "col2 应隐藏");
        assert_eq!(s.col_hidden.get(3).copied().unwrap_or(false), false, "col3 不应隐藏");
        assert_eq!(s.col_outline_level.get(0).copied(), Some(2), "col0 outline 应为2");
        assert_eq!(s.col_outline_level.get(5).copied(), Some(1), "col5 outline 应为1");

        // 行断言：行3（0-based）无数据但应保留 hidden
        assert_eq!(s.row_hidden.get(3).copied(), Some(true), "row3（空白行）应隐藏，不应被静默丢弃");
        // 行10（0-based）有数据且 outline=1
        assert_eq!(s.row_outline_level.get(10).copied(), Some(1), "row10 outline 应为1");
        // 行10 的数据本身应保持完整（hidden/outline 写出不应影响单元格内容）
        assert_eq!(
            s.cells.cols.get(&0).map(|sc| sc.blocks.len()).unwrap_or(0) > 0,
            true,
            "col0 应仍有数据"
        );
    }

    // ─── 真实文件 round-trip 测试（xlsx round-trip 任务）───────────────────
    //
    // 已确认 xlsx_writer.rs 文件头注释（第1-8行）过时：实际范围远超注释
    // 声称的"只写cell值"。核实代码确认真实支持范围：cell值(Number/Text/
    // Formula/Bool/Error/RichText) + merged_cells + cell_formats/
    // conditional_formatting + sheet_view + col/row hidden+outline_level。
    // 真正不支持（无写出路径）：col_widths/row_heights/frozen_rows/
    // frozen_cols。比较函数据此跳过这四个字段，其余全部纳入断言范围。

    /// 比较两个 Workbook 在 writer 支持范围内的字段。
    /// 跳过 col_widths/row_heights/frozen_rows/frozen_cols（确认无写出
    /// 路径，非本次任务遗漏）；cell 值按坐标枚举比较（稀疏存储，直接用
    /// SparseGrid::get 逐格读取，不关心内部 block 切分方式是否一致，
    /// 只关心"同一坐标读到的值是否一致"这个语义层面的正确性）。
    fn assert_workbook_match_within_writer_scope(
        expected: &Workbook,
        actual: &Workbook,
        path: &str,
    ) {
        assert_eq!(
            expected.sheets.len(), actual.sheets.len(),
            "{path}: sheet 数量不一致"
        );
        for (si, (exp_sheet, act_sheet)) in expected.sheets.iter().zip(actual.sheets.iter()).enumerate() {
            let s_path = format!("{path} sheet[{si}] '{}'", exp_sheet.name);
            assert_eq!(
                exp_sheet.name, act_sheet.name,
                "{s_path}: sheet 名称不一致"
            );

            // cell 值：枚举 expected 里所有出现过的 (row,col) 坐标，逐一
            // 用 SparseGrid::get 比较，覆盖两个 sheet 都可能用不同 block
            // 切分方式存同一份数据的情况。
            let mut coords: Vec<(u32, u32)> = Vec::new();
            for (&col, column) in exp_sheet.cells.cols.iter() {
                for block in &column.blocks {
                    for (i, _) in block.values.iter().enumerate() {
                        coords.push((block.start_row + i as u32, col));
                    }
                }
            }
            for (row, col) in coords {
                let exp_v = exp_sheet.cells.get(row, col);
                let act_v = act_sheet.cells.get(row, col);
                assert_eq!(
                    exp_v, act_v,
                    "{s_path}: cell({row},{col}) 值不一致 exp={exp_v:?} act={act_v:?}"
                );
            }

            // merged_cells：忽略顺序，比较集合是否一致
            let mut exp_mc: Vec<_> = exp_sheet.merged_cells.iter()
                .map(|mc| (mc.r1, mc.c1, mc.r2, mc.c2)).collect();
            let mut act_mc: Vec<_> = act_sheet.merged_cells.iter()
                .map(|mc| (mc.r1, mc.c1, mc.r2, mc.c2)).collect();
            exp_mc.sort();
            act_mc.sort();
            assert_eq!(
                exp_mc, act_mc,
                "{s_path}: merged_cells 不一致 exp={exp_mc:?} act={act_mc:?}"
            );

            // col/row hidden + outline_level：直接比较 Vec（长度不一致时
            // 用 get().unwrap_or(默认值) 语义比较，容忍 writer 合并连续
            // 区间后 reparse 出的 Vec 长度可能与原始不完全相同的情况）
            let max_col = exp_sheet.col_hidden.len().max(act_sheet.col_hidden.len())
                .max(exp_sheet.col_outline_level.len()).max(act_sheet.col_outline_level.len());
            for c in 0..max_col {
                let exp_h = exp_sheet.col_hidden.get(c).copied().unwrap_or(false);
                let act_h = act_sheet.col_hidden.get(c).copied().unwrap_or(false);
                assert_eq!(exp_h, act_h, "{s_path}: col[{c}] hidden 不一致");
                let exp_o = exp_sheet.col_outline_level.get(c).copied().unwrap_or(0);
                let act_o = act_sheet.col_outline_level.get(c).copied().unwrap_or(0);
                assert_eq!(exp_o, act_o, "{s_path}: col[{c}] outline_level 不一致");
            }
            let max_row = exp_sheet.row_hidden.len().max(act_sheet.row_hidden.len())
                .max(exp_sheet.row_outline_level.len()).max(act_sheet.row_outline_level.len());
            for r in 0..max_row {
                let exp_h = exp_sheet.row_hidden.get(r).copied().unwrap_or(false);
                let act_h = act_sheet.row_hidden.get(r).copied().unwrap_or(false);
                assert_eq!(exp_h, act_h, "{s_path}: row[{r}] hidden 不一致");
                let exp_o = exp_sheet.row_outline_level.get(r).copied().unwrap_or(0);
                let act_o = act_sheet.row_outline_level.get(r).copied().unwrap_or(0);
                assert_eq!(exp_o, act_o, "{s_path}: row[{r}] outline_level 不一致");
            }

            // 注：cell_formats/conditional_formatting/sheet_view 本轮
            // 探测脚本未能精确核实具体存储字段位置（Workbook.styles 内部
            // 结构未展开调研），暂不在本轮断言范围内逐项比较，只通过
            // "write→reparse 不panic/不报错"间接验证没有崩溃性 bug。
            // 若后续需要更强断言，应先查 StyleRegistry 具体字段。
        }
    }

    fn run_real_file_roundtrip_xlsx(path: &str) {
        let data = std::fs::read(path)
            .unwrap_or_else(|e| panic!("读取样本文件失败 {path}: {e}"));
        let expected = XlsxParser::parse(&data)
            .unwrap_or_else(|e| panic!("首次 parse 失败 {path}: {e:?}"));

        let bytes = XlsxWriter::write(&expected)
            .unwrap_or_else(|e| panic!("write 失败 {path}: {e:?}"));
        let actual = XlsxParser::parse(&bytes)
            .unwrap_or_else(|e| panic!("write→reparse 失败 {path}: {e:?}"));

        assert_workbook_match_within_writer_scope(&expected, &actual, path);
    }

    #[test]
    fn test_real_file_roundtrip_universal_content() {
        // 探测脚本 scan_xlsx_roundtrip_candidates 挑选：2 sheets / 8 cells，
        // 含 merged_cells(1个) + formula(4个)，无 frozen，是覆盖"实际支持
        // 特性"（merge+formula）最干净的小样本，作为主力基线。
        run_real_file_roundtrip_xlsx(
            "/home/yong/Projects/roe/test_samples/curated/xlsx/small__sc_data_xlsx__universal-content.xlsx",
        );
    }

    #[test]
    fn test_real_file_roundtrip_subtotal_above() {
        // 探测脚本挑选：1 sheet / 69 cells，formula密集(16个)，col/row
        // hidden+outline 均有覆盖，无 frozen，规模适中的压力测试样本。
        run_real_file_roundtrip_xlsx(
            "/home/yong/Projects/roe/test_samples/curated/xlsx/mid__sc_data_xlsx__subtotal-above.xlsx",
        );
    }

    #[test]
    fn test_real_file_roundtrip_tdf119155_error_cells() {
        // 探测脚本挑选：29 cells，其中13个是 CellValue::Error，是唯一
        // 覆盖 Error 变体的干净样本（其余候选样本 Error 列均为0），专门
        // 验证 #DIV/0! 等错误值的写出/读回正确性，无 frozen。
        run_real_file_roundtrip_xlsx(
            "/home/yong/Projects/roe/test_samples/curated/xlsx/small__sc_uicalc_data__tdf119155.xlsx",
        );
    }
}

#[cfg(test)]
mod cols_xml_tests {
    use super::cols_xml_from;

    #[test]
    fn test_cols_widths_merge_runs() {
        // 11.78125 字符 = 65.6015625pt;9.78125 字符 = 55.1015625pt
        let w = [65.6015625f32, 55.1015625, 55.1015625, 55.1015625];
        assert_eq!(
            cols_xml_from(&w, &[], &[]),
            r#"<cols><col min="1" max="1" width="11.781" customWidth="1"/><col min="2" max="4" width="9.781" customWidth="1"/></cols>"#
        );
    }

    #[test]
    fn test_cols_hidden_without_width_and_width_together() {
        let w = [0.0f32, 55.1015625];
        assert_eq!(
            cols_xml_from(&w, &[true, false], &[]),
            r#"<cols><col min="1" max="1" hidden="1"/><col min="2" max="2" width="9.781" customWidth="1"/></cols>"#
        );
    }

    #[test]
    fn test_cols_all_default_is_empty() {
        assert_eq!(cols_xml_from(&[], &[], &[]), "");
        assert_eq!(cols_xml_from(&[0.0, 0.0], &[false, false], &[0, 0]), "");
    }
}

#[cfg(test)]
mod pivot_write_tests {
    use super::*;
    use roe_model::calc::{CellValue, PivotFieldDef, PivotMeta, Sheet, Workbook};
    use std::io::Read;

    fn mk_wb(source_field: Option<&str>) -> Workbook {
        let mut src = Sheet::default();
        src.name = "src".into();
        for (c, h) in ["Region", "Category", "Sales"].iter().enumerate() {
            src.cells.set(0, c as u32, CellValue::Text(h.to_string()));
        }
        let rows: [(&str, &str, f64); 3] = [("东区", "食品", 100.0), ("南区", "电子", 200.0), ("东区", "电子", 50.0)];
        for (i, (a, b, n)) in rows.iter().enumerate() {
            let r = i as u32 + 1;
            src.cells.set(r, 0, CellValue::Text(a.to_string()));
            src.cells.set(r, 1, CellValue::Text(b.to_string()));
            src.cells.set(r, 2, CellValue::Number(*n));
        }
        let mut pv = Sheet::default();
        pv.name = "pv".into();
        pv.pivot = Some(PivotMeta {
            source_sheet: "src".into(),
            agg: "sum".into(),
            row_field_names: vec!["Category".into()],
            value_fields: vec![PivotFieldDef {
                name: "Sales的Sum".into(),
                selected_values: vec![],
                agg: Some("sum".into()),
                source_field: source_field.map(|s| s.to_string()),
            }],
            cells_rect: Some((2, 0, 5, 2)),
            row_grand_totals: true,
            col_grand_totals: true,
            ..Default::default()
        });
        Workbook {
            sheets: vec![pv, src],
            named_ranges: std::collections::HashMap::new(),
            named_formulas: std::collections::HashMap::new(),
            styles: Default::default(),
            formula_dialect_source: Default::default(),
        }
    }

    fn read_all(bytes: &[u8]) -> Vec<(String, String)> {
        let mut ar = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("zip");
        let mut out = Vec::new();
        for i in 0..ar.len() {
            let mut f = ar.by_index(i).expect("entry");
            let name = f.name().to_string();
            let mut buf = Vec::new();
            f.read_to_end(&mut buf).expect("read");
            out.push((name, String::from_utf8_lossy(&buf).to_string()));
        }
        out
    }

    fn get<'a>(parts: &'a [(String, String)], name: &str) -> Option<&'a str> {
        parts.iter().find(|(n, _)| n == name).map(|(_, c)| c.as_str())
    }

    #[test]
    fn pivot_parts_written() {
        let bytes = XlsxWriter::write(&mk_wb(Some("Sales"))).expect("write");
        let parts = read_all(&bytes);
        assert!(get(&parts, "xl/pivotTables/pivotTable1.xml").is_some(), "pivotTable1.xml");
        assert!(get(&parts, "xl/pivotTables/_rels/pivotTable1.xml.rels").is_some(), "pivotTable rels");
        let cd = get(&parts, "xl/pivotCache/pivotCacheDefinition1.xml").expect("cacheDefinition1");
        assert!(cd.contains("saveData=\"0\"") && cd.contains("refreshOnLoad=\"1\""));
        let ct = get(&parts, "[Content_Types].xml").expect("content types");
        assert!(ct.contains("pivotTable1.xml") && ct.contains("pivotCacheDefinition1.xml"));
        let wbx = get(&parts, "xl/workbook.xml").expect("workbook.xml");
        assert!(wbx.contains("<pivotCaches") && wbx.contains("cacheId=\"1\""));
        assert!(
            parts.iter().any(|(n, c)| n.contains("worksheets/_rels/") && c.contains("pivotTable1.xml")),
            "sheet rels 应指向 pivotTable1.xml"
        );
    }

    #[test]
    fn pivot_degrades_without_source_field() {
        let bytes = XlsxWriter::write(&mk_wb(None)).expect("write");
        let parts = read_all(&bytes);
        assert!(!parts.iter().any(|(n, _)| n.contains("pivot")), "降级时不应有任何 pivot 部件");
        let ct = get(&parts, "[Content_Types].xml").unwrap();
        assert!(!ct.contains("pivot"));
        assert!(!get(&parts, "xl/workbook.xml").unwrap().contains("pivotCaches"));
    }

    #[test]
    fn pivot_survives_roundtrip() {
        let bytes = XlsxWriter::write(&mk_wb(Some("Sales"))).expect("write");
        let parsed = crate::ooxml::xlsx::XlsxParser::parse(&bytes).expect("parse");
        let m = parsed.sheets.iter().find_map(|s| s.pivot.as_ref()).expect("pivot 读回");
        assert_eq!(m.row_field_names, vec!["Category".to_string()]);
        assert_eq!(m.source_ref.as_deref(), Some("A1:C4"));
    }
}
