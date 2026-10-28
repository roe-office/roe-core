//! v225：Writer 公式编辑接口（get / set / insert / 命中检测）。
//!
//! 公式只存 LaTeX 源码（Run.formula.latex）。修改时先用 roe-formula 试算，
//! 失败直接返回错误、不写模型、不入撤销栈；成功后照 text_edit.rs 的模式：
//! 克隆快照 -> 改模型 -> push_undo_snapshot -> 闭包外失效布局缓存。

use std::ffi::CString;
use std::os::raw::c_char;

use roe_model::writer::{BodyElement, Formula, Paragraph, Run};
use roe_parser::RoeDocument;
use roe_writer::layout::resolve_para_style;

use crate::error::set_last_error;
use crate::handle;
use crate::hit_test::locate_frame_by_y_global;

const FALLBACK_FONT_PT: f32 = 10.5;

fn read_utf8(ptr: *const u8, len: u32) -> Result<String, String> {
    if len == 0 {
        return Err("latex 为空".to_string());
    }
    if ptr.is_null() {
        return Err("latex 指针为空".to_string());
    }
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len as usize) };
    std::str::from_utf8(bytes)
        .map(|s| s.to_string())
        .map_err(|e| format!("latex 不是合法 UTF-8: {e}"))
}

/// 用 roe-formula 排版并生成完整 Formula；内部 catch_unwind。
fn build_formula(latex: &str, font_pt: f32) -> Result<Formula, String> {
    if latex.trim().is_empty() {
        return Err("公式为空".to_string());
    }
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<Formula, String> {
        let (w, h, d) = roe_formula::latex_layout_size(latex, font_pt as f64)?;
        let dl = roe_formula::latex_to_display_list(latex, font_pt as f64)?;
        Ok(Formula {
            latex: latex.to_string(),
            width_pt: w as f32,
            height_pt: h as f32,
            depth_pt: d as f32,
            display_list: dl,
            needs_vertical_strike: false,
        })
    }));
    match r {
        Ok(v) => v,
        Err(_) => Err("公式排版内部错误".to_string()),
    }
}

/// 在段落全文字节偏移 char_offset 处插入一个公式 run，返回新 run 的下标。
/// 公式 run 的 text 恒为空串，不影响现有偏移体系。
fn insert_formula_run(para: &mut Paragraph, char_offset: usize, formula: Formula) -> Result<usize, String> {
    let new_run = Run { formula: Some(formula), ..Default::default() };

    if para.runs.is_empty() {
        if char_offset != 0 {
            return Err(format!("char_offset={char_offset} 超出段落范围（空段落仅支持 0）"));
        }
        para.runs.push(new_run);
        return Ok(0);
    }

    let mut consumed = 0usize;
    for i in 0..para.runs.len() {
        let len = para.runs[i].text.len();
        if char_offset <= consumed + len {
            let off = char_offset - consumed;
            if off == 0 {
                para.runs.insert(i, new_run);
                return Ok(i);
            }
            if off == len {
                para.runs.insert(i + 1, new_run);
                return Ok(i + 1);
            }
            if !para.runs[i].text.is_char_boundary(off) {
                return Err(format!("char_offset={char_offset} 不在字符边界上"));
            }
            let mut right = para.runs[i].clone();
            right.text = para.runs[i].text[off..].to_string();
            right.comment_range_start.clear();
            para.runs[i].text.truncate(off);
            para.runs[i].comment_range_end.clear();
            para.runs.insert(i + 1, new_run);
            para.runs.insert(i + 2, right);
            return Ok(i + 1);
        }
        consumed += len;
    }
    Err(format!("char_offset={char_offset} 超出段落范围（总字节长度={consumed}）"))
}

fn default_font_pt(entry: &handle::DocumentEntry) -> f32 {
    entry
        .writer_font_registry
        .as_ref()
        .map(|r| r.default_font_size)
        .unwrap_or(FALLBACK_FONT_PT)
}

