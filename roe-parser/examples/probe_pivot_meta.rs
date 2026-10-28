fn main() {
    let dir = "/home/yong/pivot_tests";
    let files = ["pivot_1row1col.xlsx", "pivot_1row1col_2val.xlsx", "pivot_1row.xlsx"];
    for f in files {
        let path = format!("{dir}/{f}");
        let bytes = match std::fs::read(&path) { Ok(b) => b, Err(e) => { println!("{f}: skip ({e})"); continue; } };
        let doc = roe_parser::RoeDocument::open_xlsx_bytes(&bytes).expect("parse");
        let wb = match doc { roe_parser::RoeDocument::Calc(wb) => wb, _ => panic!("not calc") };
        let mut found = false;
        for s in &wb.sheets {
            if let Some(m) = &s.pivot {
                found = true;
                println!("{f}: sheet={} source_sheet={} source_ref={:?} style={:?} name={:?} cells_rect={:?}",
                    s.name, m.source_sheet, m.source_ref, m.style_name, m.name, m.cells_rect);
                assert_eq!(m.source_ref.as_deref(), Some("A1:H201"), "{f} source_ref");
                let want = if f == "pivot_1row.xlsx" { "PivotStyleLight17" } else { "PivotStyleLight20" };
                assert_eq!(m.style_name.as_deref(), Some(want), "{f} style");
                assert_eq!(m.name.as_deref(), Some("数据透视表1"), "{f} name");
            }
        }
        assert!(found, "{f}: no pivot");
    }
    println!("ALL OK");
}
