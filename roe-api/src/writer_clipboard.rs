//! v247: Writer 字符级剪贴板（应用内，进程级单槽位，可跨文档）。
//! 偏移 = 段内全部 run 文本拼接后的 UTF-8 字节偏移，与格式接口一致。
//! 只支持 body 里的普通段落；选区若跨表格/图片会拒绝。

use crate::error::{set_last_error, RoeErrorCode};
use crate::handle;
use roe_model::writer::{BodyElement, Paragraph, Run};
use roe_parser::RoeDocument;
use std::cell::RefCell;
use std::ffi::CString;
use std::os::raw::c_char;

thread_local! {
    static WCLIP: RefCell<Option<Vec<Paragraph>>> = const { RefCell::new(None) };
}

fn order(pa: u32, oa: u32, pb: u32, ob: u32) -> (usize, usize, usize, usize) {
    if (pa, oa) <= (pb, ob) {
        (pa as usize, oa as usize, pb as usize, ob as usize)
    } else {
        (pb as usize, ob as usize, pa as usize, oa as usize)
    }
}

fn para_ref(body: &[BodyElement], i: usize) -> Result<&Paragraph, String> {
    match body.get(i) {
        Some(BodyElement::Paragraph(p)) => Ok(p),
        _ => Err(format!("段落 {i} 不存在或不是普通段落")),
    }
}

fn check_body(body: &[BodyElement], pa: usize, pb: usize) -> Result<(), String> {
    if pa > pb || pb >= body.len() {
        return Err("段落下标越界".to_string());
    }
    for i in pa..=pb {
        if !matches!(body[i], BodyElement::Paragraph(_)) {
            return Err("选区包含表格或图片，暂不支持".to_string());
        }
    }
    Ok(())
}

/// 在字节偏移 offset 处把 runs 切成左右两份（必要时切开一个 run）。
fn split_runs(runs: &[Run], offset: usize) -> Result<(Vec<Run>, Vec<Run>), String> {
    let mut left: Vec<Run> = Vec::new();
    let mut right: Vec<Run> = Vec::new();
    let mut acc = 0usize;
    for r in runs {
        let len = r.text.len();
        if acc + len <= offset {
            left.push(r.clone());
        } else if acc >= offset {
            right.push(r.clone());
        } else {
            let rel = offset - acc;
            if !r.text.is_char_boundary(rel) {
                return Err("偏移不在字符边界上".to_string());
            }
            let mut l = r.clone();
            let mut rr = r.clone();
            l.text = r.text[..rel].to_string();
            rr.text = r.text[rel..].to_string();
            l.comment_range_end.clear();
            rr.comment_range_start.clear();
            rr.footnote_ref = None;
            rr.endnote_ref = None;
            rr.comment_ref = None;
            left.push(l);
            right.push(rr);
        }
        acc += len;
    }
    Ok((left, right))
}

fn text_len(runs: &[Run]) -> usize {
    runs.iter().map(|r| r.text.len()).sum()
}

fn copy_in(body: &[BodyElement], pa: usize, oa: usize, pb: usize, ob: usize) -> Result<Vec<Paragraph>, String> {
    check_body(body, pa, pb)?;
    let mut out: Vec<Paragraph> = Vec::new();
    if pa == pb {
        let p = para_ref(body, pa)?;
        let right = split_runs(&p.runs, oa)?.1;
        let mid = split_runs(&right, ob - oa)?.0;
        let mut np = p.clone();
        np.runs = mid;
        out.push(np);
    } else {
        let p = para_ref(body, pa)?;
        let mut np = p.clone();
        np.runs = split_runs(&p.runs, oa)?.1;
        out.push(np);
        for i in (pa + 1)..pb {
            out.push(para_ref(body, i)?.clone());
        }
        let p = para_ref(body, pb)?;
        let mut np = p.clone();
        np.runs = split_runs(&p.runs, ob)?.0;
        out.push(np);
    }
    for p in out.iter_mut() {
        p.bookmarks.clear();
    }
    Ok(out)
}

