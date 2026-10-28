//! v315：表格单元格内的文本编辑（区间替换、退格/Delete）。
//! 位置编码同 table_cell_cursor.rs：cid 高 16 位=表格在 body 中的序号，
//! 低 16 位=行*64+列；para=单元格 content 下标；off=段落内字节偏移。
//! 做法：先在段落副本上试做，成功后再存快照、写回、压入撤销栈。

use std::ffi::CString;
use std::os::raw::c_char;

use roe_model::common::CellContent;
use roe_model::writer::{BodyElement, Paragraph, Run, WriterDocument};
use roe_parser::RoeDocument;

use crate::error::set_last_error;
use crate::handle;

fn split_cid(cid: u32) -> (usize, usize, usize) {
    let t = (cid >> 16) as usize;
    let rc = (cid & 0xFFFF) as usize;
    (t, rc / 64, rc % 64)
}

fn cell_para<'a>(doc: &'a WriterDocument, cid: u32, p: usize) -> Result<&'a Paragraph, String> {
    let (t, r, c) = split_cid(cid);
    let Some(BodyElement::Table(tbl)) = doc.body.get(t) else {
        return Err(format!("body[{t}] 不是表格"));
    };
    let cell = tbl.rows.get(r).and_then(|row| row.cells.get(c))
        .ok_or_else(|| format!("单元格 r={r} c={c} 不存在"))?;
    match cell.content.get(p) {
        Some(CellContent::Paragraph(para)) => Ok(para),
        _ => Err(format!("单元格内容 {p} 不是段落")),
    }
}

fn cell_para_mut<'a>(doc: &'a mut WriterDocument, cid: u32, p: usize) -> Result<&'a mut Paragraph, String> {
    let (t, r, c) = split_cid(cid);
    let Some(BodyElement::Table(tbl)) = doc.body.get_mut(t) else {
        return Err(format!("body[{t}] 不是表格"));
    };
    let cell = tbl.rows.get_mut(r).and_then(|row| row.cells.get_mut(c))
        .ok_or_else(|| format!("单元格 r={r} c={c} 不存在"))?;
    match cell.content.get_mut(p) {
        Some(CellContent::Paragraph(para)) => Ok(para),
        _ => Err(format!("单元格内容 {p} 不是段落")),
    }
}

/// 在段落上做区间替换，返回新光标偏移。
fn replace_in_para(para: &mut Paragraph, start: usize, end: usize, text: &str) -> Result<usize, String> {
    let para_len: usize = para.runs.iter().map(|r| r.text.len()).sum();
    if start > end || end > para_len {
        return Err(format!("区间 [{start},{end}) 超出段落长度 {para_len}"));
    }
    if para.runs.is_empty() {
        if !text.is_empty() {
            para.runs.push(Run { text: text.to_string(), ..Default::default() });
        }
        return Ok(text.len());
    }
    let mut consumed = 0usize;
    let mut found: Option<(usize, usize, usize)> = None;
    for (i, run) in para.runs.iter().enumerate() {
        let rs = consumed;
        let re = consumed + run.text.len();
        if start >= rs && start <= re && end >= rs && end <= re {
            found = Some((i, start - rs, end - rs));
            break;
        }
        consumed = re;
    }
    let Some((i, ls, le)) = found else {
        return Err("区间未落在同一个 run 内".to_string());
    };
    if !para.runs[i].text.is_char_boundary(ls) || !para.runs[i].text.is_char_boundary(le) {
        return Err("区间不在字符边界上".to_string());
    }
    para.runs[i].text.replace_range(ls..le, text);
    if para.runs[i].text.is_empty() {
        para.runs.remove(i);
    }
    Ok(start + text.len())
}

