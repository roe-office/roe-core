// PROOT-002 箭头设计前置探针：确认PresetPath(len=2)的MoveTo/LineTo哪个
// 对应st_cxn（起点）哪个对应end_cxn（终点）。方法：打印每条connector
// 自身的position(xfrm off/ext)、PresetPath局部坐标点，以及st_cxn/end_cxn
// 指向的目标shape的position，用连接线端点是否落在目标shape边界附近来
// 交叉验证对应关系，不假设。
// 用法：cargo run -p roe-api --example probe_connector_path_order

use roe_parser::RoeDocument;
use std::collections::HashMap;

fn main() {
    let path = "test_samples/curated/pptx/large__sd_data_pptx__tdf114845_rotateShape.pptx";
    let doc = RoeDocument::open_pptx_file(path).expect("failed to open/parse pptx");

    let pres = match doc {
        RoeDocument::Impress(p) => p,
        _ => panic!("expected Impress document"),
    };

    for (si, slide) in pres.slides.iter().enumerate() {
        // 先建立 shape_id -> (position) 索引，方便查st_cxn/end_cxn目标位置
        let mut pos_by_id: HashMap<u32, roe_model::common::Transform> = HashMap::new();
        for s in &slide.shapes {
            pos_by_id.insert(s.id, s.position.clone());
        }

        for s in &slide.shapes {
            let is_connector = s.st_cxn.is_some() || s.end_cxn.is_some();
            if !is_connector {
                continue;
            }
            println!("=== slide{} shape_id={} ===", si, s.id);
            println!("  own position: x={:.2} y={:.2} w={:.2} h={:.2}",
                s.position.x(), s.position.y(), s.position.w(), s.position.h());

            if let roe_model::common::ShapeGeometry::PresetPath(cmds) = &s.geometry {
                for (i, cmd) in cmds.iter().enumerate() {
                    println!("  cmds[{}] = {:?}", i, cmd);
                }
            }

            if let Some((tid, idx)) = s.st_cxn {
                if let Some(p) = pos_by_id.get(&tid) {
                    println!("  st_cxn -> shape_id={} idx={} target_position: x={:.2} y={:.2} w={:.2} h={:.2}",
                        tid, idx, p.x(), p.y(), p.w(), p.h());
                }
            }
            if let Some((tid, idx)) = s.end_cxn {
                if let Some(p) = pos_by_id.get(&tid) {
                    println!("  end_cxn -> shape_id={} idx={} target_position: x={:.2} y={:.2} w={:.2} h={:.2}",
                        tid, idx, p.x(), p.y(), p.w(), p.h());
                }
            }
            println!();
        }
    }
}