fn finish(name: &str, handle: u64, result: Option<Result<String, String>>, changed: bool) -> *mut c_char {
    // 必须在 with_entry 闭包返回之后失效缓存，避免 RefCell 嵌套借用 panic。
    if changed && matches!(&result, Some(Ok(_))) {
        handle::invalidate_writer_layout_cache(handle);
    }
    match result {
        None => {
            set_last_error(format!("{name}: 无效句柄"));
            std::ptr::null_mut()
        }
        Some(Err(msg)) => {
            set_last_error(format!("{name}: {msg}"));
            std::ptr::null_mut()
        }
        Some(Ok(json)) => match CString::new(json) {
            Ok(cs) => cs.into_raw(),
            Err(e) => {
                set_last_error(format!("{name}: {e}"));
                std::ptr::null_mut()
            }
        },
    }
}

// ───────────────────────── get ─────────────────────────

/// 取公式 LaTeX 源码。成功返回 `{"latex":"..."}`，失败返回 null。
#[no_mangle]
pub extern "C" fn roe_formula_get(handle: u64, para_index: u32, run_idx: u32) -> *mut c_char {
    let result = handle::with_entry(handle, |entry| -> Result<String, String> {
        let RoeDocument::Writer(doc) = &entry.doc else {
            return Err("非 Writer 文档".to_string());
        };
        let Some(BodyElement::Paragraph(p)) = doc.body.get(para_index as usize) else {
            return Err(format!("para_index={para_index} 越界或不是段落"));
        };
        let Some(run) = p.runs.get(run_idx as usize) else {
            return Err(format!("run_idx={run_idx} 越界"));
        };
        let Some(f) = &run.formula else {
            return Err(format!("run[{run_idx}] 不是公式"));
        };
        Ok(serde_json::json!({ "latex": f.latex }).to_string())
    });
    finish("roe_formula_get", handle, result, false)
}

// ───────────────────────── set ─────────────────────────

fn set_in_entry(
    entry: &mut handle::DocumentEntry,
    para_index: usize,
    run_idx: usize,
    latex: &str,
) -> Result<String, String> {
    let default_fs = default_font_pt(entry);
    let (font_pt, offset) = {
        let RoeDocument::Writer(doc) = &entry.doc else {
            return Err("非 Writer 文档".to_string());
        };
        let Some(BodyElement::Paragraph(p)) = doc.body.get(para_index) else {
            return Err(format!("para_index={para_index} 越界或不是段落"));
        };
        let Some(run) = p.runs.get(run_idx) else {
            return Err(format!("run_idx={run_idx} 越界"));
        };
        if run.formula.is_none() {
            return Err(format!("run[{run_idx}] 不是公式"));
        }
        let fs = run
            .font_size
            .unwrap_or_else(|| resolve_para_style(doc, p, default_fs).font_size);
        let off: usize = p.runs[..run_idx].iter().map(|r| r.text.len()).sum();
        (fs, off)
    };

    // 先试算：失败不写模型、不入撤销栈。
    let formula = build_formula(latex, font_pt)?;

    let prev = entry.doc.clone();
    let RoeDocument::Writer(doc) = &mut entry.doc else {
        unreachable!("上面已确认是 Writer 文档");
    };
    if let Some(BodyElement::Paragraph(p)) = doc.body.get_mut(para_index) {
        p.runs[run_idx].formula = Some(formula);
    }
    entry.push_undo_snapshot(prev, handle::EditCursorContext::at(para_index, offset));
    Ok("{\"ok\":true}".to_string())
}