fn commit(
    entry: &mut handle::DocumentEntry,
    cid: u32,
    p: usize,
    newp: Paragraph,
    t: usize,
    ctx_off: usize,
    new_off: usize,
) -> Result<String, String> {
    entry.snapshot = Some(entry.doc.clone());
    let RoeDocument::Writer(doc) = &mut entry.doc else {
        return Err("非 Writer 文档".to_string());
    };
    *cell_para_mut(doc, cid, p)? = newp;
    if let Some(prev) = entry.snapshot.take() {
        let ctx = handle::EditCursorContext::at(t, ctx_off);
        entry.push_undo_snapshot(prev, ctx);
    }
    Ok(format!("{{\"ct\":1,\"cid\":{},\"para\":{},\"off\":{}}}", cid, p, new_off))
}

fn replace_in_entry(
    entry: &mut handle::DocumentEntry, cid: u32, p: usize, start: usize, end: usize, text: &str,
) -> Result<String, String> {
    let RoeDocument::Writer(doc) = &entry.doc else {
        return Err("非 Writer 文档".to_string());
    };
    let mut newp = cell_para(doc, cid, p)?.clone();
    let new_off = replace_in_para(&mut newp, start, end, text)?;
    commit(entry, cid, p, newp, split_cid(cid).0, start, new_off)
}

fn delete_in_entry(
    entry: &mut handle::DocumentEntry, cid: u32, p: usize, off: usize, dir: u32,
) -> Result<String, String> {
    let RoeDocument::Writer(doc) = &entry.doc else {
        return Err("非 Writer 文档".to_string());
    };
    let mut newp = cell_para(doc, cid, p)?.clone();
    let full: String = newp.runs.iter().map(|r| r.text.as_str()).collect();
    if off > full.len() || !full.is_char_boundary(off) {
        return Err(format!("off={off} 非法"));
    }
    let (s, e) = if dir == 0 {
        if off == 0 {
            return Err("单元格段落开头，退格无字符可删".to_string());
        }
        let (idx, _) = full[..off].char_indices().next_back().ok_or("无字符")?;
        (idx, off)
    } else {
        if off >= full.len() {
            return Err("单元格段落末尾，Delete 无字符可删".to_string());
        }
        let ch = full[off..].chars().next().ok_or("无字符")?;
        (off, off + ch.len_utf8())
    };
    replace_in_para(&mut newp, s, e, "")?;
    commit(entry, cid, p, newp, split_cid(cid).0, off, s)
}

fn finish(handle_val: u64, result: Option<Result<String, String>>, name: &str) -> *mut c_char {
    let ok = matches!(&result, Some(Ok(_)));
    if ok {
        handle::invalidate_writer_layout_cache(handle_val);
    }
    match result {
        None => { set_last_error(format!("{name}: 无效句柄")); std::ptr::null_mut() }
        Some(Err(m)) => { set_last_error(format!("{name}: {m}")); std::ptr::null_mut() }
        Some(Ok(json)) => match CString::new(json) {
            Ok(cs) => cs.into_raw(),
            Err(e) => { set_last_error(format!("{name}: {e}")); std::ptr::null_mut() }
        },
    }
}

/// 单元格内区间替换。返回 {"ct":1,"cid":..,"para":..,"off":新光标偏移}。
#[no_mangle]
pub extern "C" fn roe_cell_replace_range(
    handle_val: u64, cid: u32, para: u32, start: u32, end: u32,
    text_ptr: *const u8, text_len: u32,
) -> *mut c_char {
    let text = if text_len == 0 {
        String::new()
    } else if text_ptr.is_null() {
        set_last_error("roe_cell_replace_range: 空指针");
        return std::ptr::null_mut();
    } else {
        let b = unsafe { std::slice::from_raw_parts(text_ptr, text_len as usize) };
        match std::str::from_utf8(b) {
            Ok(s) => s.to_string(),
            Err(e) => { set_last_error(format!("roe_cell_replace_range: {e}")); return std::ptr::null_mut(); }
        }
    };
    let r = handle::with_entry(handle_val, |entry| {
        replace_in_entry(entry, cid, para as usize, start as usize, end as usize, &text)
    });
    finish(handle_val, r, "roe_cell_replace_range")
}

