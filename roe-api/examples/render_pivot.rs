use roe_core::tile::{CalcTileRenderer, CalcRenderConfig};
use roe_core::tile_geometry::{col_x_px, row_y_px};
use roe_model::calc::{Workbook, Sheet, SparseGrid, CellValue, SparseColumn, CellBlock};
use roe_polars_bridge::{PivotConfig, pivot_sheet_into_workbook_v2};
use std::collections::HashMap;

fn make_col(grid: &mut SparseGrid, col_idx: u32, values: Vec<CellValue>) {
    use smallvec::smallvec;
    grid.cols.insert(col_idx, SparseColumn {
        blocks: smallvec![CellBlock { start_row: 0, values: values.into() }],
    });
}

fn main() {
    let data: Vec<(&str, &str, f64)> = vec![
        ("East", "Apple",  100.0),
        ("East", "Banana", 200.0),
        ("West", "Apple",  150.0),
        ("West", "Banana", 250.0),
        ("East", "Apple",  120.0),
        ("West", "Apple",  180.0),
        ("East", "Banana", 210.0),
        ("West", "Banana", 270.0),
    ];

    let mut grid = SparseGrid::default();
    let mut regions  = vec![CellValue::Text("Region".into())];
    let mut products = vec![CellValue::Text("Product".into())];
    let mut sales    = vec![CellValue::Text("Sales".into())];
    for (r, p, s) in &data {
        regions.push(CellValue::Text(r.to_string()));
        products.push(CellValue::Text(p.to_string()));
        sales.push(CellValue::Number(*s));
    }
    make_col(&mut grid, 0, regions);
    make_col(&mut grid, 1, products);
    make_col(&mut grid, 2, sales);

    let source = Sheet {
        col_style: vec![],
        name: "Source".into(),
        cells: grid,
        col_widths: vec![100.0, 100.0, 100.0],
        row_heights: vec![],
        frozen_rows: 0, frozen_cols: 0,
        cell_formats: HashMap::new(),
        cond_formats: vec![],
        data_validations: vec![],
        auto_filter: None,
        merged_cells: vec![],
        pivot: None,
        ..Default::default()
    };

    let mut wb = Workbook {
        sheets: vec![source],
        named_ranges: HashMap::new(),
        named_formulas: HashMap::new(),
        styles: roe_model::style::StyleRegistry::default(),
        formula_dialect_source: roe_model::calc::FormulaDialectSource::Excel,
    };

    let cfg = PivotConfig {
        rows: vec!["Region".into()],
        cols: vec!["Product".into()],
        values: vec!["Sales".into()],
        agg: "sum".into(),
        filters: HashMap::new(),
    };

    let pivot_idx = pivot_sheet_into_workbook_v2(&mut wb, 0, &cfg, "Pivot".into()).expect("pivot");
    println!("透视 sheet idx={}", pivot_idx);
    println!("PivotMeta: {:?}", wb.sheets[pivot_idx].pivot);

    let mut renderer = CalcTileRenderer::new(&wb);
    renderer.sheet_idx = pivot_idx;
    renderer.config = CalcRenderConfig {
        default_col_width:  100.0,
        default_row_height: 28.0,
        ..CalcRenderConfig::default()
    };

    // v122: 迁移至 render_viewport_into（roe-ext_progress_v114第二步）。
    // 注意：cfg 必须在 renderer.config 被自定义覆盖之后取，
    // 否则 vw/vh 会用默认列宽/行高算错。
    let max_cols = 8u32;
    let max_rows = 8u32;
    let sheet = &wb.sheets[pivot_idx];
    let cfg = &renderer.config;
    let vw = col_x_px(sheet, max_cols, cfg) as u32 + 1;
    let vh = row_y_px(sheet, max_rows, cfg) as u32 + 1;
    let mut buf = vec![0u8; (vw as usize) * (vh as usize) * 4];
    let ok = renderer.render_viewport_into(0.0, 0.0, vw, vh, 100, 0.0, 0.0, &mut buf);
    assert!(ok, "render_viewport_into failed");
    println!("render {}x{}", vw, vh);

    let pixmap = tiny_skia::Pixmap::from_vec(
        buf,
        tiny_skia::IntSize::from_wh(vw, vh).unwrap(),
    ).unwrap();
    pixmap.save_png("/tmp/roe_pivot.png").unwrap();
    println!("→ /tmp/roe_pivot.png");
}
