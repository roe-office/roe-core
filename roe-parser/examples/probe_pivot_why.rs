use roe_model::calc::CellValue;
fn main() {
    let bytes = std::fs::read("/home/yong/pivot_tests/pivot_1row1col.xlsx").unwrap();
    let wb = match roe_parser::RoeDocument::open_xlsx_bytes(&bytes).unwrap() { roe_parser::RoeDocument::Calc(wb) => wb, _ => panic!() };
    for (i, s) in wb.sheets.iter().enumerate() {
        println!("sheet[{i}] name={:?} has_pivot={}", s.name, s.pivot.is_some());
    }
    let pv = wb.sheets.iter().find(|s| s.pivot.is_some()).unwrap();
    let m = pv.pivot.as_ref().unwrap();
    println!("source_sheet={:?} source_ref={:?} cells_rect={:?}", m.source_sheet, m.source_ref, m.cells_rect);
    println!("rows={:?} cols={:?} filters={:?}", m.row_field_names, m.col_field_names, m.filter_fields.iter().map(|f| &f.name).collect::<Vec<_>>());
    for v in &m.value_fields { println!("value: name={:?} agg={:?} source_field={:?}", v.name, v.agg, v.source_field); }
    let src = wb.sheets.iter().find(|s| s.name == m.source_sheet);
    println!("source found={}", src.is_some());
    if let Some(src) = src {
        for c in 0..8u32 {
            let h = src.cells.get(0, c);
            let d1 = src.cells.get(1, c);
            let kind = |v: &CellValue| match v { CellValue::Empty=>"Empty", CellValue::Number(_)=>"Num", CellValue::Text(_)=>"Text", CellValue::RichText(_)=>"Rich", CellValue::Formula{..}=>"Formula", CellValue::Bool(_)=>"Bool", CellValue::Error(_)=>"Err" };
            println!("col{c}: header={:?}({}) row1={:?}({})", h.to_display_string(), kind(&h), d1.to_display_string(), kind(&d1));
        }
    }
    let r = roe_parser::ooxml::xlsx_pivot_writer::build_spec(&wb.sheets, wb.sheets.iter().position(|s| s.pivot.is_some()).unwrap(), 1);
    println!("build_spec is_some={}", r.is_some());
}
