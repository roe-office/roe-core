use roe_parser::RoeDocument;

fn main() {
    let path = "/home/yong/Projects/roe/test_samples/curated/pptx/small__sd_data_pptx__tdf148810_PARA_OUTLLEVEL.pptx";
    let doc = RoeDocument::open_pptx_file(path).expect("parse pptx");
    let pres = match doc {
        RoeDocument::Impress(p) => p,
        _ => panic!("not an Impress document"),
    };
    for (i, slide) in pres.slides.iter().enumerate() {
        println!("=== slide {} ===", i);
        for s in &slide.shapes {
            println!("  shape id={} pos: x={} y={} w={} h={}", s.id, s.position.x(), s.position.y(), s.position.w(), s.position.h());
            if let roe_model::impress::ShapeContent::Text(paras) = &s.content {
                for p in paras {
                    println!("    para alignment={:?} bullet={:?}", p.alignment, p.bullet);
                }
            }
        }
    }
}