/// 单元格内退格(direction=0)/Delete(direction=1)。
#[no_mangle]
pub extern "C" fn roe_cell_delete_char(
    handle_val: u64, cid: u32, para: u32, off: u32, direction: u32,
) -> *mut c_char {
    let r = handle::with_entry(handle_val, |entry| {
        delete_in_entry(entry, cid, para as usize, off as usize, direction)
    });
    finish(handle_val, r, "roe_cell_delete_char")
}

// ───────── v316a：回车拆段、Tab 跳格/加行 ─────────

fn split_para(para: &mut Paragraph, off: usize) -> Result<Paragraph, String> {
    let full: String = para.runs.iter().map(|r| r.text.as_str()).collect();
    if off > full.len() || !full.is_char_boundary(off) {
        return Err(format!("off={off} 非法"));
    }
    let mut consumed = 0usize;
    let mut front: Vec<Run> = Vec::new();
    let mut back: Vec<Run> = Vec::new();
    for run in para.runs.drain(..) {
        let rs = consumed;
        let re = consumed + run.text.len();
        if re <= off {
            front.push(run);
        } else if rs >= off {
            back.push(run);
        } else {
            let k = off - rs;
            let mut f = run.clone();
            let mut b = run;
            f.text.truncate(k);
            b.text = b.text.split_off(k);
            front.push(f);
            back.push(b);
        }
        consumed = re;
    }
    let mut newp = para.clone();
    para.runs = front;
    newp.runs = back;
    Ok(newp)
}

fn split_in_entry(entry: &mut handle::DocumentEntry, cid: u32, p: usize, off: usize) -> Result<String, String> {
    let RoeDocument::Writer(doc) = &entry.doc else {
        return Err("非 Writer 文档".to_string());
    };
    let mut front = cell_para(doc, cid, p)?.clone();
    let back = split_para(&mut front, off)?;
    let (t, r, c) = split_cid(cid);
    entry.snapshot = Some(entry.doc.clone());
    let RoeDocument::Writer(doc) = &mut entry.doc else {
        return Err("非 Writer 文档".to_string());
    };
    let Some(BodyElement::Table(tbl)) = doc.body.get_mut(t) else {
        return Err(format!("body[{t}] 不是表格"));
    };
    let cell = tbl.rows.get_mut(r).and_then(|row| row.cells.get_mut(c))
        .ok_or_else(|| "单元格不存在".to_string())?;
    cell.content[p] = CellContent::Paragraph(front);
    cell.content.insert(p + 1, CellContent::Paragraph(back));
    if let Some(prev) = entry.snapshot.take() {
        let ctx = handle::EditCursorContext::at(t, off);
        entry.push_undo_snapshot(prev, ctx);
    }
    Ok(format!("{{\"ct\":1,\"cid\":{},\"para\":{},\"off\":0}}", cid, p + 1))
}

/// 单元格内回车拆段。返回 {"ct":1,"cid":..,"para":新段落content下标,"off":0}。
#[no_mangle]
pub extern "C" fn roe_cell_split_paragraph(handle_val: u64, cid: u32, para: u32, off: u32) -> *mut c_char {
    let r = handle::with_entry(handle_val, |entry| split_in_entry(entry, cid, para as usize, off as usize));
    finish(handle_val, r, "roe_cell_split_paragraph")
}

fn insert_row_in_doc(doc: &mut WriterDocument, t: usize, after: usize) -> Result<(), String> {
    let Some(BodyElement::Table(tbl)) = doc.body.get_mut(t) else {
        return Err(format!("body[{t}] 不是表格"));
    };
    let mut row = tbl.rows.get(after).ok_or_else(|| "行不存在".to_string())?.clone();
    for cell in row.cells.iter_mut() {
        let mut np = cell.content.iter().find_map(|ce| {
            if let CellContent::Paragraph(p) = ce { Some(p.clone()) } else { None }
        }).unwrap_or_default();
        np.runs.clear();
        cell.content = vec![CellContent::Paragraph(np)];
    }
    tbl.rows.insert(after + 1, row);
    Ok(())
}