fn delete_in(body: &mut Vec<BodyElement>, pa: usize, oa: usize, pb: usize, ob: usize) -> Result<(), String> {
    check_body(body, pa, pb)?;
    let np = {
        let first = para_ref(body, pa)?;
        let last = para_ref(body, pb)?;
        let mut left = split_runs(&first.runs, oa)?.0;
        let right = split_runs(&last.runs, ob)?.1;
        left.extend(right);
        let mut np = first.clone();
        np.runs = left;
        np
    };
    body[pa] = BodyElement::Paragraph(np);
    if pb > pa {
        body.drain((pa + 1)..=pb);
    }
    Ok(())
}

fn paste_in(body: &mut Vec<BodyElement>, frag: &[Paragraph], p: usize, o: usize) -> Result<(usize, usize), String> {
    check_body(body, p, p)?;
    let n = frag.len();
    if n == 0 {
        return Err("剪贴板为空".to_string());
    }
    let target = para_ref(body, p)?.clone();
    let (left, right) = split_runs(&target.runs, o)?;
    if n == 1 {
        let add = text_len(&frag[0].runs);
        let mut runs = left;
        runs.extend(frag[0].runs.iter().cloned());
        runs.extend(right);
        let mut np = target;
        np.runs = runs;
        body[p] = BodyElement::Paragraph(np);
        return Ok((p, o + add));
    }
    let mut first = target.clone();
    let mut fr = left;
    fr.extend(frag[0].runs.iter().cloned());
    first.runs = fr;
    let last_len = text_len(&frag[n - 1].runs);
    let mut last = target;
    let mut lr: Vec<Run> = frag[n - 1].runs.iter().cloned().collect();
    lr.extend(right);
    last.runs = lr;
    last.page_break_before = false;
    last.bookmarks.clear();
    let mut inserts: Vec<BodyElement> = Vec::new();
    for fp in &frag[1..n - 1] {
        inserts.push(BodyElement::Paragraph(fp.clone()));
    }
    inserts.push(BodyElement::Paragraph(last));
    body[p] = BodyElement::Paragraph(first);
    for (k, e) in inserts.into_iter().enumerate() {
        body.insert(p + 1 + k, e);
    }
    Ok((p + n - 1, last_len))
}

/// 剪贴板是否有 Writer 文本内容。
#[no_mangle]
pub extern "C" fn roe_writer_clip_has() -> i32 {
    WCLIP.with(|c| if c.borrow().is_some() { 1 } else { 0 })
}

/// 复制 [(pa,oa),(pb,ob)) 区间到剪贴板（起止顺序不限）。
#[no_mangle]
pub extern "C" fn roe_writer_copy(handle: u64, pa: u32, oa: u32, pb: u32, ob: u32) -> i32 {
    let (pa, oa, pb, ob) = order(pa, oa, pb, ob);
    let result = handle::with_entry(handle, |entry| -> Result<Vec<Paragraph>, String> {
        let RoeDocument::Writer(doc) = &entry.doc else { return Err("非 Writer 文档".to_string()); };
        copy_in(&doc.body, pa, oa, pb, ob)
    });
    match result {
        Some(Ok(frag)) => {
            WCLIP.with(|c| *c.borrow_mut() = Some(frag));
            RoeErrorCode::Ok as i32
        }
        Some(Err(m)) => { set_last_error(format!("roe_writer_copy: {m}")); RoeErrorCode::ScriptError as i32 }
        None => { set_last_error("roe_writer_copy: 无效句柄"); RoeErrorCode::InvalidHandle as i32 }
    }
}

/// 删除 [(pa,oa),(pb,ob)) 区间（可跨段），带一次撤销快照。
#[no_mangle]
pub extern "C" fn roe_writer_delete_range(handle: u64, pa: u32, oa: u32, pb: u32, ob: u32) -> i32 {
    let (pa, oa, pb, ob) = order(pa, oa, pb, ob);
    if pa == pb && oa == ob {
        return RoeErrorCode::Ok as i32;
    }
    let result = handle::with_entry(handle, |entry| -> Result<(), String> {
        if !matches!(&entry.doc, RoeDocument::Writer(_)) { return Err("非 Writer 文档".to_string()); }
        let prev = entry.doc.clone();
        let RoeDocument::Writer(doc) = &mut entry.doc else { return Err("非 Writer 文档".to_string()); };
        delete_in(&mut doc.body, pa, oa, pb, ob)?;
        entry.push_undo_snapshot(prev, handle::EditCursorContext::at(pa, 0));
        Ok(())
    });
    let ok = matches!(&result, Some(Ok(())));
    if ok {
        handle::invalidate_writer_layout_cache(handle);
    }
    match result {
        Some(Ok(())) => RoeErrorCode::Ok as i32,
        Some(Err(m)) => { set_last_error(format!("roe_writer_delete_range: {m}")); RoeErrorCode::ScriptError as i32 }
        None => { set_last_error("roe_writer_delete_range: 无效句柄"); RoeErrorCode::InvalidHandle as i32 }
    }
}

