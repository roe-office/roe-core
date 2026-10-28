use roe_model::calc::Sheet;
use std::io::Cursor;

fn dump(s: &Sheet) -> Vec<String> {
    let mut v = vec![];
    for r in 0..40u32 {
        for c in 0..10u32 {
            let t = s.cells.get(r, c).to_display_string();
            if !t.is_empty() {
                v.push(format!("({},{})={}", r, c, t));
            }
        }
    }
    v
}

fn main() {
    let files = ["pivot_1row1col.xlsx", "pivot_2row.xlsx", "pivot_1row1col_2val.xlsx"];
    for f in files {
        let path = format!("/home/yong/pivot_tests/{}", f);
        let bytes = std::fs::read(&path).expect("read");
        println!("################ {} ################", f);
        let doc = roe_parser::RoeDocument::open_xlsx_bytes(&bytes).expect("parse1");
        let wb = match doc { roe_parser::RoeDocument::Calc(wb) => wb, _ => panic!("not calc") };
        let mut before: Vec<(usize, Vec<String>)> = vec![];
        for (i, s) in wb.sheets.iter().enumerate() {
            let d = dump(s);
            println!("[打开后] sheet[{}] {:?} pivot={} cells_rect={:?} 非空格={}",
                i, s.name, s.pivot.is_some(),
                s.pivot.as_ref().and_then(|p| p.cells_rect), d.len());
            before.push((i, d));
        }
        let out = roe_parser::ooxml::xlsx_writer::XlsxWriter::write(&wb).expect("write");
        let out_path = format!("/tmp/rt_{}", f);
        std::fs::write(&out_path, &out).unwrap();
        println!("[保存] {} 字节 -> {}", out.len(), out_path);
        let mut ar = zip::ZipArchive::new(Cursor::new(&out[..])).expect("zip");
        let mut names = vec![];
        for i in 0..ar.len() {
            names.push(ar.by_index(i).unwrap().name().to_string());
        }
        println!("[zip部件] {:?}", names);
        println!("[含pivot部件] {}", names.iter().any(|n| n.to_lowercase().contains("pivot")));
        let doc2 = roe_parser::RoeDocument::open_xlsx_bytes(&out).expect("parse2");
        let wb2 = match doc2 { roe_parser::RoeDocument::Calc(wb) => wb, _ => panic!("not calc") };
        for (i, s) in wb2.sheets.iter().enumerate() {
            let d = dump(s);
            let same = before.get(i).map(|(_, b)| *b == d).unwrap_or(false);
            println!("[重开后] sheet[{}] {:?} pivot={} 非空格={} 与保存前一致={}",
                i, s.name, s.pivot.is_some(), d.len(), same);
            if !same {
                if let Some((_, b)) = before.get(i) {
                    let miss: Vec<_> = b.iter().filter(|x| !d.contains(x)).take(8).collect();
                    let extra: Vec<_> = d.iter().filter(|x| !b.contains(x)).take(8).collect();
                    println!("    丢失(前8): {:?}", miss);
                    println!("    新增(前8): {:?}", extra);
                }
            }
        }
    }
}