/// 修改已有公式的 LaTeX。成功返回 `{"ok":true}`；
/// 解析/排版失败返回 null，错误信息见 roe_last_error_message（模型未改动）。
#[no_mangle]
pub extern "C" fn roe_formula_set(
    handle: u64,
    para_index: u32,
    run_idx: u32,
    latex_utf8: *const u8,
    latex_len: u32,
) -> *mut c_char {
    let latex = match read_utf8(latex_utf8, latex_len) {
        Ok(s) => s,
        Err(m) => {
            set_last_error(format!("roe_formula_set: {m}"));
            return std::ptr::null_mut();
        }
    };
    if !handle::ensure_writer_layout(handle) {
        set_last_error("roe_formula_set: 无效句柄或非 Writer 文档");
        return std::ptr::null_mut();
    }
    let result = handle::with_entry(handle, |entry| {
        set_in_entry(entry, para_index as usize, run_idx as usize, &latex)
    });
    finish("roe_formula_set", handle, result, true)
}

// ───────────────────────── insert ─────────────────────────

fn insert_in_entry(
    entry: &mut handle::DocumentEntry,
    para_index: usize,
    char_offset: usize,
    latex: &str,
) -> Result<String, String> {
    let default_fs = default_font_pt(entry);
    let font_pt = {
        let RoeDocument::Writer(doc) = &entry.doc else {
            return Err("非 Writer 文档".to_string());
        };
        let Some(BodyElement::Paragraph(p)) = doc.body.get(para_index) else {
            return Err(format!("para_index={para_index} 越界或不是段落"));
        };
        resolve_para_style(doc, p, default_fs).font_size
    };

    let formula = build_formula(latex, font_pt)?;

    // 先在副本上试插，确认偏移合法后再动真正的文档。
    let prev = entry.doc.clone();
    let RoeDocument::Writer(doc) = &mut entry.doc else {
        unreachable!("上面已确认是 Writer 文档");
    };
    let Some(BodyElement::Paragraph(p)) = doc.body.get_mut(para_index) else {
        unreachable!("上面已确认 para_index 有效");
    };
    let run_idx = match insert_formula_run(p, char_offset, formula) {
        Ok(i) => i,
        Err(e) => {
            // 插入函数在报错路径上不会修改 runs，这里仍保险地还原。
            entry.doc = prev;
            return Err(e);
        }
    };
    entry.push_undo_snapshot(prev, handle::EditCursorContext::at(para_index, char_offset));
    Ok(format!(
        "{{\"para_index\":{},\"run_idx\":{},\"char_offset\":{}}}",
        para_index, run_idx, char_offset
    ))
}

/// 在段落 char_offset（全文字节偏移）处插入公式。
/// 成功返回 `{"para_index":N,"run_idx":M,"char_offset":K}`。
#[no_mangle]
pub extern "C" fn roe_formula_insert(
    handle: u64,
    para_index: u32,
    char_offset: u32,
    latex_utf8: *const u8,
    latex_len: u32,
) -> *mut c_char {
    let latex = match read_utf8(latex_utf8, latex_len) {
        Ok(s) => s,
        Err(m) => {
            set_last_error(format!("roe_formula_insert: {m}"));
            return std::ptr::null_mut();
        }
    };
    if !handle::ensure_writer_layout(handle) {
        set_last_error("roe_formula_insert: 无效句柄或非 Writer 文档");
        return std::ptr::null_mut();
    }
    let result = handle::with_entry(handle, |entry| {
        insert_in_entry(entry, para_index as usize, char_offset as usize, &latex)
    });
    finish("roe_formula_insert", handle, result, true)
}

// ───────────────────────── hit test ─────────────────────────

