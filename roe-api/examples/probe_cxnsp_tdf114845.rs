// PROOT-002 验证脚本：确认 tdf114845 样本里的 cxnSp 能被正确解析成 Shape，
// 检查 geometry 是否为 PresetPath、stroke 颜色是否非空、st_cxn/end_cxn
// 是否被正确读取（对应 XML 里的 <a:stCxn>/<a:endCxn>）。
// 用法：cargo run -p roe-api --example probe_cxnsp_tdf114845

use roe_parser::RoeDocument;

fn main() {
    let path = "test_samples/curated/pptx/large__sd_data_pptx__tdf114845_rotateShape.pptx";
    let doc = RoeDocument::open_pptx_file(path).expect("failed to open/parse pptx");

    let pres = match doc {
        RoeDocument::Impress(p) => p,
        _ => panic!("expected Impress document"),
    };

    let mut total_shapes = 0;
    let mut connector_like = 0;

    for (si, slide) in pres.slides.iter().enumerate() {
        for s in &slide.shapes {
            total_shapes += 1;
            let is_connector = s.st_cxn.is_some() || s.end_cxn.is_some();
            if is_connector {
                connector_like += 1;
            }
            println!(
                "slide{} shape_id={} geometry={:?} stroke={:?} st_cxn={:?} end_cxn={:?} head_arrow={:?} tail_arrow={:?}",
                si,
                s.id,
                match &s.geometry {
                    roe_model::common::ShapeGeometry::PresetPath(cmds) => format!("PresetPath(len={})", cmds.len()),
                    other => format!("{:?}", other),
                },
                s.style.stroke,
                s.st_cxn,
                s.end_cxn,
                s.style.head_arrow,
                s.style.tail_arrow,
            );
        }
    }

    println!("---");
    println!("total_shapes={} connector_like(st_cxn/end_cxn set)={}", total_shapes, connector_like);
}
