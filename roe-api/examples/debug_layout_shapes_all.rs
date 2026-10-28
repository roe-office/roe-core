// 诊断专用：打印 sd_data_pptx__ooxtheme.pptx slide0 的 layout_shapes
// 完整列表（不只是Group内部），确认Group子元素解析丢失后，这些
// shape是否变成了独立的顶层layout_shape。
use roe_model::impress::ShapeContent;
use roe_parser::RoeDocument;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let pptx_path = args.get(1).expect("usage: debug_layout_shapes_all <pptx>");

    let doc = RoeDocument::open_pptx_file(pptx_path).expect("parse failed");
    let pres = match doc {
        RoeDocument::Impress(p) => p,
        _ => panic!("not a pptx"),
    };

    let slide = &pres.slides[0];
    println!("layout_shapes.len() = {}", slide.layout_shapes.len());
    for (i, s) in slide.layout_shapes.iter().enumerate() {
        match &s.content {
            ShapeContent::Group { children, .. } => {
                println!("[{}] id={} GROUP with {} children pos={:?}",
                    i, s.id, children.len(), s.position);
                for (j, c) in children.iter().enumerate() {
                    println!("    child[{}] id={} geom={:?} pos={:?}",
                        j, c.id, c.geometry, c.position);
                }
            }
            other => {
                println!("[{}] id={} content={:?} pos={:?}",
                    i, s.id, other, s.position);
            }
        }
    }
}
