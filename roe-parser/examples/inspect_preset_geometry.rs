// PPTX-11 part5 B任务验证：跑真实LOK测试样本，人眼核对
// find_preset_shape/eval_preset_shape 接线后的 geometry 产出是否合理。
// 非正式测试，用完可删或保留作为后续回归探查工具。
use roe_parser::ooxml::pptx::PptxParser;
use roe_model::impress::ShapeGeometry;
use std::collections::HashMap;

fn geometry_summary(g: &ShapeGeometry) -> String {
    match g {
        ShapeGeometry::Rect => "Rect".to_string(),
        ShapeGeometry::RoundRect { radius } => format!("RoundRect{{radius:{radius}}}"),
        ShapeGeometry::Ellipse => "Ellipse".to_string(),
        ShapeGeometry::Triangle => "Triangle".to_string(),
        ShapeGeometry::Line => "Line".to_string(),
        ShapeGeometry::PresetPath(cmds) => format!("PresetPath({} cmds)", cmds.len()),
        ShapeGeometry::Custom(s) => format!("Custom(\"{s}\")"),
    }
}

fn walk_shapes(shapes: &[roe_model::impress::Shape], depth: usize, counts: &mut HashMap<String, u32>) {
    for shape in shapes {
        let kind = match &shape.geometry {
            ShapeGeometry::Rect => "Rect",
            ShapeGeometry::RoundRect { .. } => "RoundRect",
            ShapeGeometry::Ellipse => "Ellipse",
            ShapeGeometry::Triangle => "Triangle",
            ShapeGeometry::Line => "Line",
            ShapeGeometry::PresetPath(_) => "PresetPath",
            ShapeGeometry::Custom(_) => "Custom",
        };
        *counts.entry(kind.to_string()).or_insert(0) += 1;

        let indent = "  ".repeat(depth);
        println!("{indent}id={} {}", shape.id, geometry_summary(&shape.geometry));

        if let roe_model::impress::ShapeContent::Group { children, .. } = &shape.content {
            walk_shapes(children, depth + 1, counts);
        }
    }
}

fn main() {
    let path = std::env::args().nth(1).expect("用法: inspect_preset_geometry <pptx路径>");
    let data = std::fs::read(&path).expect("读取 pptx 失败");
    let pres = PptxParser::parse(&data).expect("解析失败");

    println!("文件: {path}");
    println!("幻灯片数: {}", pres.slides.len());
    println!();

    let mut counts: HashMap<String, u32> = HashMap::new();
    for (i, slide) in pres.slides.iter().enumerate() {
        println!("== Slide {i} ({} 个顶层 shape) ==", slide.shapes.len());
        walk_shapes(&slide.shapes, 1, &mut counts);
    }

    println!();
    println!("=== geometry 类型统计（含分组内递归） ===");
    let mut sorted: Vec<_> = counts.into_iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1));
    for (kind, n) in sorted {
        println!("  {kind}: {n}");
    }
}
