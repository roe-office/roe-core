// WROOT-101（表格浮动绕排）验证脚本：只验证解析层float_pos是否正确读到
// w:tblpPr的XML值，不涉及渲染层（渲染层此时尚未接入float_pos）。
use roe_parser::RoeDocument;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let input_path = args.get(1).expect("usage: verify_tblppr <input.docx>");

    let data = std::fs::read(input_path).expect("read file");
    let doc = RoeDocument::open_docx_bytes(&data).expect("parse docx");
    let wdoc = match doc {
        RoeDocument::Writer(d) => d,
        _ => panic!("not a Writer document"),
    };

    let mut found = 0;
    for elem in wdoc.body.iter() {
        if let roe_model::writer::BodyElement::Table(tbl) = elem {
            found += 1;
            println!("表格#{}: float_pos = {:#?}", found, tbl.float_pos);
        }
    }
    if found == 0 {
        println!("未在文档中找到任何表格");
    }
}