/// 在 (para, offset) 处粘贴，返回新光标 JSON {"local_para_index":N,"char_offset":N}。
/// 失败返回 null。调用方用 roe_free_string 释放。
#[no_mangle]
pub extern "C" fn roe_writer_paste(handle: u64, para: u32, offset: u32) -> *mut c_char {
    let frag = WCLIP.with(|c| c.borrow().clone());
    let Some(frag) = frag else {
        set_last_error("roe_writer_paste: 剪贴板为空");
        return std::ptr::null_mut();
    };
    let (p, o) = (para as usize, offset as usize);
    let result = handle::with_entry(handle, |entry| -> Result<(usize, usize), String> {
        if !matches!(&entry.doc, RoeDocument::Writer(_)) { return Err("非 Writer 文档".to_string()); }
        let prev = entry.doc.clone();
        let RoeDocument::Writer(doc) = &mut entry.doc else { return Err("非 Writer 文档".to_string()); };
        let pos = paste_in(&mut doc.body, &frag, p, o)?;
        entry.push_undo_snapshot(prev, handle::EditCursorContext::at(p, 0));
        Ok(pos)
    });
    let ok = matches!(&result, Some(Ok(_)));
    if ok {
        handle::invalidate_writer_layout_cache(handle);
    }
    match result {
        Some(Ok((np, no))) => {
            let json = format!("{{\"local_para_index\":{},\"char_offset\":{}}}", np, no);
            match CString::new(json) {
                Ok(cs) => cs.into_raw(),
                Err(e) => { set_last_error(format!("roe_writer_paste: {e}")); std::ptr::null_mut() }
            }
        }
        Some(Err(m)) => { set_last_error(format!("roe_writer_paste: {m}")); std::ptr::null_mut() }
        None => { set_last_error("roe_writer_paste: 无效句柄"); std::ptr::null_mut() }
    }
}


// ───────── v249: 格式刷（取一个 run 的字符格式，套到区间内的文字）─────────
thread_local! {
    static WPAINT: RefCell<Option<Run>> = const { RefCell::new(None) };
}

fn run_at(runs: &[Run], offset: usize) -> Option<&Run> {
    let mut acc = 0usize;
    for r in runs {
        let len = r.text.len();
        if offset >= acc && offset < acc + len {
            return Some(r);
        }
        acc += len;
    }
    runs.last()
}

/// 取 (para, offset) 所在 run 的字符格式存入格式刷槽位。
#[no_mangle]
pub extern "C" fn roe_writer_painter_pick(handle: u64, para: u32, offset: u32) -> i32 {
    let result = handle::with_entry(handle, |entry| -> Result<Run, String> {
        let RoeDocument::Writer(doc) = &entry.doc else { return Err("非 Writer 文档".to_string()); };
        let p = para_ref(&doc.body, para as usize)?;
        let r = run_at(&p.runs, offset as usize).ok_or_else(|| "该段没有文字".to_string())?;
        let mut t = r.clone();
        t.text = String::new();
        t.hyperlink = None;
        t.footnote_ref = None;
        t.endnote_ref = None;
        t.comment_range_start.clear();
        t.comment_range_end.clear();
        t.comment_ref = None;
        t.formula = None;
        t.field = None;
        Ok(t)
    });
    match result {
        Some(Ok(t)) => {
            WPAINT.with(|c| *c.borrow_mut() = Some(t));
            RoeErrorCode::Ok as i32
        }
        Some(Err(m)) => { set_last_error(format!("roe_writer_painter_pick: {m}")); RoeErrorCode::ScriptError as i32 }
        None => { set_last_error("roe_writer_painter_pick: 无效句柄"); RoeErrorCode::InvalidHandle as i32 }
    }
}

