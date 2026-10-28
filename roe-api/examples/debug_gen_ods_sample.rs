//! DEVROOT-001 修复后的手工验证：生成一个真实 .ods 文件（含单元格数据
//! + floating_image），供用户在 LibreOffice 里肉眼核对是否合规。
//! 用法: cargo run -p roe-api --example debug_gen_ods_sample -- <输出路径.ods>

use std::collections::HashMap;
use roe_model::calc::{
    Workbook, Sheet, SparseGrid, SparseColumn, CellBlock, CellValue, FloatingImage,
};

// 1x1 红色像素 PNG，base64编码，比测试里的"hello"假数据更接近真实场景，
// 方便在 LibreOffice 里目视确认图片确实被正确渲染出来（不是占位符/损坏图标）。
const RED_PIXEL_PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP4z8DwHwAFAAH/VscvDQAAAABJRU5ErkJggg==";

fn main() {
    let out_path = std::env::args().nth(1)
        .unwrap_or_else(|| "/tmp/devroot001_sample.ods".to_string());

    let mut grid = SparseGrid::default();
    grid.cols.insert(0, SparseColumn {
        blocks: vec![CellBlock {
            start_row: 0,
            values: vec![
                CellValue::Text("DEVROOT-001 验证样本".to_string()),
            ].into(),
        }].into(),
    });
    grid.cols.insert(1, SparseColumn {
        blocks: vec![CellBlock {
            start_row: 0,
            values: vec![
                CellValue::Number(42.0),
            ].into(),
        }].into(),
    });

    let sheet = Sheet {
        col_style: vec![],
        name: "Sheet1".into(),
        cells: grid,
        col_widths: vec![],
        row_heights: vec![],
        frozen_rows: 0,
        frozen_cols: 0,
        cell_formats: HashMap::new(),
        cond_formats: vec![],
        data_validations: vec![],
        auto_filter: None,
        merged_cells: vec![],
        pivot: None,
        floating_images: vec![
            FloatingImage {
                id: 0,
                data_base64: RED_PIXEL_PNG_BASE64.to_string(),
                mime: "image/png".to_string(),
                width_pt: 100.0,
                height_pt: 100.0,
                x_pt: 150.0,
                y_pt: 30.0,
                rot: 0.0,
                hidden: false,
            },
        ],
        ..Default::default()
    };

    let wb = Workbook {
        sheets: vec![sheet],
        named_ranges: HashMap::new(),
        named_formulas: HashMap::new(),
        styles: roe_model::style::StyleRegistry::default(),
        formula_dialect_source: Default::default(),
    };

    let bytes = roe_parser::odf::OdfPackageWriter::write_ods(&wb)
        .expect("write_ods failed");

    std::fs::write(&out_path, &bytes).expect("failed to write output file");
    println!("已写出 {} ({} bytes)", out_path, bytes.len());

    // 立刻自我 roundtrip 校验一遍，确保给用户的文件本身没问题
    match roe_parser::odf::OdfPackageParser::parse(&bytes) {
        Ok(roe_parser::RoeDocument::Calc(wb2)) => {
            println!("roundtrip校验: sheets={}, floating_images={}",
                wb2.sheets.len(), wb2.sheets[0].floating_images.len());
        }
        Ok(other) => println!("roundtrip校验: 意外类型 {:?}", other),
        Err(e) => println!("roundtrip校验失败: {:?}", e),
    }
}