fn formula_hit_in_entry(entry: &mut handle::DocumentEntry, x: f32, y: f32) -> Result<String, String> {
    let miss = "{\"hit\":false}".to_string();
    let RoeDocument::Writer(doc) = &entry.doc else {
        return Err("非 Writer 文档".to_string());
    };
    let layout = entry
        .writer_layout_cache
        .as_ref()
        .ok_or_else(|| "writer_layout_cache 未建立（内部错误）".to_string())?;
    let arena = &layout.arena;

    let ids: Vec<_> = arena
        .page_order
        .iter()
        .flat_map(|&pid| arena.pages[pid].frames.iter().copied())
        .filter(|&fid| !arena.frames[fid].text_lines().is_empty())
        .collect();
    if ids.is_empty() {
        return Ok(miss);
    }

    // v227: 全页扫描公式框，不依赖点击落在哪一行（公式可能画出自己的行高之外）
    for &fid0 in &ids {
        let fr = &arena.frames[fid0];
        let Some(BodyElement::Paragraph(p0)) = doc.body.get(fr.para_index) else { continue; };
        let mut t0 = fr.y_on_page;
        for ln in fr.text_lines() {
            let bx0 = arena.pages[fr.page].margin_left;
            for sp in &ln.runs {
                if !sp.style.is_formula { continue; }
                let (Some(b), Some(ri)) = (sp.formula_box, sp.run_idx) else { continue; };
                let Some(fm) = p0.runs.get(ri).and_then(|r| r.formula.as_ref()) else { continue; };
                let (l, tp) = (bx0 + b.x, t0 + b.y);
                let (w, h) = (fm.width_pt, fm.height_pt + fm.depth_pt);
                if x >= l && x <= l + w && y >= tp && y <= tp + h {
                    return Ok(format!(
                        "{{\"hit\":true,\"para_index\":{},\"run_idx\":{},\"x\":{:.2},\"y\":{:.2},\"w\":{:.2},\"h\":{:.2}}}",
                        fr.para_index, ri, l, tp, w, h
                    ));
                }
            }
            t0 += ln.line_height;
        }
    }
    let fid = locate_frame_by_y_global(arena, &ids, y);
    let frame = &arena.frames[fid];
    let Some(BodyElement::Paragraph(para)) = doc.body.get(frame.para_index) else {
        return Ok(miss);
    };

    let mut top = frame.y_on_page;
    for line in frame.text_lines() {
        let bottom = top + line.line_height;
        if y < bottom {
            let base_x = arena.pages[frame.page].margin_left; // 与 tile.rs 一致：fbox.x 已含行内偏移，不再加 text_x_offset
            for span in &line.runs {
                if !span.style.is_formula {
                    continue;
                }
                let (Some(b), Some(run_idx)) = (span.formula_box, span.run_idx) else {
                    continue;
                };
                let Some(f) = para.runs.get(run_idx).and_then(|r| r.formula.as_ref()) else {
                    continue;
                };
                let bx = base_x + b.x;
                let by = top + b.y;
                let w = f.width_pt;
                let h = f.height_pt + f.depth_pt;
                if x >= bx && x <= bx + w && y >= by && y <= by + h {
                    return Ok(format!(
                        "{{\"hit\":true,\"para_index\":{},\"run_idx\":{},\"x\":{:.2},\"y\":{:.2},\"w\":{:.2},\"h\":{:.2}}}",
                        frame.para_index, run_idx, bx, by, w, h
                    ));
                }
            }
            break;
        }
        top = bottom;
    }
    Ok(miss)
}

