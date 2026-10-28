use std::path::Path;
use roe_writer::layout::ParagraphLayoutEngine;
use roe_writer::tile::WriterTileRenderer;
use roe_writer::FontRegistry;
use roe_writer::Margins;
use roe_parser::RoeDocument;

/// v152起手：批注气泡功能最小可见版本。
/// 目标：验证"气泡位置对不对"——画气泡框（灰底+边框+圆角）+左边色条+
/// 虚线连接线，暂不画头像/按钮/文字内容（v151 2.2节完整方案的后续步骤，
/// 气泡尺寸目前是猜的固定值130x80pt，等加了文字才知道真实需要多大）。
///
/// 不改 render_viewport 本身、不改 page.width：视口比页面额外加宽
/// BUBBLE_BAR_WIDTH_PT，气泡画在这块新增区域里（v151 2.1节方案）。
fn main() {
    let args: Vec<String> = std::env::args().collect();
    const USAGE: &str = "usage: render_with_comments <input.docx> [output_dir]";
    let input_path = args.get(1).expect(USAGE);
    let default_output_dir = std::env::temp_dir().join("roe_render_with_comments").to_string_lossy().to_string();
    let output_dir: &str = args.get(2).map(|s| s.as_str()).unwrap_or(&default_output_dir);
    println!("output_dir: {}", output_dir);
    std::fs::create_dir_all(output_dir).expect("create output dir");

    let filename = Path::new(input_path).file_stem().unwrap().to_string_lossy().to_string();
    println!("=== {} ===", filename);

    let data = std::fs::read(input_path).expect("read file");
    let doc = RoeDocument::open_docx_bytes(&data).expect("parse docx");
    let wdoc = match doc {
        RoeDocument::Writer(d) => d,
        _ => panic!("not a Writer document"),
    };

    println!("  doc.comments.len(): {}", wdoc.comments.len());
    for c in &wdoc.comments {
        let text = WriterTileRenderer::comment_content_to_plain_text(&c.content);
        let preview: String = text.chars().take(40).collect();
        println!(
            "    [comment id={}] author={:?} done={} parent_id={:?} content_preview={:?}",
            c.id, c.author, c.done, c.parent_id, preview
        );
    }

    let mut registry = FontRegistry::new_with_system_fonts();
    let font_context = std::mem::replace(&mut registry.font_context, parley::FontContext::new());
    let mut engine = ParagraphLayoutEngine::new(registry);
    engine.page_width  = wdoc.page_size.0;
    engine.page_height = wdoc.page_size.1;
    engine.margins = Margins {
        top:    wdoc.margin_top,
        bottom: wdoc.margin_bottom,
        left:   wdoc.margin_left,
        right:  wdoc.margin_right,
    };
    let result = engine.layout(&wdoc);
    let arena = &result.arena;
    println!("  pages: {}", arena.page_order.len());

    const BUBBLE_BAR_WIDTH_PT: f32 = 170.0; // 气泡栏宽度：150气泡宽+10间距+10右边距，避免气泡被裁到画布外
    const BUBBLE_W_PT: f32 = 150.0;         // 气泡框自身宽度（用户反馈：偏窄，130→150）
    const BUBBLE_GAP_PT: f32 = 8.0;         // 气泡间垂直间距（v151 2.4节）
    const HIGHLIGHT_TO_BUBBLE_GAP_PT: f32 = 10.0; // 高亮右侧到气泡左边缘的间距（v151 2.1节）

    let mut renderer = WriterTileRenderer::new_with_fonts(font_context, ());
    for (i, &pid) in arena.page_order.iter().enumerate().take(10) {
        let page = &arena.pages[pid];
        let scale = 2.0f32;
        let vp_w = page.width + BUBBLE_BAR_WIDTH_PT;
        let pw = (vp_w * scale) as u32;
        let ph = (page.height * scale) as u32;
        if pw == 0 || ph == 0 { println!("  page {}: SKIP invalid size", i); continue; }
        let vp_y = arena.page_global_y(pid);
        let mut buf = vec![0u8; (pw as usize) * (ph as usize) * 4];

        match renderer.render_viewport(arena, &wdoc, 0.0, vp_y, vp_w, page.height, pw, ph, &mut buf) {
            Ok(_) => {
                let hits = renderer.take_comment_hits();
                println!("  page {} comment hits: {} 条", i, hits.len());

                // 按line_top_pt排序，从上到下纵向排布，简单贪心防重叠
                // （v151 2.4节：撞了就整体下移，不做力导向布局）。
                // 这一步不实现"挤不下则整条隐藏"——气泡尺寸还是猜的，
                // 等文字内容确定、真实高度算出来后再做这部分。
                let mut sorted_hits = hits.clone();
                sorted_hits.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());

                {
                    // 直接在已经画好页面内容的 buf 上继续画气泡，不经过
                    // tiny_skia::Pixmap 这层封装再倒腾回 PixmapMut——
                    // 那样会导致同时对同一个 pixmap 做可变+不可变借用，
                    // 触发borrow checker报错。PixmapMut::from_bytes 直接
                    // 包住 &mut buf 即可，只有一个可变借用，干净。
                    let mut pixmap = tiny_skia::PixmapMut::from_bytes(&mut buf, pw, ph)
                        .expect("PixmapMut::from_bytes failed for bubble drawing pass");

                    let mut cursor_y_pt = 0.0f32;
                    for (idx, (cid, line_top_pt, line_bottom_pt, color_idx)) in sorted_hits.iter().enumerate() {
                        let comment = wdoc.comments.iter().find(|c| c.id == *cid);
                        let author = comment.map(|c| c.author.as_str()).unwrap_or("<未找到>");
                        let content_preview = comment
                            .map(|c| WriterTileRenderer::comment_content_to_plain_text(&c.content))
                            .unwrap_or_default();
                        let y_in_page_pt = line_top_pt - vp_y;
                        println!(
                            "    comment_id={} author={:?} line_top_pt={:.2}pt (页内相对y={:.2}pt) color_idx={}",
                            cid, author, line_top_pt, y_in_page_pt, color_idx
                        );

                        // v154新增：动态测量气泡实际所需高度，不再用固定BUBBLE_H_PT。
                        // measure函数返回px，这里换算回pt，保持与其他坐标同一单位体系。
                        // 找到这条批注对应的讨论串，取出回复列表（可能为空）
                        let threads = wdoc.comment_threads();
                        let replies_owned: Vec<&roe_model::writer::Comment> = threads.iter()
                            .find(|t| t.root.id == *cid)
                            .map(|t| t.replies.clone())
                            .unwrap_or_default();
                        let replies_slice: &[&roe_model::writer::Comment] = &replies_owned;

                        let bubble_h_px_measured = renderer.measure_comment_bubble_height(
                            BUBBLE_W_PT * scale, &content_preview, scale, replies_slice,
                        );
                        let bubble_h_pt = bubble_h_px_measured / scale;
                        eprintln!(
                            "    [DEBUG] comment_id={} content_preview_len={} bubble_h_px_measured={:.2} bubble_h_pt={:.2}",
                            cid, content_preview.chars().count(), bubble_h_px_measured, bubble_h_pt
                        );

                        let desired_y_pt = y_in_page_pt;
                        let actual_y_pt = if idx == 0 { desired_y_pt } else { cursor_y_pt.max(desired_y_pt) };
                        cursor_y_pt = actual_y_pt + bubble_h_pt + BUBBLE_GAP_PT;

                        // 连接线起点：正文区右边界（page.width），批注
                        // 对应行的粗略垂直位置（+6pt估算单行高度的一半，
                        // 这一步不精确计算真实行高，只验证连接线方向）。
                        // 连接线起点右移2pt，避开page.width处画的1px页面
                        // 边框线（两者原本画在同一条x坐标上，边框的实心
                        // 灰线会盖住虚线的起始一截，视觉上像是线被截断）。
                        const CONNECTOR_START_OFFSET_PT: f32 = 2.0;
                        // y坐标改用line_bottom_pt（行底部，全局文档坐标）减去vp_y
                        // 换算成页内相对坐标，与正文内连接线的bottom_y（cm_y0+cm_h，
                        // 同样是"行底部"概念）保持一致，两段线能在同一水平线对齐。
                        let bottom_y_in_page_pt = line_bottom_pt - vp_y;
                        let connector_from_px = (
                            (page.width + CONNECTOR_START_OFFSET_PT) * scale,
                            bottom_y_in_page_pt * scale,
                        );

                        let bubble_x_pt = page.width + HIGHLIGHT_TO_BUBBLE_GAP_PT;
                        let bubble_x_px = bubble_x_pt * scale;
                        let bubble_y_px = actual_y_pt * scale;
                        let bubble_w_px = BUBBLE_W_PT * scale;
                        let bubble_h_px = bubble_h_pt * scale;
                        // 连接线终点：气泡左边缘的色条中点附近（v151 2.3节：
                        // 视觉上"接到"色条，不是接到灰色气泡框本身）。
                        let connector_to_px = (bubble_x_px, bubble_y_px + 10.0 * scale);

                        let initials_opt = comment.and_then(|c| c.initials.as_deref());

                        renderer.draw_comment_bubble_placeholder(
                            &mut pixmap,
                            bubble_x_px, bubble_y_px, bubble_w_px, bubble_h_px,
                            *color_idx,
                            connector_from_px,
                            connector_to_px,
                            author,
                            initials_opt,
                            &content_preview,
                            comment.map(|c| c.done).unwrap_or(false),
                            scale,
                            replies_slice,
                            comment.and_then(|c| c.date.as_deref()),
                        );
                    }
                } // pixmap 的可变借用在这里结束，下面可以自由使用 buf

                let pixmap_final = tiny_skia::Pixmap::from_vec(buf, tiny_skia::IntSize::from_wh(pw, ph).unwrap())
                    .expect("Pixmap::from_vec failed after bubble drawing");
                let png = pixmap_final.encode_png().unwrap();
                std::fs::create_dir_all(output_dir).expect("ensure output dir exists before write");
                let out_path = format!("{}/{}__page{}.png", output_dir, filename, i);
                std::fs::write(&out_path, &png).unwrap();
                println!("  page {} -> {}", i, out_path);
            }
            Err(e) => println!("  page {}: SKIP render failed: {:?}", i, e),
        }
    }
}
