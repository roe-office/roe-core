// PROOT-024 子问题2 探针：确认parser对noFill/ln w=0的解析结果
// 样本：small__sd_data_pptx__tdf148810_PARA_OUTLLEVEL.pptx
// 预期：两个shape都应该是 fill=None(noFill) 且 stroke 里 width=0 或 stroke=None，
// 但渲染结果却出现了黑色边框，用这个探针先确认parser层解析是否正确。
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
            println!("  shape id={}", s.id);
            println!("    fill:   {:?}", s.style.fill);
            println!("    stroke: {:?}", s.style.stroke);
        }
    }
}
