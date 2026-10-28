// v184后续：为 docx "真实文件 round-trip 测试"探测合适样本。
//
// docx_writer.rs 支持范围（见文件头注释）远窄于 pptx_writer：
// 只写 BodyElement::Paragraph，且仅 Paragraph.style（w:pStyle）+
// Run.text（纯文字）。Table/Section/Image/Frame/Shape/InlineChart
// 完全不写；Run 的 bold/italic/font_size/hyperlink 等字段不写；
// list_info 不写（编号文字已烧进 run 文字本身）。
//
// 本脚本扫描 test_samples/curated/docx/ 下每个文件，统计 body 内容
// 落在 writer 支持范围内的比例，挑出"干净"（几乎全是纯段落+文字）
// 的样本，作为 round-trip 测试的真实文件输入。
//
// 用法: cargo run --example scan_docx_roundtrip_candidates -p roe-parser -- <目录>

use roe_model::writer::BodyElement;

#[derive(Default)]
struct Stat {
    file: String,
    body_elem_count: usize,
    paragraph_count: usize,
    run_count: usize,
    empty_paragraphs: usize,
    styled_paragraphs: usize,   // style 非默认/非空，值得检验 style round-trip
    table_count: usize,         // writer 完全不支持
    section_count: usize,       // writer 完全不支持
    image_count: usize,         // writer 完全不支持
    frame_count: usize,         // writer 完全不支持
    shape_count: usize,         // writer 完全不支持
    inline_chart_count: usize,  // writer 完全不支持
}

fn main() {
    let dir = std::env::args()
        .nth(1)
        .expect("用法: scan_docx_roundtrip_candidates <docx目录>");

    let mut stats: Vec<Stat> = Vec::new();
    let mut parse_failures: Vec<String> = Vec::new();

    let entries = std::fs::read_dir(&dir).expect("读取目录失败");
    let mut paths: Vec<_> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    paths.sort();

    for path in paths {
        if path.extension().and_then(|e| e.to_str()) != Some("docx") {
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
        let doc = match roe_parser::ooxml::docx::DocxParser::parse(&data) {
            Ok(d) => d,
            Err(e) => {
                parse_failures.push(format!("{file_name}: 解析失败 {e:?}"));
                continue;
            }
        };

        let mut st = Stat {
            file: file_name,
            body_elem_count: doc.body.len(),
            ..Default::default()
        };

        for elem in &doc.body {
            match elem {
                BodyElement::Paragraph(p) => {
                    st.paragraph_count += 1;
                    st.run_count += p.runs.len();
                    let text_len: usize = p.runs.iter().map(|r| r.text.len()).sum();
                    if text_len == 0 {
                        st.empty_paragraphs += 1;
                    }
                    // StyleRef 的具体形状未知，用 Debug 粗略判断"非默认"
                    let style_dbg = format!("{:?}", p.style);
                    if !style_dbg.is_empty() && !style_dbg.to_lowercase().contains("none") {
                        st.styled_paragraphs += 1;
                    }
                }
                BodyElement::Table(_) => st.table_count += 1,
                BodyElement::Section(_) => st.section_count += 1,
                BodyElement::Image(_) => st.image_count += 1,
                BodyElement::Frame(_) => st.frame_count += 1,
                BodyElement::Shape(_) => st.shape_count += 1,
                BodyElement::InlineChart { .. } => st.inline_chart_count += 1,
            BodyElement::InlineStatChart { .. } => st.inline_chart_count += 1,
            }
        }

        stats.push(st);
    }

    stats.retain(|s| s.paragraph_count > 0);
    stats.sort_by(|a, b| {
        let a_clean = a.table_count == 0 && a.section_count == 0 && a.image_count == 0
            && a.frame_count == 0 && a.shape_count == 0 && a.inline_chart_count == 0;
        let b_clean = b.table_count == 0 && b.section_count == 0 && b.image_count == 0
            && b.frame_count == 0 && b.shape_count == 0 && b.inline_chart_count == 0;
        b_clean.cmp(&a_clean).then(a.paragraph_count.cmp(&b.paragraph_count))
    });

    println!("=== 解析失败 {} 个 ===", parse_failures.len());
    for f in &parse_failures {
        println!("  ! {f}");
    }
    println!();
    println!("=== 候选样本（干净优先、paragraph_count 从小到大排序）===");
    println!(
        "{:<55} elems paras runs empty styled tbl sect img frame shape chart",
        "file"
    );
    for s in stats.iter() {
        println!(
            "{:<55} {:>5} {:>5} {:>4} {:>5} {:>6} {:>3} {:>4} {:>3} {:>5} {:>5} {:>5}",
            s.file, s.body_elem_count, s.paragraph_count, s.run_count,
            s.empty_paragraphs, s.styled_paragraphs, s.table_count,
            s.section_count, s.image_count, s.frame_count, s.shape_count,
            s.inline_chart_count
        );
    }
}
