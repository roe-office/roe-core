//! extractModelIndex：从内存模型提取 JSON 拓扑
//! 隐私保护：只含结构信息，不含文档实际内容

use roe_model::calc::Workbook;
use roe_model::writer::{BodyElement, WriterDocument};
use roe_model::impress::Presentation;

/// Writer 文档结构索引
pub fn extract_writer_index(doc: &WriterDocument) -> String {
    let paras: Vec<String> = doc.body.iter().enumerate().filter_map(|(i, e)| {
        match e {
            BodyElement::Paragraph(p) => {
                let style = &p.style.0;
                let is_heading = style.contains("Heading") || style.contains("heading");
                let kind = if is_heading { "heading" } else { "para" };
                Some(format!(
                    r#"{{"idx":{},"type":"{}","style":"{}","run_count":{}}}"#,
                    i, kind, style, p.runs.len()
                ))
            }
            _ => None,
        }
    }).collect();

    format!(
        r#"{{"type":"writer","para_count":{},"paragraphs":[{}],"rhai_api":{{"fns":["paragraph_count()","get_paragraph(idx)","set_paragraph(idx,text)","insert_paragraph(idx,text)","append_paragraph(text)"]}}}}"#,
        doc.body.len(),
        paras.join(",")
    )
}

/// Calc 工作簿结构索引
pub fn extract_calc_index(wb: &Workbook) -> String {
    let sheets: Vec<String> = wb.sheets.iter().enumerate().map(|(i, s)| {
        let col_count = s.cells.cols.len();
        let row_count = s.cells.cols.values()
            .flat_map(|c| c.blocks.iter())
            .map(|b| b.start_row + b.values.len() as u32)
            .max()
            .unwrap_or(0);
        format!(
            r#"{{"idx":{},"name":"{}","approx_rows":{},"approx_cols":{}}}"#,
            i, s.name, row_count, col_count
        )
    }).collect();

    format!(
        r#"{{"type":"calc","sheet_count":{},"sheets":[{}],"rhai_api":{{"fns":["get_sheet_count()","get_sheet_name(idx)","get_cell(sheet,row,col)","set_cell(sheet,row,col,value)","set_formula(sheet,row,col,formula)"]}}}}"#,
        wb.sheets.len(),
        sheets.join(",")
    )
}

/// Impress 演示文稿结构索引
pub fn extract_impress_index(pres: &Presentation) -> String {
    let slides: Vec<String> = pres.slides.iter().enumerate().map(|(i, s)| {
        format!(
            r#"{{"idx":{},"shape_count":{}}}"#,
            i, s.shapes.len()
        )
    }).collect();

    format!(
        r#"{{"type":"impress","slide_count":{},"slides":[{}],"rhai_api":{{"fns":["slide_count()","shape_count(slide)","get_shape_text(slide,shape)","set_shape_text(slide,shape,text)"]}}}}"#,
        pres.slides.len(),
        slides.join(",")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use roe_model::writer::{WriterDocument, DocumentMeta, DocumentColumns, HeaderFooterSet, Paragraph, Run, BodyElement, NumFormat};
        use roe_model::common::StyleRef;
    use roe_model::style::StyleRegistry;
    use roe_model::calc::{Workbook, Sheet, SparseGrid};
    use std::collections::HashMap;

    #[test]
    fn test_writer_index() {
        let doc = WriterDocument {
            page_size: (595.28, 841.89),
            styles: StyleRegistry::default(),
            columns: DocumentColumns::default(),
            sections: Vec::new(),
            comments: Vec::new(),
            body: vec![
                BodyElement::Paragraph(Paragraph {
                    style: StyleRef("Heading1".into()),
                    runs: vec![Run { text: "Title".into(), char_style: None, ..Default::default() }],
                    list_info: None,
                    ..Default::default()
                }),
                BodyElement::Paragraph(Paragraph {
                    style: StyleRef("Default".into()),
                    runs: vec![Run { text: "Body text".into(), char_style: None, ..Default::default() }],
                    list_info: None,
                    ..Default::default()
                }),
            ],
            metadata: DocumentMeta::default(),
            header: HeaderFooterSet::default(),
            footer: HeaderFooterSet::default(),
            title_page: false,
            even_and_odd_headers: false,
            margin_top:    56.69,
            margin_bottom: 56.69,
            margin_left:   70.87,
            margin_right:  70.87,
            footnotes: Vec::new(),
            endnotes: Vec::new(),
            endnote_num_fmt: NumFormat::default(),
            footnote_num_fmt: NumFormat::default(),
        };
        let json = extract_writer_index(&doc);
        assert!(json.contains("\"type\":\"writer\""));
        assert!(json.contains("\"para_count\":2"));
        assert!(json.contains("heading"));
    }

    #[test]
    fn test_calc_index() {
        let wb = Workbook {
            sheets: vec![Sheet {
                col_style: vec![],
                tables: vec![],
                name: "Data".into(),
                cells: SparseGrid::default(),
                col_widths: vec![],
                row_heights: vec![],
                frozen_rows: 0,
                frozen_cols: 0,
                date1904: false,
                zoom_scale: None,
                show_grid_lines: None,
                col_hidden: vec![],
                row_hidden: vec![],
                col_outline_level: vec![],
                row_outline_level: vec![],
                cell_formats:     std::collections::HashMap::new(),
                cond_formats:     vec![],
                data_validations: vec![],
                auto_filter:      None,
                merged_cells:     vec![],
                pivot:            None,
                default_col_width: 0.0,
                default_row_height: 0.0,
                floating_images: vec![],
                floating_shapes: vec![],
                next_object_id: 0,
                floating_charts: vec![],
                floating_stat_charts: vec![],
                sparkline_groups: vec![],
            }],
            named_ranges: HashMap::new(),
            named_formulas: HashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        };
        let json = extract_calc_index(&wb);
        assert!(json.contains("\"type\":\"calc\""));
        assert!(json.contains("\"sheet_count\":1"));
        assert!(json.contains("\"name\":\"Data\""));
    }
}
