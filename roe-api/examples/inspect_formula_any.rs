use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = &args[1];
    let data = std::fs::read(path).expect("read failed");
    let ext = Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();

    let doc = match ext.as_str() {
        "xlsx" => roe_parser::RoeDocument::open_xlsx_bytes(&data),
        "fods" => roe_parser::RoeDocument::open_fods_bytes(&data),
        "ods" => roe_parser::RoeDocument::open_odf_bytes(&data),
        other => panic!("unsupported ext: {}", other),
    }.expect("parse failed");

    let mut doc = doc;
    let _engine = roe_api::document::init_calc_engine_for_doc(&mut doc);

    if let roe_parser::RoeDocument::Calc(wb) = doc {
        for (si, sheet) in wb.sheets.iter().enumerate() {
            println!("--- sheet {} ({}) ---", si, sheet.name);
            for (&col, column) in sheet.cells.cols.iter() {
                for block in &column.blocks {
                    for (offset, value) in block.values.iter().enumerate() {
                        if let roe_model::calc::CellValue::Formula { source, cached } = value {
                            let row = block.start_row + offset as u32;
                            println!("r{} c{}: formula={:?} cached={:?}", row, col, source, cached);
                        }
                    }
                }
            }
        }
    }
}