fn tab_in_entry(entry: &mut handle::DocumentEntry, cid: u32, back: bool) -> Result<String, String> {
    let (t, r, c) = split_cid(cid);
    let RoeDocument::Writer(doc) = &entry.doc else {
        return Err("非 Writer 文档".to_string());
    };
    let Some(BodyElement::Table(tbl)) = doc.body.get(t) else {
        return Err(format!("body[{t}] 不是表格"));
    };
    let nrows = tbl.rows.len();
    let row_len = tbl.rows.get(r).map(|x| x.cells.len()).ok_or_else(|| "行不存在".to_string())?;
    let (nr, nc, need_add) = if !back {
        if c + 1 < row_len { (r, c + 1, false) }
        else if r + 1 < nrows { (r + 1, 0, false) }
        else { (r + 1, 0, true) }
    } else if c > 0 {
        (r, c - 1, false)
    } else if r > 0 {
        (r - 1, tbl.rows[r - 1].cells.len().saturating_sub(1), false)
    } else {
        (r, c, false)
    };
    if nc >= 64 {
        return Err("列数超过 63".to_string());
    }
    if need_add {
        entry.snapshot = Some(entry.doc.clone());
        let RoeDocument::Writer(doc) = &mut entry.doc else {
            return Err("非 Writer 文档".to_string());
        };
        insert_row_in_doc(doc, t, r)?;
        if let Some(prev) = entry.snapshot.take() {
            let ctx = handle::EditCursorContext::at(t, 0);
            entry.push_undo_snapshot(prev, ctx);
        }
    }
    let RoeDocument::Writer(doc) = &entry.doc else {
        return Err("非 Writer 文档".to_string());
    };
    let Some(BodyElement::Table(tbl)) = doc.body.get(t) else {
        return Err("表格丢失".to_string());
    };
    let cell = tbl.rows.get(nr).and_then(|row| row.cells.get(nc))
        .ok_or_else(|| "目标单元格不存在".to_string())?;
    let p = cell.content.iter().position(|ce| matches!(ce, CellContent::Paragraph(_))).unwrap_or(0);
    let off = match cell.content.get(p) {
        Some(CellContent::Paragraph(pp)) => pp.runs.iter().map(|r| r.text.len()).sum::<usize>(),
        _ => 0,
    };
    let ncid = ((t as u32) << 16) | ((nr * 64 + nc) as u32);
    Ok(format!("{{\"ct\":1,\"cid\":{},\"para\":{},\"off\":{}}}", ncid, p, off))
}

/// Tab(back=0)/Shift+Tab(back=1) 跳格；表尾 Tab 自动加一行。
/// 返回 {"ct":1,"cid":新cid,"para":..,"off":..}。
#[no_mangle]
pub extern "C" fn roe_cell_tab(handle_val: u64, cid: u32, back: u32) -> *mut c_char {
    let r = handle::with_entry(handle_val, |entry| tab_in_entry(entry, cid, back != 0));
    finish(handle_val, r, "roe_cell_tab")
}

// ───────── v316b：单元格内段落合并（段首退格 / 段尾 Delete）─────────

