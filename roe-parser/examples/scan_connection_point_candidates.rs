// v181 阶段E探测脚本：遍历 test_samples/pptx/ 目录，找出适合作为
// roe_impress_get_connection_points 端到端测试样本的文件——
// 含 PresetPath 几何、preset_name 非空且不属于 Rect/Ellipse/Triangle/Line
// 这4个已被专门几何变体覆盖的类型，且最好带 rotation/flip/adj_overrides，
// 这样端到端测试能同时验证 map_connection_point_to_global 的变换路径，
// 不只是本地坐标求值。
//
// 用法: cargo run --example scan_connection_point_candidates -p roe-parser -- <目录>
// 输出: 每个候选文件的 slide_idx/shape_id/preset_name/是否带变换/adj数量，
// 按"带变换"优先排序，方便直接挑第一批作为测试样本。

use roe_model::common::ShapeGeometry;
use roe_model::impress::{Shape, ShapeContent};
use std::path::Path;

struct Candidate {
    file: String,
    slide_idx: usize,
    shape_id: u32,
    preset_name: String,
    has_transform: bool,
    adj_count: usize,
    cxn_count_hint: bool, // true 表示后续需要实际调用 find_preset_shape 确认 cxn_lst 非空
}

fn walk_shapes(
    shapes: &[Shape],
    file: &str,
    slide_idx: usize,
    out: &mut Vec<Candidate>,
) {
    for shape in shapes {
        if let ShapeGeometry::PresetPath(path_data) = &shape.geometry {
            let name = path_data.preset_name.as_str();
            if !name.is_empty() && !matches!(name, "rect" | "ellipse" | "triangle" | "line") {
                let has_transform = shape.position.rotation.is_some()
                    || shape.position.flip_h.unwrap_or(false)
                    || shape.position.flip_v.unwrap_or(false);
                out.push(Candidate {
                    file: file.to_string(),
                    slide_idx,
                    shape_id: shape.id,
                    preset_name: name.to_string(),
                    has_transform,
                    adj_count: path_data.adj_overrides.len(),
                    cxn_count_hint: true,
                });
            }
        }
        if let ShapeContent::Group { children, .. } = &shape.content {
            walk_shapes(children, file, slide_idx, out);
        }
    }
}

fn main() {
    let dir = std::env::args()
        .nth(1)
        .expect("用法: scan_connection_point_candidates <pptx目录>");

    let mut all_candidates: Vec<Candidate> = Vec::new();
    let mut total_files = 0u32;
    let mut parse_failures: Vec<String> = Vec::new();

    let entries = std::fs::read_dir(&dir).expect("读取目录失败");
    let mut paths: Vec<_> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    paths.sort();

    for path in paths {
        if path.extension().and_then(|e| e.to_str()) != Some("pptx") {
            continue;
        }
        total_files += 1;
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
        for (slide_idx, slide) in pres.slides.iter().enumerate() {
            walk_shapes(&slide.shapes, &file_name, slide_idx, &mut all_candidates);
        }
    }

    // 带变换的排前面，其次按 adj_count 降序（adj覆盖越多，越能验证"必须用
    // 同一份adj_overrides重新求值"这条设计约束，见v181二节）
    all_candidates.sort_by(|a, b| {
        b.has_transform
            .cmp(&a.has_transform)
            .then(b.adj_count.cmp(&a.adj_count))
    });

    println!("=== 扫描结果 ===");
    println!("总文件数: {total_files}");
    println!("解析失败: {} 个", parse_failures.len());
    for f in &parse_failures {
        println!("  ! {f}");
    }
    println!("候选 shape 总数: {}", all_candidates.len());
    println!();
    println!("=== 候选列表（前30条，优先展示带变换的） ===");
    for c in all_candidates.iter().take(30) {
        println!(
            "{file}\tslide={slide_idx}\tshape_id={shape_id}\tpreset={preset_name}\ttransform={has_transform}\tadj_count={adj_count}",
            file = c.file,
            slide_idx = c.slide_idx,
            shape_id = c.shape_id,
            preset_name = c.preset_name,
            has_transform = c.has_transform,
            adj_count = c.adj_count,
        );
    }

    println!();
    println!("=== preset_name 分布 ===");
    let mut name_counts: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    for c in &all_candidates {
        *name_counts.entry(c.preset_name.clone()).or_insert(0) += 1;
    }
    let mut sorted_names: Vec<_> = name_counts.into_iter().collect();
    sorted_names.sort_by(|a, b| b.1.cmp(&a.1));
    for (name, n) in sorted_names.iter().take(20) {
        println!("  {name}: {n}");
    }
    let _ = all_candidates.iter().any(|c| c.cxn_count_hint); // 占位，避免unused warning
}
