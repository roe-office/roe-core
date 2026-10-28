// v183 阶段：为"真实文件 round-trip 测试"探测合适样本。
//
// 目的：pptx_writer.rs 明确只支持部分字段（slide_size / shape id・position・
// geometry(preset) / fill(Solid・None) / stroke宽度 / text文字 / image），
// 不支持 Table/Chart/SmartArt/Animation/Notes/Gradient/Picture(Tiled)Fill 等。
// 本脚本扫描 test_samples/curated/pptx/ 下每个文件，统计其 shape 内容
// 落在 writer 支持范围内的比例，挑出"干净"（内容主要可被 writer 表达）
// 的样本，作为 round-trip 测试的真实文件输入。
//
// 用法: cargo run --example scan_pptx_roundtrip_candidates -p roe-parser -- <目录>

use roe_model::impress::{ShapeContent, ShapeGeometry};
use roe_model::common::FillStyle;

#[derive(Default)]
struct Stat {
    file: String,
    slide_count: usize,
    shape_count: usize,
    text_shapes: usize,
    image_shapes: usize,
    empty_shapes: usize,
    table_or_chart_shapes: usize, // writer 完全不支持，出现即说明该文件不适合做round-trip样本
    group_shapes: usize,
    preset_geometry: usize,   // Rect/Ellipse/Triangle/Line/RoundRect/Custom(prst) 均可被 writer 表达
    solid_or_none_fill: usize,
    gradient_or_picture_fill: usize, // writer 会退化成 noFill，语义不保真
    has_stroke: usize,
}

fn walk_shapes(shapes: &[roe_model::impress::Shape], st: &mut Stat) {
    for shape in shapes {
        st.shape_count += 1;
        match &shape.content {
            ShapeContent::Text(_) => st.text_shapes += 1,
            ShapeContent::Image(_) => st.image_shapes += 1,
            ShapeContent::Empty => st.empty_shapes += 1,
            ShapeContent::Table(_) | ShapeContent::Chart(_) | ShapeContent::StatChart(_) => st.table_or_chart_shapes += 1,
            ShapeContent::Group { children, .. } => {
                st.group_shapes += 1;
                walk_shapes(children, st);
            }
        }
        match &shape.geometry {
            ShapeGeometry::Rect
            | ShapeGeometry::Ellipse
            | ShapeGeometry::Triangle
            | ShapeGeometry::Line
            | ShapeGeometry::RoundRect { .. }
            | ShapeGeometry::Custom(_) => st.preset_geometry += 1,
            _ => {}
        }
        match &shape.style.fill {
            Some(FillStyle::Solid(_)) | Some(FillStyle::None) | None => st.solid_or_none_fill += 1,
            Some(FillStyle::Gradient { .. }) | Some(FillStyle::Picture(_)) | Some(FillStyle::PictureTiled { .. }) => {
                st.gradient_or_picture_fill += 1;
            }
        }
        if shape.style.stroke.is_some() {
            st.has_stroke += 1;
        }
    }
}

fn main() {
    let dir = std::env::args()
        .nth(1)
        .expect("用法: scan_pptx_roundtrip_candidates <pptx目录>");

    let mut stats: Vec<Stat> = Vec::new();
    let mut parse_failures: Vec<String> = Vec::new();

    let entries = std::fs::read_dir(&dir).expect("读取目录失败");
    let mut paths: Vec<_> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    paths.sort();

    for path in paths {
        if path.extension().and_then(|e| e.to_str()) != Some("pptx") {
            continue;
        }
        let file_name = path.file_name().unwrap().to_string_lossy().to_string();
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(e) => {
                parse_failures.push(format!("{file_name}: 读取失败 {e}"));
                continue;
            }
        };
        let pres = match roe_parser::ooxml::pptx::PptxParser::parse(&data) {
            Ok(p) => p,
            Err(e) => {
                parse_failures.push(format!("{file_name}: 解析失败 {e:?}"));
                continue;
            }
        };
        let mut st = Stat {
            file: file_name,
            slide_count: pres.slides.len(),
            ..Default::default()
        };
        for slide in &pres.slides {
            walk_shapes(&slide.shapes, &mut st);
        }
        stats.push(st);
    }

    // 排序：优先"干净"（无 table/chart、无 gradient/picture fill）且
    // shape_count 适中（不要太小没意义，不要太大难人工核对）
    stats.retain(|s| s.shape_count > 0);
    stats.sort_by(|a, b| {
        let a_clean = a.table_or_chart_shapes == 0 && a.gradient_or_picture_fill == 0;
        let b_clean = b.table_or_chart_shapes == 0 && b.gradient_or_picture_fill == 0;
        b_clean.cmp(&a_clean).then(a.shape_count.cmp(&b.shape_count))
    });

    println!("=== 解析失败 {} 个 ===", parse_failures.len());
    for f in &parse_failures {
        println!("  ! {f}");
    }
    println!();
    println!("=== 候选样本（干净优先、shape_count 从小到大排序）===");
    println!(
        "{:<55} slides shapes text img empty tbl/chart grp preset solidFill grad/pic stroke",
        "file"
    );
    for s in stats.iter().take(40) {
        println!(
            "{:<55} {:>6} {:>6} {:>4} {:>3} {:>5} {:>9} {:>3} {:>6} {:>9} {:>8} {:>6}",
            s.file, s.slide_count, s.shape_count, s.text_shapes, s.image_shapes,
            s.empty_shapes, s.table_or_chart_shapes, s.group_shapes, s.preset_geometry,
            s.solid_or_none_fill, s.gradient_or_picture_fill, s.has_stroke
        );
    }
}