fn merge_in_entry(entry: &mut handle::DocumentEntry, cid: u32, p: usize, dir: u32) -> Result<String, String> {
    let (t, r, c) = split_cid(cid);
    let RoeDocument::Writer(doc) = &entry.doc else {
        return Err("非 Writer 文档".to_string());
    };
    let Some(BodyElement::Table(tbl)) = doc.body.get(t) else {
        return Err(format!("body[{t}] 不是表格"));
    };
    let cell = tbl.rows.get(r).and_then(|row| row.cells.get(c))
        .ok_or_else(|| "单元格不存在".to_string())?;
    // 把 hi 并入 lo
    let (lo, hi) = if dir == 0 {
        if p == 0 {
            return Err("已是单元格第一段，无法向前合并".to_string());
        }
        (p - 1, p)
    } else {
        (p, p + 1)
    };
    let (Some(CellContent::Paragraph(a)), Some(CellContent::Paragraph(b))) =
        (cell.content.get(lo), cell.content.get(hi)) else {
        return Err("相邻内容不是段落，不能合并".to_string());
    };
    let join_off: usize = a.runs.iter().map(|x| x.text.len()).sum();
    let mut merged = a.clone();
    merged.runs.extend(b.runs.iter().cloned());

    entry.snapshot = Some(entry.doc.clone());
    let RoeDocument::Writer(doc) = &mut entry.doc else {
        return Err("非 Writer 文档".to_string());
    };
    let Some(BodyElement::Table(tbl)) = doc.body.get_mut(t) else {
        return Err(format!("body[{t}] 不是表格"));
    };
    let cell = tbl.rows.get_mut(r).and_then(|row| row.cells.get_mut(c))
        .ok_or_else(|| "单元格不存在".to_string())?;
    cell.content[lo] = CellContent::Paragraph(merged);
    cell.content.remove(hi);
    if let Some(prev) = entry.snapshot.take() {
        let ctx = handle::EditCursorContext::at(t, join_off);
        entry.push_undo_snapshot(prev, ctx);
    }
    Ok(format!("{{\"ct\":1,\"cid\":{},\"para\":{},\"off\":{}}}", cid, lo, join_off))
}

/// 单元格内段落合并。dir=0：段首退格，当前段并入上一段；dir=1：段尾 Delete，下一段并入当前段。
/// 返回 {"ct":1,"cid":..,"para":合并后段落下标,"off":合并点偏移}。
#[no_mangle]
pub extern "C" fn roe_cell_merge_paragraph(handle_val: u64, cid: u32, para: u32, dir: u32) -> *mut c_char {
    let r = handle::with_entry(handle_val, |entry| merge_in_entry(entry, cid, para as usize, dir));
    finish(handle_val, r, "roe_cell_merge_paragraph")
}

// ───────── v316b：单元格内光标移动（左/右）与位置校正 ─────────

fn plen(content: &[CellContent], i: usize) -> Option<usize> {
    match content.get(i) {
        Some(CellContent::Paragraph(pp)) => Some(pp.runs.iter().map(|r| r.text.len()).sum()),
        _ => None,
    }
}

fn ptext(content: &[CellContent], i: usize) -> Option<String> {
    match content.get(i) {
        Some(CellContent::Paragraph(pp)) => Some(pp.runs.iter().map(|r| r.text.as_str()).collect()),
        _ => None,
    }
}

