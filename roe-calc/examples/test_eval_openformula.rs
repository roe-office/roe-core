use roe_model::calc::{Workbook, Sheet, SparseGrid, SparseColumn, CellBlock, CellValue, FormulaDialectSource};
use std::collections::HashMap;
use smallvec::smallvec;

fn main() {
    let mut grid = SparseGrid::default();
    grid.cols.insert(0, SparseColumn { blocks: vec![CellBlock { start_row: 111, values: smallvec![CellValue::Number(5.0)] }].into() });
    grid.cols.insert(1, SparseColumn { blocks: vec![CellBlock { start_row: 111, values: smallvec![CellValue::Number(5.0)] }].into() });

    let sheet = Sheet {
        col_style: vec![],
        tables: vec![],
        name: "Sheet1".into(),
        cells: grid,
        col_widths: vec![],
        row_heights: vec![],
        frozen_rows: 0,
        frozen_cols: 0,
        zoom_scale: None,
        show_grid_lines: None,
        col_hidden: vec![],
        row_hidden: vec![],
        col_outline_level: vec![],
        row_outline_level: vec![],
        cell_formats: HashMap::new(),
        cond_formats: vec![],
        auto_filter: None,
        merged_cells: vec![],
        pivot: None,
        floating_charts: vec![],
        floating_stat_charts: vec![],
        floating_images: vec![],
        default_col_width: 0.0,
        default_row_height: 0.0,
        data_validations: vec![],
        sparkline_groups: vec![],
        date1904: false,
    };

    let wb = Workbook {
        sheets: vec![sheet],
        named_ranges: HashMap::new(),
        named_formulas: HashMap::new(),
        styles: Default::default(),
        formula_dialect_source: FormulaDialectSource::OpenFormula,
    };

    let result = roe_calc::formula_engine::eval_formula(&wb, "Sheet1", "[.A112]=[.B112]");
    println!("result: {:?}", result);
}