/// 把格式刷槽位里的格式套到 para 段内 [start, end) 区间，带一次撤销快照。
#[no_mangle]
pub extern "C" fn roe_writer_painter_apply(handle: u64, para: u32, start: u32, end: u32) -> i32 {
    let tpl = WPAINT.with(|c| c.borrow().clone());
    let Some(tpl) = tpl else {
        set_last_error("roe_writer_painter_apply: 没有已取的格式");
        return RoeErrorCode::ScriptError as i32;
    };
    if start >= end {
        return RoeErrorCode::Ok as i32;
    }
    let p = para as usize;
    let (s, e) = (start as usize, end as usize);
    let result = handle::with_entry(handle, |entry| -> Result<(), String> {
        if !matches!(&entry.doc, RoeDocument::Writer(_)) { return Err("非 Writer 文档".to_string()); }
        let prev = entry.doc.clone();
        let RoeDocument::Writer(doc) = &mut entry.doc else { return Err("非 Writer 文档".to_string()); };
        let target = para_ref(&doc.body, p)?.clone();
        let (left, right) = split_runs(&target.runs, s)?;
        let (mut mid, tail) = split_runs(&right, e - s)?;
        for r in mid.iter_mut() {
            if r.formula.is_some() || r.field.is_some() { continue; }
            r.bold = tpl.bold.clone();
            r.italic = tpl.italic.clone();
            r.underline = tpl.underline.clone();
            r.underline_style = tpl.underline_style.clone();
            r.font_size = tpl.font_size.clone();
            r.font_family = tpl.font_family.clone();
            r.font_family_ascii = tpl.font_family_ascii.clone();
            r.color = tpl.color.clone();
            r.background = tpl.background.clone();
            r.strikethrough = tpl.strikethrough.clone();
            r.script = tpl.script.clone();
            r.vertical_position = tpl.vertical_position.clone();
            r.char_style = tpl.char_style.clone();
        }
        let mut runs = left;
        runs.extend(mid);
        runs.extend(tail);
        let mut np = target;
        np.runs = runs;
        doc.body[p] = BodyElement::Paragraph(np);
        entry.push_undo_snapshot(prev, handle::EditCursorContext::at(p, 0));
        Ok(())
    });
    let ok = matches!(&result, Some(Ok(())));
    if ok {
        handle::invalidate_writer_layout_cache(handle);
    }
    match result {
        Some(Ok(())) => RoeErrorCode::Ok as i32,
        Some(Err(m)) => { set_last_error(format!("roe_writer_painter_apply: {m}")); RoeErrorCode::ScriptError as i32 }
        None => { set_last_error("roe_writer_painter_apply: 无效句柄"); RoeErrorCode::InvalidHandle as i32 }
    }
}


// ───────── v266: 更改大小写 ─────────
fn convert_case(text: &str, mode: u32) -> String {
    match mode {
        1 => text.to_lowercase(),
        2 => text.to_uppercase(),
        3 => {
            let mut out = String::with_capacity(text.len());
            let mut start = true;
            for ch in text.chars() {
                if ch.is_alphanumeric() || ch == '\'' {
                    if start { out.extend(ch.to_uppercase()); } else { out.extend(ch.to_lowercase()); }
                    start = false;
                } else {
                    out.push(ch);
                    start = true;
                }
            }
            out
        }
        4 => text.chars().map(|c| {
            if c.is_lowercase() { c.to_uppercase().collect::<String>() }
            else if c.is_uppercase() { c.to_lowercase().collect::<String>() }
            else { c.to_string() }
        }).collect(),
        _ => {
            // 句首大写：run 开头和 . ! ? 。！？ 之后的第一个字母大写，其余小写
            let mut out = String::with_capacity(text.len());
            let mut cap = true;
            for ch in text.chars() {
                if ch.is_alphabetic() {
                    if cap { out.extend(ch.to_uppercase()); } else { out.extend(ch.to_lowercase()); }
                    cap = false;
                } else {
                    out.push(ch);
                    if matches!(ch, '.' | '!' | '?' | '。' | '！' | '？') { cap = true; }
                }
            }
            out
        }
    }
}