/// cmd: 0=左 1=右 2=仅校正（para/off 越界时夹到合法位置）
fn nav_in_entry(entry: &handle::DocumentEntry, cid: u32, p: usize, off: usize, cmd: u32) -> Result<String, String> {
    let (t, r, c) = split_cid(cid);
    let RoeDocument::Writer(doc) = &entry.doc else {
        return Err("非 Writer 文档".to_string());
    };
    let Some(BodyElement::Table(tbl)) = doc.body.get(t) else {
        return Err(format!("body[{t}] 不是表格"));
    };
    let cell = tbl.rows.get(r).and_then(|row| row.cells.get(c))
        .ok_or_else(|| "单元格不存在".to_string())?;
    let content = &cell.content;
    let is_para = |x: &CellContent| matches!(x, CellContent::Paragraph(_));
    let mk = |tr: usize, tc: usize| ((t as u32) << 16) | ((tr * 64 + tc) as u32);
    let last_p = content.iter().rposition(|x| is_para(x));
    let cur_p = if plen(content, p).is_some() { p } else { last_p.ok_or_else(|| "单元格没有段落".to_string())? };
    let text = ptext(content, cur_p).unwrap_or_default();
    let mut o = off.min(text.len());
    while o > 0 && text.is_char_boundary(o) == false { o -= 1; }
    let mut np = cur_p;
    let mut no = o;
    let mut ncid = cid;

    if cmd == 0 {
        if o > 0 {
            no = text[..o].char_indices().next_back().map(|(i, _)| i).unwrap_or(0);
        } else if let Some(q) = content[..cur_p].iter().rposition(|x| is_para(x)) {
            np = q;
            no = plen(content, q).unwrap_or(0);
        } else {
            let prev = if c > 0 { Some((r, c - 1)) }
                else if r > 0 { Some((r - 1, tbl.rows[r - 1].cells.len().saturating_sub(1))) }
                else { None };
            if let Some((tr, tc)) = prev {
                if let Some(tcell) = tbl.rows.get(tr).and_then(|row| row.cells.get(tc)) {
                    if let Some(lp) = tcell.content.iter().rposition(|x| is_para(x)) {
                        ncid = mk(tr, tc);
                        np = lp;
                        no = plen(&tcell.content, lp).unwrap_or(0);
                    }
                }
            }
        }
    } else if cmd == 1 {
        if o < text.len() {
            let n = text[o..].chars().next().map(|ch| ch.len_utf8()).unwrap_or(1);
            no = o + n;
        } else if let Some(q) = content.iter().skip(cur_p + 1).position(|x| is_para(x)).map(|i| i + cur_p + 1) {
            np = q;
            no = 0;
        } else {
            let next = if c + 1 < tbl.rows[r].cells.len() { Some((r, c + 1)) }
                else if r + 1 < tbl.rows.len() { Some((r + 1, 0)) }
                else { None };
            if let Some((tr, tc)) = next {
                if let Some(tcell) = tbl.rows.get(tr).and_then(|row| row.cells.get(tc)) {
                    if let Some(fp) = tcell.content.iter().position(|x| is_para(x)) {
                        ncid = mk(tr, tc);
                        np = fp;
                        no = 0;
                    }
                }
            }
        }
    }
    Ok(format!("{{\"ct\":1,\"cid\":{},\"para\":{},\"off\":{}}}", ncid, np, no))
}

/// 单元格内光标左(0)/右(1)移动，cmd=2 仅校正位置。只读，不入撤销栈。
#[no_mangle]
pub extern "C" fn roe_cell_nav(handle_val: u64, cid: u32, para: u32, off: u32, cmd: u32) -> *mut c_char {
    let r = handle::with_entry(handle_val, |entry| nav_in_entry(entry, cid, para as usize, off as usize, cmd));
    match r {
        None => { set_last_error("roe_cell_nav: 无效句柄"); std::ptr::null_mut() }
        Some(Err(m)) => { set_last_error(format!("roe_cell_nav: {m}")); std::ptr::null_mut() }
        Some(Ok(json)) => match CString::new(json) {
            Ok(cs) => cs.into_raw(),
            Err(_) => std::ptr::null_mut(),
        },
    }
}

// ───────── v317：单元格内字符格式 / 段落格式 ─────────

fn fmt_color(rgb: u32) -> roe_model::common::Color {
    roe_model::common::Color { r: ((rgb >> 16) & 0xff) as u8, g: ((rgb >> 8) & 0xff) as u8, b: (rgb & 0xff) as u8, a: 255 }
}

fn fmt_char_in_entry(
    entry: &mut handle::DocumentEntry, cid: u32, p: usize, start: usize, end: usize, kind: u32, a: u32, b: u32,
) -> Result<String, String> {
    use roe_model::writer::ScriptPosition as SP;
    use roe_writer::char_format::{set_char_format_in_range, CharFormatPatch};
    let tri = |v: u32| -> Result<Option<bool>, String> {
        match v { 0 => Ok(Some(false)), 1 => Ok(Some(true)), 2 => Ok(None), _ => Err(format!("参数非法 {v}")) }
    };
    let mut patch = CharFormatPatch::default();
    match kind {
        0 => patch.bold = Some(tri(a)?),
        1 => patch.italic = Some(tri(a)?),
        2 => patch.underline = Some(tri(a)?),
        3 => patch.strikethrough = Some(tri(a)?),
        4 => patch.script = Some(match a {
            0 => Some(SP::Normal), 1 => Some(SP::Superscript), 2 => Some(SP::Subscript), 3 => None,
            _ => return Err(format!("上下标参数非法 {a}")),
        }),
        5 => patch.font_size = Some(if a == 0 { None } else { Some(a as f32 / 10.0) }),
        6 => patch.color = Some(if b != 0 { None } else { Some(fmt_color(a)) }),
        7 => patch.background = Some(if b != 0 { None } else { Some(fmt_color(a)) }),
        _ => return Err(format!("未知字符格式 kind={kind}")),
    }
    let RoeDocument::Writer(doc) = &entry.doc else {
        return Err("非 Writer 文档".to_string());
    };
    let mut newp = cell_para(doc, cid, p)?.clone();
    let plen: usize = newp.runs.iter().map(|r| r.text.len()).sum();
    let end = end.min(plen);
    if start >= end {
        return Ok(format!("{{\"ct\":1,\"cid\":{},\"para\":{}}}", cid, p));
    }
    set_char_format_in_range(&mut newp, start, end, &patch)?;
    commit(entry, cid, p, newp, split_cid(cid).0, start, end)?;
    Ok(format!("{{\"ct\":1,\"cid\":{},\"para\":{}}}", cid, p))
}

