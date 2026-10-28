// v186后续：为 odp/fodp "真实文件 round-trip 测试"探测合适样本。
//
// 重要前提：本脚本基于交叉核实 fodp_writer.rs 实际代码（而非仅凭文件头
// 注释，见 roe-save-design.md §三 记录的"文档滞后于代码"教训）确认的
// 真实范围。fodp_writer.rs 的 write_shape_text 函数是唯一权威依据：
//
//   完全不写出（ShapeContent 级别静默丢弃，代码实测，match 分支是
//   `=> {}`，不是注释猜测）：
//     - ShapeContent::Table
//     - ShapeContent::Chart
//     - ShapeContent::Group { .. }（含其所有子 Shape，递归丢失）
//   这三项一旦出现，写出后该 shape 的内容完全消失（Group 更严重，整棵
//   子树都消失），不适合作为 round-trip 样本的核心验证点。
//
//   有写出路径，但语义会退化/单向（文件头注释明确说明）：
//     - ShapeGeometry::RoundRect{radius}/Triangle：写出后 parser 读回
//       会分类成 Custom(other)，不是原变体（geometry 类型判别式不保持，
//       但视觉渲染语义等价——这与 roe-save-design.md §八 记录的
//       ShapeGeometry 技术债是同一个问题在 ODF 侧的表现，本次不处理，
//       只在探测阶段避开或特殊标记）
//     - ShapeStyle（fill/stroke/shadow）：parser 完全不解析，读回后
//       恒为 default，完全单向
//     - ShapeContent::Image：完全单向（parser v1 不解码 base64，归为
//       Empty）
//     - 形状级 anim:transitionFilter：不写出
//     - BeginCondition 的多数变体：读回后会降级为 Indefinite
//
// 本脚本扫描 test_samples/curated/odp/ 下每个文件（同时处理 .odp 和
// .fodp 两种扩展名），递归统计所有 shape（含 Group 子树）的内容分布，
// 挑出"干净"（不含 Table/Chart/Group，且 geometry 不落在
// RoundRect/Triangle）的样本。
//
// 用法: cargo run --example scan_odp_roundtrip_candidates -p roe-parser -- <目录>

use roe_model::impress::{Shape, ShapeContent};
use roe_model::common::ShapeGeometry;
use roe_parser::RoeDocument;

#[derive(Default)]
struct Stat {
    file: String,
    slide_count: usize,
    shape_count: usize,
    text_shapes: usize,
    image_shapes: usize,   // 完全单向（parser 读回后归 Empty）
    empty_shapes: usize,
    table_shapes: usize,   // writer 完全不写出
    chart_shapes: usize,   // writer 完全不写出
    group_shapes: usize,   // writer 完全不写出（含子树）
    roundrect_or_triangle: usize, // 写出后 geometry 类型会变成 Custom
}

fn walk_shapes(shapes: &[Shape], st: &mut Stat) {
    for shape in shapes {
        st.shape_count += 1;
        match &shape.content {
            ShapeContent::Text(_) => st.text_shapes += 1,
            ShapeContent::Image(_) => st.image_shapes += 1,
            ShapeContent::Empty => st.empty_shapes += 1,
            ShapeContent::Table(_) => st.table_shapes += 1,
            ShapeContent::Chart(_) | ShapeContent::StatChart(_) => st.chart_shapes += 1,
            ShapeContent::Group { children, .. } => {
                st.group_shapes += 1;
                walk_shapes(children, st);
            }
        }
        if matches!(
            shape.geometry,
            ShapeGeometry::RoundRect { .. } | ShapeGeometry::Triangle
        ) {
            st.roundrect_or_triangle += 1;
        }
    }
}

fn main() {
    let dir = std::env::args()
        .nth(1)
        .expect("用法: scan_odp_roundtrip_candidates <odp目录>");

    let mut stats: Vec<Stat> = Vec::new();
    let mut parse_failures: Vec<String> = Vec::new();

    let entries = std::fs::read_dir(&dir).expect("读取目录失败");
    let mut paths: Vec<_> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    paths.sort();

    for path in paths {
        let ext = path.extension().and_then(|e| e.to_str());
        if ext != Some("odp") && ext != Some("fodp") {
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

        let doc = if ext == Some("fodp") {
            RoeDocument::open_fodp_bytes(&data)
        } else {
            RoeDocument::open_odf_bytes(&data)
        };
        let doc = match doc {
            Ok(d) => d,
            Err(e) => {
                parse_failures.push(format!("{file_name}: 解析失败 {e:?}"));
                continue;
            }
        };
        let pres = match doc {
            RoeDocument::Impress(p) => p,
            other => {
                parse_failures.push(format!("{file_name}: 不是 Impress 文档 ({other:?} variant)"));
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

    stats.retain(|s| s.shape_count > 0);
    stats.sort_by(|a, b| {
        let a_clean = a.table_shapes == 0 && a.chart_shapes == 0 && a.group_shapes == 0
            && a.roundrect_or_triangle == 0;
        let b_clean = b.table_shapes == 0 && b.chart_shapes == 0 && b.group_shapes == 0
            && b.roundrect_or_triangle == 0;
        b_clean.cmp(&a_clean).then(a.shape_count.cmp(&b.shape_count))
    });

    println!("=== 解析失败 {} 个 ===", parse_failures.len());
    for f in &parse_failures {
        println!("  ! {f}");
    }
    println!();
    println!("=== 候选样本（干净优先[无table/chart/group/roundrect/triangle]、shape_count 从小到大排序）===");
    println!(
        "{:<55} slides shapes text img empty tbl chart grp rrTri",
        "file"
    );
    for s in stats.iter().take(40) {
        println!(
            "{:<55} {:>6} {:>6} {:>4} {:>3} {:>5} {:>3} {:>5} {:>3} {:>5}",
            s.file, s.slide_count, s.shape_count, s.text_shapes, s.image_shapes,
            s.empty_shapes, s.table_shapes, s.chart_shapes, s.group_shapes,
            s.roundrect_or_triangle
        );
    }
}
