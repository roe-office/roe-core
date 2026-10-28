use std::io::Cursor;
fn main() {
    let dir = "/home/yong/pivot_tests";
    std::fs::create_dir_all("/tmp/px_out").unwrap();
    let files = ["pivot_1row", "pivot_1col", "pivot_2row", "pivot_2col", "pivot_2row2col", "pivot_1row1col", "pivot_1row1col1filter", "pivot_1row1col_2val"];
    for f in files {
        let bytes = match std::fs::read(format!("{dir}/{f}.xlsx")) { Ok(b) => b, Err(e) => { println!("{f}: skip ({e})"); continue; } };
        let wb = match roe_parser::RoeDocument::open_xlsx_bytes(&bytes).expect("parse1") { roe_parser::RoeDocument::Calc(wb) => wb, _ => panic!("not calc") };
        let out = roe_parser::ooxml::xlsx_writer::XlsxWriter::write(&wb).expect("write");
        std::fs::write(format!("/tmp/px_out/{f}.xlsx"), &out).unwrap();
        let ar = zip::ZipArchive::new(Cursor::new(&out)).expect("zip");
        let names: Vec<String> = ar.file_names().filter(|n| n.contains("pivot")).map(|s| s.to_string()).collect();
        let wb2 = match roe_parser::RoeDocument::open_xlsx_bytes(&out).expect("parse2") { roe_parser::RoeDocument::Calc(wb) => wb, _ => panic!("not calc") };
        let m1 = wb.sheets.iter().find_map(|s| s.pivot.as_ref());
        let m2 = wb2.sheets.iter().find_map(|s| s.pivot.as_ref());
        println!("{f}: parts={} pivot_before={} pivot_after={}", names.len(), m1.is_some(), m2.is_some());
        if let (Some(a), Some(b)) = (m1, m2) {
            println!("   rows {:?}/{:?} cols {:?}/{:?} vals {}/{} filters {}/{} name {:?}/{:?} style {:?}/{:?} src {:?}/{:?}",
                a.row_field_names, b.row_field_names, a.col_field_names, b.col_field_names,
                a.value_fields.len(), b.value_fields.len(), a.filter_fields.len(), b.filter_fields.len(),
                a.name, b.name, a.style_name, b.style_name, a.source_ref, b.source_ref);
        }
    }
}