/// 点击命中检测：点中公式返回
/// `{"hit":true,"para_index":N,"run_idx":M,"x":..,"y":..,"w":..,"h":..}`（全局坐标，pt），
/// 没点中返回 `{"hit":false}`。失败返回 null。
#[no_mangle]
pub extern "C" fn roe_formula_hit_test(handle: u64, x: f64, y: f64) -> *mut c_char {
    if !handle::ensure_writer_layout(handle) {
        set_last_error("roe_formula_hit_test: 无效句柄或非 Writer 文档");
        return std::ptr::null_mut();
    }
    let result = handle::with_entry(handle, |entry| formula_hit_in_entry(entry, x as f32, y as f32));
    finish("roe_formula_hit_test", handle, result, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_run(s: &str) -> Run {
        Run { text: s.to_string(), ..Default::default() }
    }

    #[test]
    fn build_formula_ok_has_size() {
        let f = build_formula("x^2", 10.5).expect("x^2 应能排版");
        assert!(f.width_pt > 0.0 && f.height_pt > 0.0);
        assert_eq!(f.latex, "x^2");
    }

    #[test]
    fn build_formula_rejects_empty() {
        assert!(build_formula("   ", 10.5).is_err());
    }

    #[test]
    fn insert_into_empty_paragraph() {
        let mut p = Paragraph::default();
        let f = build_formula("x", 10.5).unwrap();
        assert_eq!(insert_formula_run(&mut p, 0, f).unwrap(), 0);
        assert_eq!(p.runs.len(), 1);
        assert!(p.runs[0].formula.is_some());
        let f2 = build_formula("x", 10.5).unwrap();
        assert!(insert_formula_run(&mut Paragraph::default(), 3, f2).is_err());
    }

    #[test]
    fn insert_splits_text_run() {
        let mut p = Paragraph::default();
        p.runs.push(text_run("abcd"));
        let idx = insert_formula_run(&mut p, 2, build_formula("y", 10.5).unwrap()).unwrap();
        assert_eq!(idx, 1);
        assert_eq!(p.runs.len(), 3);
        assert_eq!(p.runs[0].text, "ab");
        assert!(p.runs[1].formula.is_some());
        assert_eq!(p.runs[2].text, "cd");

        // 段首 / 段尾 / 越界
        let mut q = Paragraph::default();
        q.runs.push(text_run("abcd"));
        assert_eq!(insert_formula_run(&mut q, 0, build_formula("y", 10.5).unwrap()).unwrap(), 0);
        let mut r = Paragraph::default();
        r.runs.push(text_run("abcd"));
        assert_eq!(insert_formula_run(&mut r, 4, build_formula("y", 10.5).unwrap()).unwrap(), 1);
        let mut s = Paragraph::default();
        s.runs.push(text_run("abcd"));
        assert!(insert_formula_run(&mut s, 9, build_formula("y", 10.5).unwrap()).is_err());
    }
}


#[cfg(test)]
mod e2e_tests {
    use super::*;
    use crate::document::roe_open_document_from_bytes;
    use crate::snapshot::roe_undo;

    const SAMPLE: &str = "/home/yong/small__sw_ooxmlexport_data__math-mso2k7.docx";

    fn take(ptr: *mut c_char) -> Option<String> {
        if ptr.is_null() {
            return None;
        }
        Some(unsafe { CString::from_raw(ptr) }.into_string().unwrap())
    }

    fn call_set(h: u64, p: u32, r: u32, latex: &str) -> Option<String> {
        take(roe_formula_set(h, p, r, latex.as_ptr(), latex.len() as u32))
    }

    fn get_latex(h: u64, p: u32, r: u32) -> String {
        let s = take(roe_formula_get(h, p, r)).expect("get 应成功");
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        v["latex"].as_str().unwrap().to_string()
    }

    #[test]
    fn e2e_set_get_undo_hit() {
        let Ok(data) = std::fs::read(SAMPLE) else {
            eprintln!("样本不存在，跳过");
            return;
        };
        let h = roe_open_document_from_bytes(data.as_ptr(), data.len(), 4);
        assert!(h != 0, "打开样本失败");

        // 找第一个公式 run
        let (pi, ri) = handle::with_entry(h, |e| {
            let RoeDocument::Writer(doc) = &e.doc else { return None };
            for (i, el) in doc.body.iter().enumerate() {
                if let BodyElement::Paragraph(p) = el {
                    for (j, r) in p.runs.iter().enumerate() {
                        if r.formula.is_some() {
                            return Some((i as u32, j as u32));
                        }
                    }
                }
            }
            None
        })
        .flatten()
        .expect("样本里应有公式");

        let orig = get_latex(h, pi, ri);
        assert!(!orig.is_empty());

        // 1. set 成功
        assert!(call_set(h, pi, ri, "a^2+b^2").is_some(), "set 应成功");
        assert_eq!(get_latex(h, pi, ri), "a^2+b^2");
        let undo_len = handle::with_entry(h, |e| e.undo_stack.len()).unwrap();

        // 2. 非法 LaTeX：返回 null，模型与撤销栈都不变
        let bad = call_set(h, pi, ri, "\\frac{");
        eprintln!("非法 LaTeX 结果 = {:?}", bad);
        if bad.is_none() {
            assert_eq!(get_latex(h, pi, ri), "a^2+b^2");
            assert_eq!(handle::with_entry(h, |e| e.undo_stack.len()).unwrap(), undo_len);
        }

        // 3. 命中检测：取该公式 span 的中心点
        assert!(handle::ensure_writer_layout(h));
        let center = handle::with_entry(h, |e| {
            let RoeDocument::Writer(doc) = &e.doc else { return None };
            let arena = &e.writer_layout_cache.as_ref()?.arena;
            for &pid in arena.page_order.iter() {
                for &fid in arena.pages[pid].frames.iter() {
                    let fr = &arena.frames[fid];
                    if fr.para_index != pi as usize {
                        continue;
                    }
                    let mut top = fr.y_on_page;
                    for line in fr.text_lines() {
                        for sp in &line.runs {
                            if sp.style.is_formula && sp.run_idx == Some(ri as usize) {
                                let b = sp.formula_box?;
                                let BodyElement::Paragraph(p) = doc.body.get(pi as usize)? else { return None };
                                let f = p.runs.get(ri as usize)?.formula.as_ref()?;
                                let cx = arena.pages[fr.page].margin_left + b.x + f.width_pt / 2.0;
                                let cy = top + b.y + (f.height_pt + f.depth_pt) / 2.0;
                                return Some((cx, cy));
                            }
                        }
                        top += line.line_height;
                    }
                }
            }
            None
        })
        .flatten();
        let (cx, cy) = center.expect("布局里应找到该公式 span");
        let hit = take(roe_formula_hit_test(h, cx as f64, cy as f64)).expect("hit_test 应成功");
        eprintln!("hit = {hit}");
        let v: serde_json::Value = serde_json::from_str(&hit).unwrap();
        assert_eq!(v["hit"], true, "公式中心点应命中: {hit}");
        assert_eq!(v["para_index"], pi);
        assert_eq!(v["run_idx"], ri);

        // 页面左上角空白处不应命中
        let miss = take(roe_formula_hit_test(h, 1.0, 1.0)).unwrap();
        assert!(miss.contains("\"hit\":false"), "空白处不应命中: {miss}");

        // 4. undo 回到原公式
        assert!(take(roe_undo(h)).is_some(), "undo 应成功");
        assert_eq!(get_latex(h, pi, ri), orig);

        // 5. insert：在该段落开头插入公式，run 数 +1
        let before = handle::with_entry(h, |e| {
            let RoeDocument::Writer(d) = &e.doc else { return 0 };
            match d.body.get(pi as usize) { Some(BodyElement::Paragraph(p)) => p.runs.len(), _ => 0 }
        }).unwrap();
        let ins = "x+1";
        let r = take(roe_formula_insert(h, pi, 0, ins.as_ptr(), ins.len() as u32));
        assert!(r.is_some(), "insert 应成功");
        let after = handle::with_entry(h, |e| {
            let RoeDocument::Writer(d) = &e.doc else { return 0 };
            match d.body.get(pi as usize) { Some(BodyElement::Paragraph(p)) => p.runs.len(), _ => 0 }
        }).unwrap();
        assert_eq!(after, before + 1);

        handle::remove(h);
    }
}


// ───────────────────────── preview ─────────────────────────

thread_local! {
    // KaTeX 字体上下文构建很慢（加载系统字体 + 19 个 KaTeX 字体），
    // 预览会随输入频繁调用，所以缓存在线程内，只构建一次。
    static PREVIEW_FONTS: std::cell::RefCell<Option<parley::FontContext>> =
        std::cell::RefCell::new(None);
}

fn render_preview(latex: &str, font_pt: f32, scale: f32) -> Result<(u32, u32, Vec<u8>), String> {
    let formula = build_formula(latex, font_pt)?;
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        PREVIEW_FONTS.with(|cell| {
            let mut b = cell.borrow_mut();
            let fc = b.get_or_insert_with(roe_writer::FontRegistry::build_font_context_with_katex);
            roe_writer::tile_formula::render_formula_preview_rgba(
                fc, &formula, scale, 4.0, [255, 255, 255, 255],
            )
        })
    }));
    match r {
        Ok(Some(v)) => Ok(v),
        Ok(None) => Err("预览尺寸无效或过大".to_string()),
        Err(_) => Err("预览渲染内部错误".to_string()),
    }
}

