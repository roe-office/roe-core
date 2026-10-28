// v88 会话：核实 tdf165180 样本箭头丢失/线条变黑问题，打印每条 connector
// 完整路径点坐标，检查是否存在退化（零长度）线段导致 draw_one_end 提前 return。
use roe_parser::RoeDocument;

fn main() {
    let path = "test_samples/pptx/oox_unit_data__tdf165180_standardConnectorsISO29500.potx";
    let doc = RoeDocument::open_pptx_file(path).expect("failed to open/parse pptx");

    let pres = match doc {
        RoeDocument::Impress(p) => p,
        _ => panic!("expected Impress document"),
    };

    for (si, slide) in pres.slides.iter().enumerate() {
        for s in &slide.shapes {
            let is_connector = matches!(&s.geometry, roe_model::common::ShapeGeometry::PresetPath(_)) && (s.style.head_arrow.is_some() || s.style.tail_arrow.is_some() || s.st_cxn.is_some() || s.end_cxn.is_some());
            if !is_connector {
                continue;
            }
            println!("=== slide{} shape_id={} ===", si, s.id);
            println!("  position: x={:.2} y={:.2} w={:.2} h={:.2}",
                s.position.x(), s.position.y(), s.position.w(), s.position.h());
            println!("  head_arrow={:?} tail_arrow={:?}", s.style.head_arrow, s.style.tail_arrow);
            if let Some(stroke) = &s.style.stroke {
                println!("  stroke.color={:?} width={:?}", stroke.color, stroke.width);
            } else {
                println!("  stroke=None");
            }

            if let roe_model::common::ShapeGeometry::PresetPath(cmds) = &s.geometry {
                for (i, cmd) in cmds.iter().enumerate() {
                    println!("  cmds[{}] = {:?}", i, cmd);
                }
                // 检查相邻点是否重合
                let get_xy = |cmd: &roe_model::common::PathCommand| -> Option<(f32, f32)> {
                    use roe_model::common::PathCommand::*;
                    match cmd {
                        MoveTo { x, y } | LineTo { x, y } => Some((*x, *y)),
                        CubicTo { x, y, .. } => Some((*x, *y)),
                        _ => None,
                    }
                };
                if cmds.len() >= 2 {
                    if let (Some(p0), Some(p1)) = (get_xy(&cmds[0]), get_xy(&cmds[1])) {
                        let d = ((p1.0-p0.0).powi(2) + (p1.1-p0.1).powi(2)).sqrt();
                        println!("  head-side dist(cmds[0],cmds[1]) = {:.4}", d);
                    }
                    let n = cmds.len();
                    if let (Some(pn1), Some(pn2)) = (get_xy(&cmds[n-1]), get_xy(&cmds[n.saturating_sub(2)])) {
                        let d = ((pn1.0-pn2.0).powi(2) + (pn1.1-pn2.1).powi(2)).sqrt();
                        println!("  tail-side dist(cmds[len-2],cmds[len-1]) = {:.4}", d);
                    }
                }
            } else {
                println!("  geometry = {:?} (NOT PresetPath!)", s.geometry);
            }
            println!();
        }
    }
}
