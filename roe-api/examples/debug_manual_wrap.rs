// 临时诊断脚本：打印手册.docx 的 body 顺序 + 每段文字预览 + Frame 位置，
// 用于定位 WROOT-072 段落级 AABB 环绕判断在真实样本上具体在哪一步失效。
// 不改动任何production代码，纯只读诊断。
use roe_parser::RoeDocument;
use roe_model::writer::BodyElement;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).expect("usage: debug_manual_wrap <input.docx>");

    let data = std::fs::read(path).expect("read file");
    let doc = RoeDocument::open_docx_bytes(&data).expect("parse docx");
    let wdoc = match doc {
        RoeDocument::Writer(d) => d,
        _ => panic!("not a Writer document"),
    };

    println!("page_size: {:?}", wdoc.page_size);
    println!("body.len(): {}", wdoc.body.len());
    println!();

    for (i, elem) in wdoc.body.iter().enumerate() {
        match elem {
            BodyElement::Paragraph(p) => {
                let text: String = p.runs.iter().map(|r| r.text.as_str()).collect();
                let preview: String = text.chars().take(20).collect();
                println!(
                    "[{:3}] Paragraph  style={:?}  runs={}  text=\"{}{}\"",
                    i,
                    p.style,
                    p.runs.len(),
                    preview,
                    if text.chars().count() > 20 { "..." } else { "" }
                );
            }
            BodyElement::Frame(f) => {
                println!(
                    "[{:3}] Frame      x_pt={:.1} y_pt={:.1} w_pt={:.1} h_pt={:.1} anchor_h={:?} anchor_v={:?} align_h={:?} wrap_type={:?}",
                    i, f.x_pt, f.y_pt, f.width_pt, f.height_pt, f.anchor_type_h, f.anchor_type_v, f.align_h, f.wrap_type
                );
            }
            BodyElement::Table(_) => {
                println!("[{:3}] Table", i);
            }
            other => {
                println!("[{:3}] {:?}", i, std::mem::discriminant(other));
            }
        }
    }
}
