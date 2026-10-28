// WROOT-101 排查用：验证 col_widths 解析结果，排查浮动表格列宽比例异常问题
use roe_parser::RoeDocument;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let input_path = args.get(1).expect("usage: verify_colwidths <input.docx>");

    let data = std::fs::read(input_path).expect("read file");
    let doc = RoeDocument::open_docx_bytes(&data).expect("parse docx");
    let wdoc = match doc {
        RoeDocument::Writer(d) => d,
        _ => panic!("not a Writer document"),
    };

    for elem in wdoc.body.iter() {
        if let roe_model::writer::BodyElement::Table(tbl) = elem {
            println!("表格 col_widths = {:?}", tbl.col_widths);
            println!("表格 float_pos = {:#?}", tbl.float_pos);
            let total: f32 = tbl.col_widths.iter().sum();
            println!("col_widths总和 = {}", total);
        }
    }
}
