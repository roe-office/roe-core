// 临时调试脚本：验证透视表筛选字段下拉按钮三种状态（未筛选/单值筛选/多值筛选）
// 的渲染效果，用于ROOT-051修复后的配色方案讨论。用完可删除。
use roe_core::tile::{CalcTileRenderer, CalcRenderConfig};
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
    let data: Vec<(&str, &str, &str, f64)> = vec![
        ("East", "Q1", "Apple",  100.0),
        ("East", "Q1", "Banana", 200.0),
        ("West", "Q2", "Apple",  150.0),
        ("West", "Q2", "Banana", 250.0),
        ("East", "Q1", "Apple",  120.0),
        ("West", "Q2", "Apple",  180.0),
        ("East", "Q1", "Banana", 210.0),
        ("West", "Q2", "Banana", 270.0),
    ];

    let mut grid = SparseGrid::default();
    let mut regions  = vec![CellValue::Text("Region".into())];
    let mut quarters = vec![CellValue::Text("Quarter".into())];
    let mut products = vec![CellValue::Text("Product".into())];
    let mut sales    = vec![CellValue::Text("Sales".into())];
    for (r, q, p, s) in &data {
        regions.push(CellValue::Text(r.to_string()));
        quarters.push(CellValue::Text(q.to_string()));
        products.push(CellValue::Text(p.to_string()));
        sales.push(CellValue::Number(*s));
    }
    make_col(&mut grid, 0, regions);
    make_col(&mut grid, 1, quarters);
    make_col(&mut grid, 2, products);
    make_col(&mut grid, 3, sales);

    let source = Sheet {
        col_style: vec![],
        name: "Source".into(),
        cells: grid,
        col_widths: vec![100.0, 100.0, 100.0, 100.0],
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
        rows: vec!["Product".into()],
        cols: vec![],
        values: vec!["Sales".into()],
        agg: "sum".into(),
        filters: {
            let mut m = HashMap::new();
            m.insert("Region".to_string(), vec![]);
            m.insert("Quarter".to_string(), vec![]);
            m
        },
    };

    let pivot_idx = pivot_sheet_into_workbook_v2(&mut wb, 0, &cfg, "Pivot".into()).expect("pivot");

    // 手动模拟三种筛选状态：Region=未筛选，Quarter=单值筛选
    if let Some(pivot) = wb.sheets[pivot_idx].pivot.as_mut() {
        for ff in pivot.filter_fields.iter_mut() {
            if ff.name == "Quarter" {
                ff.selected_values = vec!["Q1".to_string()];
            }
            // Region 保持 selected_values 为空，即"未筛选/全部"状态
        }
        println!("filter_fields: {:?}", pivot.filter_fields);
    }

    let mut renderer = CalcTileRenderer::new(&wb);
    renderer.sheet_idx = pivot_idx;
    renderer.config = CalcRenderConfig {
        default_col_width:  100.0,
        default_row_height: 28.0,
        ..CalcRenderConfig::default()
    };

    let vw: u32 = 500;
    let vh: u32 = 300;
    let mut buf = vec![0u8; (vw * vh * 4) as usize];
    let ok = renderer.render_viewport_into(0.0, 0.0, vw, vh, 100, 80.0, 20.0, &mut buf);
    assert!(ok, "render_viewport_into failed");

    let pixmap = tiny_skia::Pixmap::from_vec(
        buf,
        tiny_skia::IntSize::from_wh(vw, vh).unwrap(),
    ).unwrap();
    pixmap.save_png("/tmp/debug_filter_btn_states.png").unwrap();
    println!("→ /tmp/debug_filter_btn_states.png");
}
