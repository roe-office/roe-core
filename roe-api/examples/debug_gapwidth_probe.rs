// 临时探针：只用于排查 gap_width_pct 解析链路是否走通，验证完可删除。
use roe_parser::RoeDocument;

fn main() {
    let path = "/home/yong/Projects/roe/test_samples/curated/pptx/small__oox_unit_data__chart-theme-override.pptx";
    let doc = RoeDocument::open_pptx_file(path).expect("parse pptx");
    let pres = match doc {
        RoeDocument::Impress(p) => p,
        _ => panic!("not an Impress document"),
    };

    for (si, slide) in pres.slides.iter().enumerate() {
        for shape in slide.shapes.iter() {
            if let roe_model::impress::ShapeContent::Chart(model) = &shape.content {
                println!(
                    "slide {} shape {}: kind={:?} gap_width_pct={:?}",
                    si, shape.id, model.kind, model.gap_width_pct
                );
            }
        }
    }
}