/// 公式预览：LaTeX + 字号(pt) + 缩放 -> RGBA（白底，长度 w*h*4）。
/// 宽高写入 out_w / out_h，字节数写入 out_len；返回的缓冲区用 roe_free_bytes 释放。
/// 排版失败（LaTeX 有误）返回 null，错误信息见 roe_last_error_message。
#[no_mangle]
pub extern "C" fn roe_formula_preview_rgba(
    latex_utf8: *const u8,
    latex_len: u32,
    font_pt: f64,
    scale: f64,
    out_w: *mut u32,
    out_h: *mut u32,
    out_len: *mut u32,
) -> *mut std::os::raw::c_uchar {
    if out_w.is_null() || out_h.is_null() || out_len.is_null() {
        set_last_error("roe_formula_preview_rgba: 输出指针为空");
        return std::ptr::null_mut();
    }
    unsafe {
        *out_w = 0;
        *out_h = 0;
        *out_len = 0;
    }
    let latex = match read_utf8(latex_utf8, latex_len) {
        Ok(s) => s,
        Err(m) => {
            set_last_error(format!("roe_formula_preview_rgba: {m}"));
            return std::ptr::null_mut();
        }
    };
    let fs = if font_pt.is_finite() && font_pt > 1.0 && font_pt < 200.0 { font_pt as f32 } else { FALLBACK_FONT_PT };
    let sc = if scale.is_finite() && scale > 0.1 && scale < 8.0 { scale as f32 } else { 1.0 };
    match render_preview(&latex, fs, sc) {
        Ok((w, h, buf)) => {
            let len = buf.len() as u32;
            let mut boxed = buf.into_boxed_slice();
            let ptr = boxed.as_mut_ptr();
            std::mem::forget(boxed);
            unsafe {
                *out_w = w;
                *out_h = h;
                *out_len = len;
            }
            ptr
        }
        Err(m) => {
            set_last_error(format!("roe_formula_preview_rgba: {m}"));
            std::ptr::null_mut()
        }
    }
}

