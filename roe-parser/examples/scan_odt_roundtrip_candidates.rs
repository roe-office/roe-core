// v186后续：为 odt/fodt "真实文件 round-trip 测试"探测合适样本。
//
// 重要前提：本脚本基于交叉核实 fodt_writer.rs 实际代码（而非仅凭文件头
// 注释）确认的真实范围（roe-save-design.md §三 记录的教训——本项目已
// 两次发现注释滞后于代码，因此本次交叉核实了 write_body_element 的实际
// match 分支）：
//
//   完全不写出（BodyElement 级别静默丢弃，代码实测，不是注释猜测）：
//     - Frame（浮动框）
//     - Shape（形状）
//     - InlineChart（内嵌图表）
//   这三项一旦出现在样本里，写出后内容会完全消失，不适合作为 round-trip
//   样本（除非专门测试"这些内容被正确丢弃"这个反向断言，但那不是本次
//   目标）。
//
//   有写出路径，但 roundtrip 语义打折扣（非静默丢弃，是文件头注释里
//   写明的已知限制，本次未逐条重新验证每一条，只核实了"有没有写出路径
//   这个真假判断"）：
//     - Table：rows/cells/col_span/row_span/cell style 可 roundtrip；
//       table-column 实际宽度不行（parser 固定回填 60.0）
//     - Section：单向，parser 不识别 text:section，内部段落会被拍扁到
//       顶层 body（包裹结构丢失，内容不丢）
//     - hyperlink（text:a）/footnote（text:note）：单向，内部文本会被
//       当作普通文本"漏"进所在段落的 runs 里
//     - Image：完全单向，parser 不识别 draw:frame/draw:image
//     - CharStyle.color / CellStyle.bg_color/text_color：不写出
//
// 本脚本扫描 test_samples/curated/odt/ 下每个文件（同时处理 .odt 和
// .fodt 两种扩展名，分别走 package 解析和 flat 解析），统计 body 内容
// 落在"完全支持"范围内的比例，挑出"干净"（不含 Frame/Shape/InlineChart）
// 的样本，作为 round-trip 测试的真实文件输入。
//
// 用法: cargo run --example scan_odt_roundtrip_candidates -p roe-parser -- <目录>

use roe_model::writer::{BodyElement, WriterDocument};
use roe_parser::RoeDocument;

#[derive(Default)]
struct Stat {
    file: String,
    body_elem_count: usize,
    paragraph_count: usize,
    run_count: usize,
    table_count: usize,       // 有写出路径，但列宽会丢
    section_count: usize,     // 有写出路径，但包裹结构会丢（拍扁）
    image_count: usize,       // 完全单向（parser 不识别写出的标签）
    frame_count: usize,       // writer 完全不写出
    shape_count: usize,       // writer 完全不写出
    inline_chart_count: usize,// writer 完全不写出
}

fn walk(body: &[BodyElement], st: &mut Stat) {
    for elem in body {
        st.body_elem_count += 1;
        match elem {
            BodyElement::Paragraph(p) => {
                st.paragraph_count += 1;
                st.run_count += p.runs.len();
            }
            BodyElement::Table(t) => {
                st.table_count += 1;
                for row in &t.rows {
                    for cell in &row.cells {
                        for c in &cell.content {
                            if let roe_model::common::CellContent::Paragraph(p) = c {
                                st.paragraph_count += 1;
                                st.run_count += p.runs.len();
                            }
                        }
                    }
                }
            }
            BodyElement::Section(s) => {
                st.section_count += 1;
                walk(&s.content, st);
            }
            BodyElement::Image(_) => st.image_count += 1,
            BodyElement::Frame(_) => st.frame_count += 1,
            BodyElement::Shape(_) => st.shape_count += 1,
            BodyElement::InlineChart { .. } => st.inline_chart_count += 1,
            BodyElement::InlineStatChart { .. } => st.inline_chart_count += 1,
        }
    }
}

fn get_writer_doc(path: &std::path::Path) -> Result<WriterDocument, String> {
    let file_name = path.file_name().unwrap().to_string_lossy().to_string();
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let data = std::fs::read(path).map_err(|e| format!("读取失败 {e}"))?;

    let doc = if ext == "fodt" {
        RoeDocument::open_fodt_bytes(&data).map_err(|e| format!("解析失败 {e:?}"))?
    } else {
        RoeDocument::open_odf_bytes(&data).map_err(|e| format!("解析失败 {e:?}"))?
    };

    match doc {
        RoeDocument::Writer(w) => Ok(w),
        other => Err(format!("{file_name}: 不是 Writer 文档 ({other:?} variant)")),
    }
}

fn main() {
    let dir = std::env::args()
        .nth(1)
        .expect("用法: scan_odt_roundtrip_candidates <odt目录>");

    let mut stats: Vec<Stat> = Vec::new();
    let mut parse_failures: Vec<String> = Vec::new();

    let entries = std::fs::read_dir(&dir).expect("读取目录失败");
    let mut paths: Vec<_> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    paths.sort();

    for path in paths {
        let ext = path.extension().and_then(|e| e.to_str());
        if ext != Some("odt") && ext != Some("fodt") {
            continue;
        }
        let file_name = path.file_name().unwrap().to_string_lossy().to_string();

        let doc = match get_writer_doc(&path) {
            Ok(d) => d,
            Err(e) => {
                parse_failures.push(format!("{file_name}: {e}"));
                continue;
            }
        };

        let mut st = Stat {
            file: file_name,
            ..Default::default()
        };
        walk(&doc.body, &mut st);
        stats.push(st);
    }

    stats.retain(|s| s.paragraph_count > 0);
    stats.sort_by(|a, b| {
        let a_clean = a.frame_count == 0 && a.shape_count == 0 && a.inline_chart_count == 0;
        let b_clean = b.frame_count == 0 && b.shape_count == 0 && b.inline_chart_count == 0;
        b_clean.cmp(&a_clean).then(a.paragraph_count.cmp(&b.paragraph_count))
    });

    println!("=== 解析失败 {} 个 ===", parse_failures.len());
    for f in &parse_failures {
        println!("  ! {f}");
    }
    println!();
    println!("=== 候选样本（干净优先[无frame/shape/chart]、paragraph_count 从小到大排序）===");
    println!(
        "{:<65} elems paras runs table sect img frame shape chart",
        "file"
    );
    for s in stats.iter() {
        println!(
            "{:<65} {:>5} {:>5} {:>4} {:>5} {:>4} {:>3} {:>5} {:>5} {:>5}",
            s.file, s.body_elem_count, s.paragraph_count, s.run_count,
            s.table_count, s.section_count, s.image_count, s.frame_count,
            s.shape_count, s.inline_chart_count
        );
    }
}
