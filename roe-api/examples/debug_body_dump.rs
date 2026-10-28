use roe_parser::RoeDocument;

fn main() {
    let data = std::fs::read(std::env::var("HOME").unwrap() + "/桌面/手册_number.docx").expect("read file");
    let doc = RoeDocument::open_docx_bytes(&data).expect("parse docx");
    let wdoc = match doc {
        RoeDocument::Writer(d) => d,
        _ => panic!("not a Writer document"),
    };
    for (i, e) in wdoc.body.iter().enumerate() {
        match e {
            roe_model::writer::BodyElement::Paragraph(p) => {
                let text: String = p.runs.iter().map(|r| r.text.as_str()).collect();
                let preview: String = text.chars().take(30).collect();
                println!("[{}] Paragraph: {:?}", i, preview);
            }
            roe_model::writer::BodyElement::Frame(f) => {
                println!("[{}] Frame(image) y_pt={} anchor_type_v={:?}", i, f.y_pt, f.anchor_type_v);
            }
            other => {
                println!("[{}] {:?}", i, std::mem::discriminant(other));
            }
        }
    }
}
