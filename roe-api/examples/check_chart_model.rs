use roe_parser::RoeDocument;
use roe_model::impress::ShapeContent;

fn main() {
    let path = "/home/yong/Projects/roe/test_samples/curated/pptx/small__oox_unit_data__chart-theme-override.pptx";
    let doc = RoeDocument::open_pptx_file(path).expect("parse pptx");
    let pres = match doc {
        RoeDocument::Impress(p) => p,
        _ => panic!("not an Impress document"),
    };

    for (i, slide) in pres.slides.iter().enumerate() {
        for s in &slide.shapes {
            if let ShapeContent::Chart(chart_model) = &s.content {
                println!("slide {} shape id={}: title={:?} kind={:?} x_labels={:?}",
                    i, s.id, chart_model.title, chart_model.kind, chart_model.x_labels);
                for ser in &chart_model.series {
                    println!("  series name={:?} color={:?}", ser.name, ser.color);
                }
            }
        }
    }
}