fn fmt_para_in_entry(
    entry: &mut handle::DocumentEntry, cid: u32, p: usize, kind: u32, value: f32,
) -> Result<String, String> {
    use roe_model::style::Alignment as AL;
    let RoeDocument::Writer(doc) = &entry.doc else {
        return Err("非 Writer 文档".to_string());
    };
    let mut newp = cell_para(doc, cid, p)?.clone();
    let opt = if value < 0.0 { None } else { Some(value) };
    match kind {
        0 => newp.alignment = match value as i32 {
            0 => Some(AL::Left), 1 => Some(AL::Center), 2 => Some(AL::Right), 3 => Some(AL::Justify), 4 => None,
            _ => return Err("对齐参数非法".to_string()),
        },
        1 => newp.space_before = opt,
        2 => newp.space_after = opt,
        3 => newp.line_height = opt,
        4 => newp.shading = if value < 0.0 { None } else {
            let c = value as u32;
            Some([((c >> 16) & 0xff) as u8, ((c >> 8) & 0xff) as u8, (c & 0xff) as u8, 255])
        },
        _ => return Err(format!("未知段落格式 kind={kind}")),
    }
    commit(entry, cid, p, newp, split_cid(cid).0, 0, 0)?;
    Ok(format!("{{\"ct\":1,\"cid\":{},\"para\":{}}}", cid, p))
}

/// 单元格内字符格式。区间 [start,end)，kind/a/b 见 v243 文档。成功返回 {"ct":1,"cid":..,"para":..}。
#[no_mangle]
pub extern "C" fn roe_cell_fmt_char(handle_val: u64, cid: u32, para: u32, start: u32, end: u32, kind: u32, a: u32, b: u32) -> *mut c_char {
    let r = handle::with_entry(handle_val, |entry| {
        fmt_char_in_entry(entry, cid, para as usize, start as usize, end as usize, kind, a, b)
    });
    finish(handle_val, r, "roe_cell_fmt_char")
}

/// 单元格内段落格式（整段）。kind/value 见 v243 文档。
#[no_mangle]
pub extern "C" fn roe_cell_fmt_para(handle_val: u64, cid: u32, para: u32, kind: u32, value: f32) -> *mut c_char {
    let r = handle::with_entry(handle_val, |entry| fmt_para_in_entry(entry, cid, para as usize, kind, value));
    finish(handle_val, r, "roe_cell_fmt_para")
}

// ───────── v317：单元格整段字符格式查询 ─────────