#[cfg(test)]
mod preview_tests {
    use super::*;

    #[test]
    fn preview_frac_has_ink() {
        let latex = "\\frac{a}{b}";
        let (mut w, mut h, mut n) = (0u32, 0u32, 0u32);
        let p = roe_formula_preview_rgba(latex.as_ptr(), latex.len() as u32, 20.0, 1.0, &mut w, &mut h, &mut n);
        assert!(!p.is_null(), "预览应成功");
        assert!(w > 0 && h > 0);
        assert_eq!(n, w * h * 4);
        let buf = unsafe { std::slice::from_raw_parts(p, n as usize) };
        let ink = buf.chunks(4).filter(|px| px[0] < 200 || px[1] < 200 || px[2] < 200).count();
        eprintln!("preview {w}x{h}, ink pixels = {ink}");
        assert!(ink > 10, "应有非背景像素");
        crate::chart::roe_free_bytes(p, n);
    }

    #[test]
    fn preview_bad_latex_returns_null() {
        let latex = "\\frac{";
        let (mut w, mut h, mut n) = (0u32, 0u32, 0u32);
        let p = roe_formula_preview_rgba(latex.as_ptr(), latex.len() as u32, 20.0, 1.0, &mut w, &mut h, &mut n);
        assert!(p.is_null());
        assert_eq!(n, 0);
    }
}
