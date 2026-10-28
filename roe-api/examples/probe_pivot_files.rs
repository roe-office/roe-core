fn main() {
    let files = [
        "pivot_1row.xlsx", "pivot_1col.xlsx", "pivot_1row1col.xlsx",
        "pivot_2row.xlsx", "pivot_2col.xlsx", "pivot_2row2col.xlsx",
        "pivot_1row1col1filter.xlsx", "pivot_test.xlsx", "pivot_1row1col_2val.xlsx",
    ];
    for f in files {
        let path = format!("/home/yong/pivot_tests/{}", f);
        let bytes = std::fs::read(&path).expect("read");
        let doc = roe_parser::RoeDocument::open_xlsx_bytes(&bytes).expect("parse");
        let wb = match doc { roe_parser::RoeDocument::Calc(wb) => wb, _ => panic!("not calc") };
        println!("=== {} ===", f);
        for (i, s) in wb.sheets.iter().enumerate() {
            let cols: Vec<String> = if let Some(col0) = s.cells.cols.get(&0) {
                vec![]
            } else { vec![] };
            println!("  sheet[{}] = {:?}  pivot={}", i, s.name, s.pivot.is_some());
            if let Some(p) = &s.pivot {
                println!("    source_sheet={:?} rows={:?} cols={:?} values={:?}",
                    p.source_sheet, p.row_field_names, p.col_field_names,
                    p.value_fields.iter().map(|v| &v.name).collect::<Vec<_>>());
            }
        }
        // 打印非pivot sheet（明细sheet）的表头行（第0行前几列文本）
        for (i, s) in wb.sheets.iter().enumerate() {
            if s.pivot.is_some() { continue; }
            let mut headers = vec![];
            for col_idx in 0..10u32 {
                if let Some(col) = s.cells.cols.get(&col_idx) {
                    if let Some(block) = col.blocks.first() {
                        if let Some(v) = block.values.get(0) {
                            headers.push(format!("{:?}", v));
                        }
                    }
                }
            }
            println!("  明细sheet[{}]={:?} 表头: {:?}", i, s.name, headers);
        }
    }
}