/// 更改 para 段内 [start, end) 的大小写。mode: 0句首 1小写 2大写 3每词首字母 4切换。带一次撤销快照。
#[no_mangle]
pub extern "C" fn roe_writer_change_case(handle: u64, para: u32, start: u32, end: u32, mode: u32) -> i32 {
    if mode > 4 {
        set_last_error("roe_writer_change_case: mode 非法（合法值 0-4）");
        return RoeErrorCode::ScriptError as i32;
    }
    if start >= end { return RoeErrorCode::Ok as i32; }
    let p = para as usize;
    let (s, e) = (start as usize, end as usize);
    let result = handle::with_entry(handle, |entry| -> Result<(), String> {
        if !matches!(&entry.doc, RoeDocument::Writer(_)) { return Err("非 Writer 文档".to_string()); }
        let prev = entry.doc.clone();
        let RoeDocument::Writer(doc) = &mut entry.doc else { return Err("非 Writer 文档".to_string()); };
        let target = para_ref(&doc.body, p)?.clone();
        let (left, right) = split_runs(&target.runs, s)?;
        let (mut mid, tail) = split_runs(&right, e - s)?;
        for r in mid.iter_mut() {
            if r.formula.is_some() || r.field.is_some() { continue; }
            r.text = convert_case(&r.text, mode);
        }
        let mut runs = left;
        runs.extend(mid);
        runs.extend(tail);
        let mut np = target;
        np.runs = runs;
        doc.body[p] = BodyElement::Paragraph(np);
        entry.push_undo_snapshot(prev, handle::EditCursorContext::at(p, 0));
        Ok(())
    });
    let ok = matches!(&result, Some(Ok(())));
    if ok { handle::invalidate_writer_layout_cache(handle); }
    match result {
        Some(Ok(())) => RoeErrorCode::Ok as i32,
        Some(Err(m)) => { set_last_error(format!("roe_writer_change_case: {m}")); RoeErrorCode::ScriptError as i32 }
        None => { set_last_error("roe_writer_change_case: 无效句柄"); RoeErrorCode::InvalidHandle as i32 }
    }
}


// ───────── v267: 插入分页符 ─────────
/// 在 para 段内 offset 处插入分页符：offset=0 只给本段置 page_break_before；
/// 否则拆段，后半段置标志。返回 0=未拆段 1=已拆段 -1=失败。带一次撤销快照。
#[no_mangle]
pub extern "C" fn roe_writer_insert_page_break(handle: u64, para: u32, offset: u32) -> i32 {
    let p = para as usize;
    let o = offset as usize;
    let result = handle::with_entry(handle, |entry| -> Result<i32, String> {
        if !matches!(&entry.doc, RoeDocument::Writer(_)) { return Err("非 Writer 文档".to_string()); }
        let prev = entry.doc.clone();
        let RoeDocument::Writer(doc) = &mut entry.doc else { return Err("非 Writer 文档".to_string()); };
        let target = para_ref(&doc.body, p)?.clone();
        let total = text_len(&target.runs);
        if o > total { return Err(format!("offset={o} 超出段落长度 {total}")); }
        let code;
        if o == 0 {
            let mut np = target;
            np.page_break_before = true;
            doc.body[p] = BodyElement::Paragraph(np);
            code = 0;
        } else {
            let (left, right) = split_runs(&target.runs, o)?;
            let mut first = target.clone();
            first.runs = left;
            let mut second = target;
            second.runs = right;
            second.bookmarks.clear();
            second.page_break_before = true;
            doc.body[p] = BodyElement::Paragraph(first);
            doc.body.insert(p + 1, BodyElement::Paragraph(second));
            code = 1;
        }
        entry.push_undo_snapshot(prev, handle::EditCursorContext::at(p, o));
        Ok(code)
    });
    match result {
        Some(Ok(c)) => { handle::invalidate_writer_layout_cache(handle); c }
        Some(Err(m)) => { set_last_error(format!("roe_writer_insert_page_break: {m}")); -1 }
        None => { set_last_error("roe_writer_insert_page_break: 无效句柄"); -1 }
    }
}
