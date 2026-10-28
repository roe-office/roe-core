// 临时性能实测,用完删除,不进提交
use roe_calc::CalcEngine;
use roe_model::calc::{CellValue, Sheet, SparseGrid, Workbook};
use std::collections::HashMap;
use std::time::Instant;

fn new_wb() -> Workbook {
    Workbook {
        sheets: vec![Sheet {
            col_style: vec![],
            tables: vec![],
            name: "Sheet1".into(),
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
            cell_formats: HashMap::new(),
            cond_formats: vec![],
            data_validations: vec![],
            auto_filter: None,
            merged_cells: vec![],
            pivot: None,
            floating_images: vec![],
            floating_charts: vec![],
            floating_stat_charts: vec![],
            floating_shapes: vec![],
            next_object_id: 0,
            default_col_width: 0.0,
            default_row_height: 0.0,
            sparkline_groups: vec![],
        }],
        named_ranges: HashMap::new(),
        named_formulas: HashMap::new(),
        styles: roe_model::style::StyleRegistry::default(),
        formula_dialect_source: Default::default(),
    }
}

fn run() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(|s| s.as_str()).unwrap_or("indep");
    let n: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1000);

    let mut wb = new_wb();
    let t = Instant::now();
    for r in 0..n {
        wb.sheets[0].cells.set(r, 0, CellValue::Number((r + 1) as f64));
        let f = if mode == "chain" {
            if r == 0 { "=A1+1".to_string() } else { format!("=B{}+1", r) }
        } else {
            format!("=A{}*2", r + 1)
        };
        wb.sheets[0].cells.set(
            r,
            1,
            CellValue::Formula { source: f, cached: Box::new(CellValue::Empty) },
        );
    }
    let build_ms = t.elapsed().as_secs_f64() * 1000.0;

    let dirty: Vec<(usize, u32, u32)> = (0..n).map(|r| (0usize, r, 1u32)).collect();
    let mut engine = CalcEngine::new();

    let t = Instant::now();
    let applied = engine.recalc_and_apply(&mut wb, &dirty);
    let full_ms = t.elapsed().as_secs_f64() * 1000.0;
    let last = wb.sheets[0].cells.get(n - 1, 1);

    let mut edit_ms = Vec::new();
    let mut edit_applied = 0;
    for i in 0..2 {
        wb.sheets[0].cells.set(0, 0, CellValue::Number(1000.0 + i as f64));
        let t = Instant::now();
        let a = engine.recalc_and_apply(&mut wb, &[(0usize, 0u32, 0u32)]);
        edit_ms.push(t.elapsed().as_secs_f64() * 1000.0);
        edit_applied = a.len();
    }
    let last2 = wb.sheets[0].cells.get(n - 1, 1);

    println!(
        "mode={} n={} build={:.1}ms full_recalc={:.1}ms applied={} | edit1={:.1}ms edit2={:.1}ms edit_applied={}",
        mode, n, build_ms, full_ms, applied.len(), edit_ms[0], edit_ms[1], edit_applied
    );
    println!("  last_after_full={:?}  last_after_edit={:?}", last, last2);
}

fn main() {
    match std::env::var("STACK_MB").ok().and_then(|v| v.parse::<usize>().ok()) {
        Some(mb) => {
            std::thread::Builder::new()
                .stack_size(mb * 1024 * 1024)
                .spawn(run)
                .unwrap()
                .join()
                .unwrap();
        }
        None => run(),
    }
}