fn fmt_get_in_entry(entry: &handle::DocumentEntry, cid: u32, p: usize, kind: u32) -> Result<i32, String> {
    use roe_model::writer::ScriptPosition as SP;
    use roe_writer::char_format::{get_bool_flag_in_range, BoolFlagResult};
    let RoeDocument::Writer(doc) = &entry.doc else {
        return Err("非 Writer 文档".to_string());
    };
    let para = cell_para(doc, cid, p)?;
    if kind == 6 {
        let mut it = para.runs.iter().filter(|r| !r.text.is_empty()).map(|r| r.font_size);
        let first = match it.next() { Some(v) => v, None => return Ok(-1) };
        if it.all(|v| v == first) {
            return Ok(first.map(|s| (s * 10.0).round() as i32).unwrap_or(-1));
        }
        return Ok(-1);
    }
    let plen: usize = para.runs.iter().map(|r| r.text.len()).sum();
    if plen == 0 {
        return Ok(0);
    }
    let r = match kind {
        0 => get_bool_flag_in_range(para, 0, plen, |r| r.bold),
        1 => get_bool_flag_in_range(para, 0, plen, |r| r.italic),
        2 => get_bool_flag_in_range(para, 0, plen, |r| r.underline),
        3 => get_bool_flag_in_range(para, 0, plen, |r| r.strikethrough),
        4 => get_bool_flag_in_range(para, 0, plen, |r| Some(r.script == Some(SP::Superscript))),
        5 => get_bool_flag_in_range(para, 0, plen, |r| Some(r.script == Some(SP::Subscript))),
        _ => return Err(format!("未知查询 kind={kind}")),
    }?;
    Ok(match r {
        BoolFlagResult::AllTrue => 1,
        BoolFlagResult::AllFalse => 0,
        BoolFlagResult::Mixed => 2,
    })
}

/// 单元格整段字符格式查询。kind: 0加粗 1斜体 2下划线 3删除线 4上标 5下标。返回 0全否 1全是 2混合 -1出错。
#[no_mangle]
pub extern "C" fn roe_cell_fmt_get(handle_val: u64, cid: u32, para: u32, kind: u32) -> i32 {
    match handle::with_entry(handle_val, |entry| fmt_get_in_entry(&*entry, cid, para as usize, kind)) {
        Some(Ok(v)) => v,
        Some(Err(m)) => { set_last_error(format!("roe_cell_fmt_get: {m}")); -1 }
        None => { set_last_error("roe_cell_fmt_get: 无效句柄"); -1 }
    }
}


/// v317f：设置单元格段落字体名（name 为空指针或空串 = 清除）。区间同 fmt_char。
pub extern "C" fn roe_cell_fmt_font(handle_val: u64, cid: u32, para: u32, start: u32, end: u32, name: *const c_char) -> *mut c_char {
    let nm: Option<String> = if name.is_null() { None } else {
        match unsafe { std::ffi::CStr::from_ptr(name) }.to_str() {
            Ok(s) if !s.is_empty() => Some(s.to_string()),
            Ok(_) => None,
            Err(_) => { set_last_error("roe_cell_fmt_font: 字体名不是有效 UTF-8"); return std::ptr::null_mut(); }
        }
    };
    let r = handle::with_entry(handle_val, |entry| {
        fmt_font_in_entry(entry, cid, para as usize, start as usize, end as usize, nm.clone())
    });
    finish(handle_val, r, "roe_cell_fmt_font")
}

fn fmt_font_in_entry(
    entry: &mut handle::DocumentEntry, cid: u32, p: usize, start: usize, end: usize, name: Option<String>,
) -> Result<String, String> {
    use roe_writer::char_format::{set_char_format_in_range, CharFormatPatch};
    let mut patch = CharFormatPatch::default();
    patch.font_family = Some(name);
    let RoeDocument::Writer(doc) = &entry.doc else {
        return Err("非 Writer 文档".to_string());
    };
    let mut newp = cell_para(doc, cid, p)?.clone();
    let plen: usize = newp.runs.iter().map(|r| r.text.len()).sum();
    let end = end.min(plen);
    if start >= end {
        return Ok(format!("{{\"ct\":1,\"cid\":{},\"para\":{}}}", cid, p));
    }
    set_char_format_in_range(&mut newp, start, end, &patch)?;
    commit(entry, cid, p, newp, split_cid(cid).0, start, end)?;
    Ok(format!("{{\"ct\":1,\"cid\":{},\"para\":{}}}", cid, p))
}
