use roe_parser::RoeDocument;

fn main() {
    let path = "/home/yong/Projects/roe/test_samples/pptx/sd_data_pptx__ooxtheme.pptx";
    let doc = RoeDocument::open_pptx_file(path).expect("parse failed");
    let pres = match doc {
        RoeDocument::Impress(p) => p,
        _ => panic!("not a pptx"),
    };
    for (i, slide) in pres.slides.iter().enumerate() {
        println!("=== slide {} : {} shapes (own), {} layout_shapes ===",
            i, slide.shapes.len(), slide.layout_shapes.len());
        println!("--- own shapes ---");
        for s in &slide.shapes {
            println!("  id={} geometry={:?} fill={:?}", s.id, s.geometry, s.style.fill);
        }
        println!("--- layout_shapes (master裁底+layout叠上, 渲染顺序=Vec顺序) ---");
        for (idx, s) in slide.layout_shapes.iter().enumerate() {
            println!(
                "  #{} id={} geometry={:?} pos={:?} fill={:?}",
                idx, s.id, s.geometry, s.position, s.style.fill
            );
        }
    }
}
