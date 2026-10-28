// PROOT-005 followup 临时调试脚本：打印 slide.layout_shapes 内容，
// 定位 layout 装饰性形状（tdf158512 太阳+房子图案）为什么没渲染出来。
use roe_parser::RoeDocument;

fn main() {
    let path = "/home/yong/Projects/roe/test_samples/pptx/sd_data_pptx__tdf158512.pptx";
    let doc = RoeDocument::open_pptx_file(path).expect("parse pptx");
    let pres = match doc {
        RoeDocument::Impress(p) => p,
        _ => panic!("not an Impress document"),
    };

    for (i, slide) in pres.slides.iter().enumerate() {
        println!("=== slide {} ===", i);
        println!("  master_idx: {:?}", slide.master_idx);
        println!("  background: {:?}", slide.background.is_some());
        println!("  layout_shapes.len(): {}", slide.layout_shapes.len());
        for s in &slide.layout_shapes {
            print!("    shape id={} ", s.id);
            match &s.content {
                roe_model::impress::ShapeContent::Image(img) => {
                    println!("Image bytes.len()={} mime={}", img.data.len(), img.mime);
                }
                roe_model::impress::ShapeContent::Text(_) => println!("Text"),
                roe_model::impress::ShapeContent::Empty => println!("Empty"),
                other => println!("{:?}", std::mem::discriminant(other)),
            }
            println!("      position: x={} y={} w={} h={}", s.position.x(), s.position.y(), s.position.w(), s.position.h());
        }
    }
}
