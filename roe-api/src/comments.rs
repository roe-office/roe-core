//! 批注（comment）查询 C-ABI 接口，Slint 批注气泡面板第一阶段
//! （ROE_Progress_v157.md 第三节方向讨论的落地，见 v158 会话记录）。
//!
//! 两个接口配对使用，职责拆分：
//! - roe_get_comment_threads：批注文本内容，一次性返回全文档所有讨论串
//!   （root + replies），不含坐标。调用时机：文档打开后、批注数据变化后。
//!   本次只做纯文本（ROE_Progress_v157.md 第三节"忠实转发"原则的第一步
//!   落地——BodyElement 富文本样式本次不传，下次设计中间表示时再扩展）。
//! - roe_take_comment_hits：批注锚点坐标，只覆盖"最近一次 roe_render_
//!   writer_viewport 调用时，落在可见viewport范围内"的批注命中，不是
//!   全文档坐标（全文档扫描是独立的、本次未实现的后续功能）。调用时机：
//!   每次 roe_render_writer_viewport 渲染完成后立即调用。
//!
//! 两者用 comment_id 关联：调用方（Slint/doc-core）用
//! roe_get_comment_threads 拿到的文本内容建一份 id -> 文本 的映射，
//! 每次渲染后用 roe_take_comment_hits 拿到"这次可见的批注坐标列表"，
//! 按 comment_id 查文本、按坐标画气泡；不在坐标列表里的批注暂不显示
//! （还没滚动到），气泡跟随滚动的效果由调用方每次重渲染后重新调用
//! roe_take_comment_hits 实现，roe 侧不缓存"曾经见过但当前不可见"的坐标。

use std::ffi::CString;
use std::os::raw::c_char;

use roe_model::writer::comment_plain_text;
use roe_parser::RoeDocument;

use crate::error::set_last_error;
use crate::handle;

/// 把 Comment.date 原始字符串（docx w:date，标准格式
/// "YYYY-MM-DDTHH:MM:SSZ"）格式化成 "YYYY-MM-DD HH:MM" 短格式显示；
/// 不符合标准 ISO8601 格式的原样返回。判断标准与
/// roe-model WriterDocument::comment_threads 内部 looks_like_iso8601
/// 完全一致（定长20字符、以Z结尾、第11个字符是T），是同一套判断逻辑的
/// 独立实现（ROE_Progress_v157.md 已记录：显示层格式化与排序用的原始
/// 字符串比较是两处独立代码，不共享同一份实现，本次沿用这个既有决定，
/// 不做跨 crate 抽象）。
fn format_comment_date(date: &str) -> String {
    let looks_like_iso8601 = date.len() == 20
        && date.ends_with('Z')
        && date.as_bytes().get(10) == Some(&b'T');
    if looks_like_iso8601 {
        format!("{} {}", &date[0..10], &date[11..16])
    } else {
        date.to_string()
    }
}

/// JSON字符串转义（复用现成写法，避免引入完整serde_json::to_string的
/// 依赖复杂度——本函数只需要转义少量字段，与项目里 format! 手写JSON的
/// 既有风格一致，见 roe_get_document_size 等函数）。
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// 返回全文档所有批注讨论串（root+replies）的纯文本内容，JSON格式：
/// `{"threads":[{"root":{"id":..,"author":"..","date":"..","done":..,
/// "text":".."},"replies":[{同root字段结构（无done）}]}]}`
///
/// 非 Writer 文档、句柄无效、或文档没有任何批注时，均返回
/// `{"threads":[]}`（不是错误，是"没有批注"这个正常状态的表示）。
///
/// 调用方必须用 roe_free_string 释放返回值。
#[no_mangle]
pub extern "C" fn roe_get_comment_threads(handle_val: u64) -> *mut c_char {
    let result = handle::with_entry(handle_val, |entry| {
        let RoeDocument::Writer(doc) = &entry.doc else {
            return "{\"threads\":[]}".to_string();
        };

        let threads = doc.comment_threads();
        let mut thread_jsons: Vec<String> = Vec::with_capacity(threads.len());

        for thread in &threads {
            let root_date = thread.root.date.as_deref().unwrap_or("");
            let root_json = format!(
                "{{\"id\":{},\"author\":\"{}\",\"date\":\"{}\",\"done\":{},\"text\":\"{}\"}}",
                thread.root.id,
                json_escape(&thread.root.author),
                json_escape(&format_comment_date(root_date)),
                thread.root.done,
                json_escape(&comment_plain_text(thread.root)),
            );

            let reply_jsons: Vec<String> = thread.replies.iter().map(|reply| {
                let reply_date = reply.date.as_deref().unwrap_or("");
                format!(
                    "{{\"id\":{},\"author\":\"{}\",\"date\":\"{}\",\"text\":\"{}\"}}",
                    reply.id,
                    json_escape(&reply.author),
                    json_escape(&format_comment_date(reply_date)),
                    json_escape(&comment_plain_text(reply)),
                )
            }).collect();

            thread_jsons.push(format!(
                "{{\"root\":{},\"replies\":[{}]}}",
                root_json,
                reply_jsons.join(",")
            ));
        }

        format!("{{\"threads\":[{}]}}", thread_jsons.join(","))
    });

    let json = result.unwrap_or_else(|| {
        set_last_error("roe_get_comment_threads: 无效句柄");
        "{\"threads\":[]}".to_string()
    });

    match CString::new(json) {
        Ok(cs) => cs.into_raw(),
        Err(e) => {
            set_last_error(format!("roe_get_comment_threads: {e}"));
            std::ptr::null_mut()
        }
    }
}

/// 返回"最近一次 roe_render_writer_viewport 调用中，落在可见viewport
/// 范围内"的批注命中坐标列表，JSON格式：
/// `{"hits":[{"comment_id":..,"line_top_pt":..,"line_bottom_pt":..,
/// "color_idx":..}]}`
///
/// line_top_pt/line_bottom_pt 是文档坐标系累计pt（与
/// roe_query_layout_rect / writer_query_cursor_rect 同一套坐标基准，
/// 不含scroll偏移），调用方需要按当前 vp_y 做减法换算成屏幕坐标
/// （换算方式与 roe-api/src/tile.rs::roe_render_writer_viewport 内
/// canvas_offset 的处理方式一致：screen_y = line_top_pt - vp_y +
/// canvas_offset）。
///
/// 非 Writer 文档、句柄无效、或本次viewport内没有任何批注命中，均返回
/// `{"hits":[]}`。
///
/// 语义务必注意：这不是全文档批注坐标，只是"最近一次渲染时可见的那些"，
/// 调用方应在每次 roe_render_writer_viewport 之后立即调用本函数以获取
/// 最新的可见批注列表（气泡跟随滚动效果由调用方每次重渲染后重新调用
/// 本函数实现）。
///
/// 调用方必须用 roe_free_string 释放返回值。
#[no_mangle]
pub extern "C" fn roe_take_comment_hits(handle_val: u64) -> *mut c_char {
    let result = handle::with_entry(handle_val, |entry| {
        let hits = std::mem::take(&mut entry.pending_comment_hits);
        let hit_jsons: Vec<String> = hits.iter().map(|(comment_id, top, bottom, color_idx)| {
            format!(
                "{{\"comment_id\":{},\"line_top_pt\":{:.2},\"line_bottom_pt\":{:.2},\"color_idx\":{}}}",
                comment_id, top, bottom, color_idx
            )
        }).collect();
        format!("{{\"hits\":[{}]}}", hit_jsons.join(","))
    });

    let json = result.unwrap_or_else(|| {
        set_last_error("roe_take_comment_hits: 无效句柄");
        "{\"hits\":[]}".to_string()
    });

    match CString::new(json) {
        Ok(cs) => cs.into_raw(),
        Err(e) => {
            set_last_error(format!("roe_take_comment_hits: {e}"));
            std::ptr::null_mut()
        }
    }
}

// ───────── v283: 插入批注 ─────────
use roe_model::writer::{BodyElement, Comment, Paragraph, Run, WriterDocument};

fn cm_split(runs: &[Run], offset: usize) -> Result<(Vec<Run>, Vec<Run>), String> {
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

fn cm_now_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86400);
    let sod = secs.rem_euclid(86400);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let mut y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    if m <= 2 { y += 1; }
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, sod / 3600, (sod % 3600) / 60, sod % 60)
}

/// 核心逻辑：在 [(pa,oa),(pb,ob)) 上插入一条批注，返回新批注 id。
pub fn insert_comment_core(
    doc: &mut WriterDocument,
    pa: usize, oa: usize, pb: usize, ob: usize,
    author: &str, text: &str, date: &str,
) -> Result<u32, String> {
    let ((pa, oa), (pb, ob)) = if (pa, oa) <= (pb, ob) { ((pa, oa), (pb, ob)) } else { ((pb, ob), (pa, oa)) };
    if pa == pb && oa == ob {
        return Err("请先选中文字".to_string());
    }
    for i in [pa, pb] {
        if !matches!(doc.body.get(i), Some(BodyElement::Paragraph(_))) {
            return Err(format!("段落 {i} 不存在或不是普通段落"));
        }
    }
    let id = doc.comments.iter().map(|c| c.id + 1).max().unwrap_or(0);
    let ref_run = || Run { comment_ref: Some(id), ..Default::default() };

    if pa == pb {
        let Some(BodyElement::Paragraph(p)) = doc.body.get_mut(pa) else { unreachable!() };
        let (left, right) = cm_split(&p.runs, ob)?;
        let (a, mut mid) = cm_split(&left, oa)?;
        if mid.is_empty() {
            return Err("选区内没有内容".to_string());
        }
        mid[0].comment_range_start.push(id);
        let last = mid.len() - 1;
        mid[last].comment_range_end.push(id);
        mid.push(ref_run());
        let mut runs = a;
        runs.extend(mid);
        runs.extend(right);
        p.runs = runs;
    } else {
        {
            let Some(BodyElement::Paragraph(p)) = doc.body.get_mut(pa) else { unreachable!() };
            let (a, mut r) = cm_split(&p.runs, oa)?;
            if r.is_empty() {
                r.push(Run::default());
            }
            r[0].comment_range_start.push(id);
            let mut runs = a;
            runs.extend(r);
            p.runs = runs;
        }
        {
            let Some(BodyElement::Paragraph(p)) = doc.body.get_mut(pb) else { unreachable!() };
            let (mut l, r) = cm_split(&p.runs, ob)?;
            if l.is_empty() {
                l.push(Run::default());
            }
            let last = l.len() - 1;
            l[last].comment_range_end.push(id);
            l.push(ref_run());
            l.extend(r);
            p.runs = l;
        }
    }

    let content: Vec<BodyElement> = text
        .split('\n')
        .map(|seg| BodyElement::Paragraph(Paragraph {
            runs: vec![Run { text: seg.to_string(), ..Default::default() }],
            ..Default::default()
        }))
        .collect();
    doc.comments.push(Comment {
        id,
        author: author.to_string(),
        date: Some(if date.is_empty() { cm_now_iso() } else { date.to_string() }),
        initials: author.chars().next().map(|ch| ch.to_string()),
        content,
        done: false,
        parent_id: None,
    });
    Ok(id)
}

unsafe fn cm_cstr(p: *const c_char) -> String {
    if p.is_null() { return String::new(); }
    std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
}

/// 在选区 [(pa,oa),(pb,ob)) 上插入批注（偏移为段内字节偏移）。
/// date 传空串则取当前 UTC。成功返回新批注 id（>=0），失败返回 -1。带一次撤销快照。
#[no_mangle]
pub extern "C" fn roe_insert_comment(
    handle_val: u64,
    pa: u32, oa: u32, pb: u32, ob: u32,
    author: *const c_char,
    text: *const c_char,
    date: *const c_char,
) -> i32 {
    let author = unsafe { cm_cstr(author) };
    let text = unsafe { cm_cstr(text) };
    let date = unsafe { cm_cstr(date) };
    if text.trim().is_empty() {
        set_last_error("roe_insert_comment: 批注内容不能为空");
        return -1;
    }
    let author = if author.trim().is_empty() { "用户".to_string() } else { author };
    let result = handle::with_entry(handle_val, |entry| -> Result<u32, String> {
        if !matches!(&entry.doc, RoeDocument::Writer(_)) { return Err("非 Writer 文档".to_string()); }
        let prev = entry.doc.clone();
        let r = {
            let RoeDocument::Writer(doc) = &mut entry.doc else { unreachable!() };
            insert_comment_core(doc, pa as usize, oa as usize, pb as usize, ob as usize, &author, &text, &date)
        };
        match r {
            Ok(id) => {
                let first = (pa as usize).min(pb as usize);
                entry.push_undo_snapshot(prev, handle::EditCursorContext::at(first, 0));
                Ok(id)
            }
            Err(e) => { entry.doc = prev; Err(e) }
        }
    });
    let ok = matches!(&result, Some(Ok(_)));
    if ok { handle::invalidate_writer_layout_cache(handle_val); }
    match result {
        Some(Ok(id)) => id as i32,
        Some(Err(m)) => { set_last_error(format!("roe_insert_comment: {m}")); -1 }
        None => { set_last_error("roe_insert_comment: 无效句柄"); -1 }
    }
}


// ───────── v289: 删除批注 ─────────
fn cm_clean_runs(runs: &mut Vec<Run>, ids: &[u32]) -> bool {
    let mut changed = false;
    let mut drop_flags: Vec<bool> = Vec::with_capacity(runs.len());
    for r in runs.iter_mut() {
        let a = r.comment_range_start.len();
        let b = r.comment_range_end.len();
        r.comment_range_start.retain(|x| !ids.contains(x));
        r.comment_range_end.retain(|x| !ids.contains(x));
        if r.comment_range_start.len() != a || r.comment_range_end.len() != b {
            changed = true;
        }
        let mut was_ref = false;
        if let Some(c) = r.comment_ref {
            if ids.contains(&c) {
                r.comment_ref = None;
                was_ref = true;
                changed = true;
            }
        }
        // 只有「纯引用 run」（无文字、无别的标记）才整个删掉
        drop_flags.push(
            was_ref
                && r.text.is_empty()
                && r.comment_range_start.is_empty()
                && r.comment_range_end.is_empty()
                && r.footnote_ref.is_none()
                && r.endnote_ref.is_none()
                && r.field.is_none(),
        );
    }
    let mut it = drop_flags.into_iter();
    runs.retain(|_| !it.next().unwrap_or(false));
    changed
}

fn cm_clean_table(t: &mut roe_model::common::Table, ids: &[u32]) -> bool {
    let mut ch = false;
    for row in t.rows.iter_mut() {
        for cell in row.cells.iter_mut() {
            for c in cell.content.iter_mut() {
                match c {
                    roe_model::common::CellContent::Paragraph(p) => { ch |= cm_clean_runs(&mut p.runs, ids); }
                    roe_model::common::CellContent::Table(inner) => { ch |= cm_clean_table(inner, ids); }
                    _ => {}
                }
            }
        }
    }
    ch
}

/// 删除批注及其回复，并清掉正文（含表格）里对应的起止标记与引用 run。
/// 返回首个被改动的 body 下标（用于撤销后光标定位）。
pub fn delete_comment_core(doc: &mut WriterDocument, id: u32) -> Result<usize, String> {
    if !doc.comments.iter().any(|c| c.id == id) {
        return Err(format!("批注 {id} 不存在"));
    }
    let mut ids = vec![id];
    for c in doc.comments.iter() {
        if c.parent_id == Some(id) { ids.push(c.id); }
    }
    let mut first: Option<usize> = None;
    for (i, el) in doc.body.iter_mut().enumerate() {
        let ch = match el {
            BodyElement::Paragraph(p) => cm_clean_runs(&mut p.runs, &ids),
            BodyElement::Table(t) => cm_clean_table(t, &ids),
            _ => false,
        };
        if ch && first.is_none() { first = Some(i); }
    }
    doc.comments.retain(|c| !ids.contains(&c.id));
    Ok(first.unwrap_or(0))
}

/// 删除批注（连同回复）。成功返回 0，失败返回 -1。带一次撤销快照。
#[no_mangle]
pub extern "C" fn roe_delete_comment(handle_val: u64, comment_id: u32) -> i32 {
    let result = handle::with_entry(handle_val, |entry| -> Result<usize, String> {
        if !matches!(&entry.doc, RoeDocument::Writer(_)) { return Err("非 Writer 文档".to_string()); }
        let prev = entry.doc.clone();
        let r = {
            let RoeDocument::Writer(doc) = &mut entry.doc else { unreachable!() };
            delete_comment_core(doc, comment_id)
        };
        match r {
            Ok(first) => {
                entry.push_undo_snapshot(prev, handle::EditCursorContext::at(first, 0));
                Ok(first)
            }
            Err(e) => { entry.doc = prev; Err(e) }
        }
    });
    let ok = matches!(&result, Some(Ok(_)));
    if ok { handle::invalidate_writer_layout_cache(handle_val); }
    match result {
        Some(Ok(_)) => 0,
        Some(Err(m)) => { set_last_error(format!("roe_delete_comment: {m}")); -1 }
        None => { set_last_error("roe_delete_comment: 无效句柄"); -1 }
    }
}


// ───────── v291: 标记完成 / 编辑批注 ─────────
fn cm_para_of(doc: &WriterDocument, id: u32) -> usize {
    for (i, el) in doc.body.iter().enumerate() {
        if let BodyElement::Paragraph(p) = el {
            if p.runs.iter().any(|r| r.comment_range_start.contains(&id) || r.comment_ref == Some(id)) {
                return i;
            }
        }
    }
    0
}

fn cm_run_with_undo<F>(handle_val: u64, name: &str, f: F) -> i32
where
    F: FnOnce(&mut WriterDocument) -> Result<usize, String>,
{
    let result = handle::with_entry(handle_val, |entry| -> Result<usize, String> {
        if !matches!(&entry.doc, RoeDocument::Writer(_)) { return Err("非 Writer 文档".to_string()); }
        let prev = entry.doc.clone();
        let r = {
            let RoeDocument::Writer(doc) = &mut entry.doc else { unreachable!() };
            f(doc)
        };
        match r {
            Ok(first) => {
                entry.push_undo_snapshot(prev, handle::EditCursorContext::at(first, 0));
                Ok(first)
            }
            Err(e) => { entry.doc = prev; Err(e) }
        }
    });
    let ok = matches!(&result, Some(Ok(_)));
    if ok { handle::invalidate_writer_layout_cache(handle_val); }
    match result {
        Some(Ok(_)) => 0,
        Some(Err(m)) => { set_last_error(format!("{name}: {m}")); -1 }
        None => { set_last_error(format!("{name}: 无效句柄")); -1 }
    }
}

/// 设置批注（连同其回复）的完成状态。成功 0，失败 -1。带撤销快照。
#[no_mangle]
pub extern "C" fn roe_set_comment_done(handle_val: u64, comment_id: u32, done: i32) -> i32 {
    cm_run_with_undo(handle_val, "roe_set_comment_done", |doc| {
        if !doc.comments.iter().any(|c| c.id == comment_id) {
            return Err(format!("批注 {comment_id} 不存在"));
        }
        let v = done != 0;
        for c in doc.comments.iter_mut() {
            if c.id == comment_id || c.parent_id == Some(comment_id) {
                c.done = v;
            }
        }
        Ok(cm_para_of(doc, comment_id))
    })
}

/// 替换批注正文（按换行拆成多段）。成功 0，失败 -1。带撤销快照。
#[no_mangle]
pub extern "C" fn roe_edit_comment(handle_val: u64, comment_id: u32, text: *const c_char) -> i32 {
    let text = unsafe { cm_cstr(text) };
    if text.trim().is_empty() {
        set_last_error("roe_edit_comment: 批注内容不能为空");
        return -1;
    }
    cm_run_with_undo(handle_val, "roe_edit_comment", move |doc| {
        let content: Vec<BodyElement> = text
            .split('\n')
            .map(|seg| BodyElement::Paragraph(Paragraph {
                runs: vec![Run { text: seg.to_string(), ..Default::default() }],
                ..Default::default()
            }))
            .collect();
        let Some(c) = doc.comments.iter_mut().find(|c| c.id == comment_id) else {
            return Err(format!("批注 {comment_id} 不存在"));
        };
        c.content = content;
        Ok(cm_para_of(doc, comment_id))
    })
}


// ───────── v292: 回复批注 ─────────
/// 给根批注添加一条回复（只允许一层）。回复在正文里与父批注同位置挂起止标记与引用 run。
pub fn reply_comment_core(doc: &mut WriterDocument, parent_id: u32, author: &str, text: &str) -> Result<usize, String> {
    let parent_done = {
        let Some(parent) = doc.comments.iter().find(|c| c.id == parent_id) else {
            return Err(format!("批注 {parent_id} 不存在"));
        };
        if parent.parent_id.is_some() {
            return Err("不能回复一条回复".to_string());
        }
        parent.done
    };
    let id = doc.comments.iter().map(|c| c.id + 1).max().unwrap_or(0);
    let mut first: Option<usize> = None;
    for (i, el) in doc.body.iter_mut().enumerate() {
        if let BodyElement::Paragraph(p) = el {
            let mut changed = false;
            let mut out: Vec<Run> = Vec::with_capacity(p.runs.len() + 1);
            for mut r in std::mem::take(&mut p.runs) {
                if r.comment_range_start.contains(&parent_id) {
                    r.comment_range_start.push(id);
                    changed = true;
                }
                if r.comment_range_end.contains(&parent_id) {
                    r.comment_range_end.push(id);
                    changed = true;
                }
                let is_ref = r.comment_ref == Some(parent_id);
                out.push(r);
                if is_ref {
                    out.push(Run { comment_ref: Some(id), ..Default::default() });
                    changed = true;
                }
            }
            p.runs = out;
            if changed && first.is_none() { first = Some(i); }
        }
    }
    let Some(first) = first else {
        return Err("未在正文中找到父批注的锚点".to_string());
    };
    let content: Vec<BodyElement> = text
        .split('\n')
        .map(|seg| BodyElement::Paragraph(Paragraph {
            runs: vec![Run { text: seg.to_string(), ..Default::default() }],
            ..Default::default()
        }))
        .collect();
    doc.comments.push(Comment {
        id,
        author: author.to_string(),
        date: Some(cm_now_iso()),
        initials: author.chars().next().map(|ch| ch.to_string()),
        content,
        done: parent_done,
        parent_id: Some(parent_id),
    });
    Ok(first)
}

/// 添加回复。成功 0，失败 -1。带撤销快照。
#[no_mangle]
pub extern "C" fn roe_reply_comment(handle_val: u64, parent_id: u32, author: *const c_char, text: *const c_char) -> i32 {
    let author = unsafe { cm_cstr(author) };
    let text = unsafe { cm_cstr(text) };
    if text.trim().is_empty() {
        set_last_error("roe_reply_comment: 回复内容不能为空");
        return -1;
    }
    let author = if author.trim().is_empty() { "用户".to_string() } else { author };
    cm_run_with_undo(handle_val, "roe_reply_comment", move |doc| {
        reply_comment_core(doc, parent_id, &author, &text)
    })
}


// ───────── v295: 插入脚注 / 尾注 ─────────
/// 在段落 para 的字节偏移 offset 处插入脚注(kind=0)/尾注(kind=1)引用，并新增对应 Note。
pub fn insert_note_core(doc: &mut WriterDocument, para: usize, offset: usize, kind: u32, text: &str) -> Result<usize, String> {
    use roe_model::writer::{FieldKind, Note, NoteType, ScriptPosition};
    if !matches!(doc.body.get(para), Some(BodyElement::Paragraph(_))) {
        return Err(format!("段落 {para} 不存在或不是普通段落"));
    }
    let is_end = kind == 1;
    if is_end
        && matches!(doc.endnote_num_fmt, roe_model::writer::NumFormat::Arabic)
        && !doc.endnotes.iter().any(|n| matches!(n.note_type, NoteType::Normal))
    {
        doc.endnote_num_fmt = roe_model::writer::NumFormat::LowerRoman;
    }
    let id = {
        let notes = if is_end { &doc.endnotes } else { &doc.footnotes };
        notes.iter().map(|n| n.id + 1).max().unwrap_or(1).max(1)
    };
    let ref_run = Run {
        footnote_ref: if is_end { None } else { Some(id) },
        endnote_ref: if is_end { Some(id) } else { None },
        field: Some(if is_end { FieldKind::EndnoteRef } else { FieldKind::FootnoteRef }),
        script: Some(ScriptPosition::Superscript),
        ..Default::default()
    };
    {
        let Some(BodyElement::Paragraph(p)) = doc.body.get_mut(para) else { unreachable!() };
        let (mut left, right) = cm_split(&p.runs, offset)?;
        left.push(ref_run);
        left.extend(right);
        p.runs = left;
    }
    let field = if is_end { FieldKind::EndnoteRef } else { FieldKind::FootnoteRef };
    let mut content: Vec<BodyElement> = Vec::new();
    for (i, seg) in text.split('\n').enumerate() {
        let mut runs: Vec<Run> = Vec::new();
        if i == 0 {
            runs.push(Run { field: Some(field), script: Some(ScriptPosition::Superscript), ..Default::default() });
            runs.push(Run { text: format!(" {seg}"), ..Default::default() });
        } else {
            runs.push(Run { text: seg.to_string(), ..Default::default() });
        }
        content.push(BodyElement::Paragraph(Paragraph { runs, ..Default::default() }));
    }
    let note = Note { id, content, note_type: NoteType::Normal };
    if is_end { doc.endnotes.push(note); } else { doc.footnotes.push(note); }
    Ok(para)
}

/// 插入脚注/尾注。kind 0=脚注 1=尾注。成功 0，失败 -1。带撤销快照。
#[no_mangle]
pub extern "C" fn roe_insert_footnote(handle_val: u64, para: u32, offset: u32, kind: u32, text: *const c_char) -> i32 {
    let text = unsafe { cm_cstr(text) };
    if text.trim().is_empty() {
        set_last_error("roe_insert_footnote: 注释内容不能为空");
        return -1;
    }
    cm_run_with_undo(handle_val, "roe_insert_footnote", move |doc| {
        insert_note_core(doc, para as usize, offset as usize, kind, &text)
    })
}

// ===== v297: 删除脚注/尾注、插入分栏符、设置分栏 =====
pub fn delete_note_core(doc: &mut WriterDocument, para: usize, offset: usize, kind: u32) -> Result<usize, String> {
    let is_end = kind == 1;
    let id = {
        let Some(BodyElement::Paragraph(p)) = doc.body.get_mut(para) else {
            return Err(format!("段落 {para} 不存在或不是普通段落"));
        };
        let mut refs: Vec<(usize, usize, u32)> = Vec::new();
        let mut pos = 0usize;
        for (i, r) in p.runs.iter().enumerate() {
            let rid = if is_end { r.endnote_ref } else { r.footnote_ref };
            if let Some(rid) = rid { refs.push((i, pos, rid)); }
            pos += r.text.chars().count();
        }
        if refs.is_empty() {
            return Err(if is_end { "光标所在段落没有尾注引用".to_string() } else { "光标所在段落没有脚注引用".to_string() });
        }
        let pick = refs.iter().rev().find(|x| x.1 <= offset).copied().unwrap_or(refs[0]);
        let (ri, _, rid) = pick;
        let pure = {
            let r = &p.runs[ri];
            r.text.is_empty() && r.comment_range_start.is_empty() && r.comment_range_end.is_empty() && r.comment_ref.is_none()
        };
        if pure {
            p.runs.remove(ri);
        } else {
            let r = &mut p.runs[ri];
            r.footnote_ref = None;
            r.endnote_ref = None;
            r.field = None;
        }
        rid
    };
    if is_end { doc.endnotes.retain(|n| n.id != id); } else { doc.footnotes.retain(|n| n.id != id); }
    Ok(para)
}

#[no_mangle]
pub extern "C" fn roe_delete_note(handle_val: u64, para: u32, offset: u32, kind: u32) -> i32 {
    cm_run_with_undo(handle_val, "roe_delete_note", move |doc| {
        delete_note_core(doc, para as usize, offset as usize, kind)
    })
}

pub fn insert_column_break_core(doc: &mut WriterDocument, para: usize, offset: usize) -> Result<usize, String> {
    let multi = doc.columns.num_cols > 1
        || doc.sections.iter().any(|s| s.columns.as_ref().map(|c| c.num_cols > 1).unwrap_or(false));
    if !multi {
        return Err("当前文档为单栏，请先在「分栏」里设置两栏或三栏".to_string());
    }
    let (left, right) = {
        let Some(BodyElement::Paragraph(p)) = doc.body.get(para) else {
            return Err(format!("段落 {para} 不存在或不是普通段落"));
        };
        cm_split(&p.runs, offset)?
    };
    let second = {
        let Some(BodyElement::Paragraph(p)) = doc.body.get_mut(para) else { unreachable!() };
        let mut second = p.clone();
        second.runs = right;
        second.bookmarks.clear();
        second.page_break_before = false;
        p.runs = left;
        p.column_break_after = true;
        second
    };
    doc.body.insert(para + 1, BodyElement::Paragraph(second));
    Ok(para)
}

#[no_mangle]
pub extern "C" fn roe_writer_insert_column_break(handle_val: u64, para: u32, offset: u32) -> i32 {
    cm_run_with_undo(handle_val, "roe_writer_insert_column_break", move |doc| {
        insert_column_break_core(doc, para as usize, offset as usize)
    })
}

pub fn set_columns_core(doc: &mut WriterDocument, num: usize, spacing: f32) -> Result<usize, String> {
    if num < 1 || num > 6 { return Err(format!("栏数 {num} 超出范围 1-6")); }
    doc.columns = roe_model::writer::DocumentColumns {
        num_cols: num,
        uniform_spacing: spacing.max(0.0),
        col_widths: Vec::new(),
        col_spacing: Vec::new(),
    };
    for s in doc.sections.iter_mut() { s.columns = None; }
    Ok(0)
}

#[no_mangle]
pub extern "C" fn roe_writer_set_columns(handle_val: u64, num: u32, spacing_pt: f32) -> i32 {
    cm_run_with_undo(handle_val, "roe_writer_set_columns", move |doc| {
        set_columns_core(doc, num as usize, spacing_pt)
    })
}

// ===== v298: Writer 插入图片 =====
pub fn insert_image_core(doc: &mut WriterDocument, para: usize, offset: usize, data: Vec<u8>, mime: String, w: f32, h: f32) -> Result<usize, String> {
    use roe_model::writer::InlineImage;
    if !matches!(doc.body.get(para), Some(BodyElement::Paragraph(_))) {
        return Err(format!("段落 {para} 不存在或不是普通段落"));
    }
    let img = BodyElement::Image(InlineImage {
        image: roe_model::common::ImageData { data, mime, ..Default::default() },
        width_pt: w,
        height_pt: h,
        ..Default::default()
    });
    if offset == 0 {
        doc.body.insert(para, img);
        return Ok(para);
    }
    let (left, right) = {
        let Some(BodyElement::Paragraph(p)) = doc.body.get(para) else { unreachable!() };
        cm_split(&p.runs, offset)?
    };
    let right_empty = right.iter().all(|r| {
        r.text.is_empty() && r.footnote_ref.is_none() && r.endnote_ref.is_none()
            && r.comment_ref.is_none() && r.formula.is_none() && r.comment_range_end.is_empty()
    });
    if right_empty {
        doc.body.insert(para + 1, img);
        return Ok(para);
    }
    let second = {
        let Some(BodyElement::Paragraph(p)) = doc.body.get_mut(para) else { unreachable!() };
        let mut s = p.clone();
        s.runs = right;
        s.bookmarks.clear();
        s.page_break_before = false;
        p.runs = left;
        p.column_break_after = false;
        s
    };
    doc.body.insert(para + 1, img);
    doc.body.insert(para + 2, BodyElement::Paragraph(second));
    Ok(para)
}

#[no_mangle]
pub extern "C" fn roe_writer_insert_image(handle_val: u64, para: u32, offset: u32, bytes: *const u8, len: usize, mime: *const c_char, width_pt: f32, height_pt: f32) -> i32 {
    if bytes.is_null() || len == 0 {
        set_last_error("roe_writer_insert_image: 图片数据为空");
        return -1;
    }
    let data = unsafe { std::slice::from_raw_parts(bytes, len) }.to_vec();
    let mime = unsafe { cm_cstr(mime) };
    let mime = if mime.is_empty() { "image/png".to_string() } else { mime };
    cm_run_with_undo(handle_val, "roe_writer_insert_image", move |doc| {
        insert_image_core(doc, para as usize, offset as usize, data.clone(), mime.clone(), width_pt, height_pt)
    })
}

// ===== v299: Writer 对象命中/缩放/删除（内联图片） =====
fn hit_object_in_entry(entry: &mut handle::DocumentEntry, x: f32, y: f32) -> Option<String> {
    let RoeDocument::Writer(doc) = &entry.doc else { return None; };
    let layout = entry.writer_layout_cache.as_ref()?;
    let arena = &layout.arena;
    for &pid in arena.page_order.iter() {
        let page = &arena.pages[pid];
        for &fid in page.frames.iter() {
            let f = &arena.frames[fid];
            if !f.text_lines().is_empty() { continue; }
            if let Some(BodyElement::Shape(sh)) = doc.body.get(f.para_index) {
                let cx = page.columns.col_x_offsets.get(f.column_index).copied().unwrap_or(0.0);
                let (rx, ry, rw, rh) = (page.margin_left + cx + sh.x_pt, f.y_on_page, sh.width_pt, sh.height_pt);
                if x >= rx && x <= rx + rw && y >= ry && y <= ry + rh {
                    return Some(format!(
                        "{{\"body_index\":{},\"x\":{},\"y\":{},\"w\":{},\"h\":{}}}",
                        f.para_index, rx, ry, rw, rh
                    ));
                }
            }
            let chart_wh = match doc.body.get(f.para_index) {
                Some(BodyElement::InlineChart { width_pt, height_pt, .. }) => Some((*width_pt, *height_pt)),
                Some(BodyElement::InlineStatChart { width_pt, height_pt, .. }) => Some((*width_pt, *height_pt)),
                _ => None,
            };
            if let Some((cw, ch)) = chart_wh {
                let (rx, ry) = (page.margin_left, f.y_on_page);
                if x >= rx && x <= rx + cw && y >= ry && y <= ry + ch {
                    return Some(format!(
                        "{{\"body_index\":{},\"x\":{},\"y\":{},\"w\":{},\"h\":{}}}",
                        f.para_index, rx, ry, cw, ch
                    ));
                }
            }
            if let Some(BodyElement::Image(img)) = doc.body.get(f.para_index) {
                let (rx, ry, rw, rh) = (page.margin_left, f.y_on_page, img.width_pt, img.height_pt);
                if x >= rx && x <= rx + rw && y >= ry && y <= ry + rh {
                    return Some(format!(
                        "{{\"body_index\":{},\"x\":{},\"y\":{},\"w\":{},\"h\":{}}}",
                        f.para_index, rx, ry, rw, rh
                    ));
                }
            }
        }
    }
    None
}

#[no_mangle]
pub extern "C" fn roe_writer_hit_object(handle_val: u64, x: f64, y: f64) -> *mut c_char {
    if !handle::ensure_writer_layout(handle_val) { return std::ptr::null_mut(); }
    let r = handle::with_entry(handle_val, |entry| hit_object_in_entry(entry, x as f32, y as f32)).flatten();
    match r {
        Some(s) => match std::ffi::CString::new(s) { Ok(c) => c.into_raw(), Err(_) => std::ptr::null_mut() },
        None => std::ptr::null_mut(),
    }
}

pub fn resize_object_core(doc: &mut WriterDocument, idx: usize, w: f32, h: f32) -> Result<usize, String> {
    match doc.body.get_mut(idx) {
        Some(BodyElement::Image(img)) => {
            img.width_pt = w.max(8.0);
            img.height_pt = h.max(8.0);
            Ok(idx)
        }
        Some(BodyElement::Shape(sh)) => {
            sh.width_pt = w.max(8.0);
            sh.height_pt = h.max(8.0);
            let name = match &sh.geometry {
                roe_model::common::ShapeGeometry::PresetPath(d) if !d.preset_name.is_empty() => Some(d.preset_name.clone()),
                _ => None,
            };
            if let Some(n) = name {
                if let Some(p) = ws_preset(&n, sh.width_pt, sh.height_pt) {
                    sh.geometry = roe_model::common::ShapeGeometry::PresetPath(p);
                }
            }
            Ok(idx)
        }
        Some(BodyElement::InlineChart { model, width_pt, height_pt }) => {
            *width_pt = w.max(8.0);
            *height_pt = h.max(8.0);
            model.width_pt = *width_pt;
            model.height_pt = *height_pt;
            Ok(idx)
        }
        Some(BodyElement::InlineStatChart { width_pt, height_pt, .. }) => {
            *width_pt = w.max(8.0);
            *height_pt = h.max(8.0);
            Ok(idx)
        }
        _ => Err(format!("body[{idx}] 不是图片或形状")),
    }
}

#[no_mangle]
pub extern "C" fn roe_writer_resize_object(handle_val: u64, body_index: u32, width_pt: f32, height_pt: f32) -> i32 {
    cm_run_with_undo(handle_val, "roe_writer_resize_object", move |doc| {
        resize_object_core(doc, body_index as usize, width_pt, height_pt)
    })
}

pub fn delete_object_core(doc: &mut WriterDocument, idx: usize) -> Result<usize, String> {
    if !matches!(doc.body.get(idx), Some(BodyElement::Image(_)) | Some(BodyElement::Shape(_)) | Some(BodyElement::InlineChart { .. }) | Some(BodyElement::InlineStatChart { .. })) {
        return Err(format!("body[{idx}] 不是图片或形状"));
    }
    doc.body.remove(idx);
    Ok(idx.min(doc.body.len().saturating_sub(1)))
}

#[no_mangle]
pub extern "C" fn roe_writer_delete_object(handle_val: u64, body_index: u32) -> i32 {
    cm_run_with_undo(handle_val, "roe_writer_delete_object", move |doc| {
        delete_object_core(doc, body_index as usize)
    })
}


// ───────── v302: Writer 插入形状 ─────────
fn ws_preset(name: &str, w: f32, h: f32) -> Option<roe_model::common::PresetPathData> {
    use std::collections::HashMap;
    let formula = roe_model::preset_geometry::find_preset_shape(name)?;
    let empty: HashMap<&str, f64> = HashMap::new();
    let path = roe_model::preset_geometry::eval_preset_shape(formula, w as f64, h as f64, &empty);
    Some(roe_model::common::PresetPathData {
        commands: path,
        preset_name: name.to_string(),
        adj_overrides: Vec::new(),
    })
}

fn ws_build_shape(name: &str, x_pt: f32, y_pt: f32, w: f32, h: f32) -> roe_model::writer::Shape {
    use roe_model::common::{ArrowType, Color, FillStyle, ShapeGeometry, ShapeStyle, StrokeStyle};
    let (name, want_head, want_tail) = match name {
        "lineWithArrow" => ("line", false, true),
        "lineWithTwoArrows" => ("line", true, true),
        "bentConnector5WithArrow" => ("bentConnector5", false, true),
        "bentConnector5WithTwoArrows" => ("bentConnector5", true, true),
        "curvedConnector3WithArrow" => ("curvedConnector3", false, true),
        "curvedConnector3WithTwoArrows" => ("curvedConnector3", true, true),
        other => (other, false, false),
    };
    let is_line = name == "line" || name.starts_with("straightConnector")
        || name.starts_with("bentConnector") || name.starts_with("curvedConnector");
    let geometry = match name {
        "rect" => ShapeGeometry::Rect,
        "ellipse" => ShapeGeometry::Ellipse,
        "line" => ShapeGeometry::Line,
        other => match ws_preset(other, w, h) {
            Some(p) => ShapeGeometry::PresetPath(p),
            None => ShapeGeometry::Rect,
        },
    };
    let stroke_color = Color { r: 0x38, g: 0x5D, b: 0x8A, a: 255 };
    let style = if is_line {
        ShapeStyle {
            fill: Some(FillStyle::None),
            stroke: Some(StrokeStyle { color: stroke_color, width: 1.5 }),
            head_arrow: if want_head { Some(ArrowType::Triangle) } else { None },
            tail_arrow: if want_tail { Some(ArrowType::Triangle) } else { None },
            ..Default::default()
        }
    } else {
        ShapeStyle {
            fill: Some(FillStyle::Solid(Color { r: 0x4F, g: 0x81, b: 0xBD, a: 255 })),
            stroke: Some(StrokeStyle { color: stroke_color, width: 1.0 }),
            ..Default::default()
        }
    };
    roe_model::writer::Shape {
        geometry,
        style,
        text_content: Vec::new(),
        width_pt: w,
        height_pt: h,
        x_pt,
        y_pt,
        anchor_type_h: Some("column".to_string()),
        anchor_type_v: Some("margin".to_string()),
        align_h: None,
        align_v: None,
        wrap_type: Some("none".to_string()),
        dist_t_pt: 0.0,
        dist_b_pt: 0.0,
        dist_l_pt: 0.0,
        dist_r_pt: 0.0,
        flip_v: None,
    rotation: None, }
}

/// 在文档坐标 (x,y)（pt，不含画布偏移）处插入形状，宽高 w×h。
/// 返回形状插入后的 body 下标（>=0），失败 -1。带撤销。
#[no_mangle]
pub extern "C" fn roe_writer_insert_shape(handle_val: u64, geometry: *const c_char, x: f32, y: f32, w: f32, h: f32) -> i32 {
    if geometry.is_null() {
        set_last_error("roe_writer_insert_shape: geometry 为空指针".to_string());
        return -1;
    }
    let name = match unsafe { std::ffi::CStr::from_ptr(geometry) }.to_str() {
        Ok(s) => s.to_string(),
        Err(_) => {
            set_last_error("roe_writer_insert_shape: geometry 不是合法 UTF-8".to_string());
            return -1;
        }
    };
    if !handle::ensure_writer_layout(handle_val) {
        set_last_error("roe_writer_insert_shape: 布局失败".to_string());
        return -1;
    }
    let w = w.max(8.0);
    let h = h.max(1.0);
    let result = handle::with_entry(handle_val, |entry| -> Result<usize, String> {
        if !matches!(&entry.doc, RoeDocument::Writer(_)) { return Err("非 Writer 文档".to_string()); }
        let (idx, x_pt, y_pt) = {
            let layout = entry.writer_layout_cache.as_ref().ok_or("没有布局缓存")?;
            let arena = &layout.arena;
            let mut target = None;
            let mut last = None;
            for &pid in arena.page_order.iter() {
                last = Some(pid);
                if y < arena.page_global_y(pid) + arena.pages[pid].height { target = Some(pid); break; }
            }
            let pid = target.or(last).ok_or("文档没有页面")?;
            let page = &arena.pages[pid];
            let idx = page.frames.iter().map(|&fid| arena.frames[fid].para_index).min().unwrap_or(usize::MAX);
            (idx, x - page.margin_left, y - arena.page_content_global_y(pid))
        };
        let shape = ws_build_shape(&name, x_pt, y_pt, w, h);
        let prev = entry.doc.clone();
        let at = {
            let RoeDocument::Writer(doc) = &mut entry.doc else { unreachable!() };
            let at = idx.min(doc.body.len());
            doc.body.insert(at, BodyElement::Shape(shape));
            at
        };
        entry.push_undo_snapshot(prev, handle::EditCursorContext::at(at, 0));
        Ok(at)
    });
    match result {
        Some(Ok(at)) => { handle::invalidate_writer_layout_cache(handle_val); at as i32 }
        Some(Err(m)) => { set_last_error(format!("roe_writer_insert_shape: {m}")); -1 }
        None => { set_last_error("roe_writer_insert_shape: 无效句柄".to_string()); -1 }
    }
}


// ───────── v305: Writer 形状样式 / 对象类型 ─────────
fn ws_parse_hex(s: &str) -> Option<roe_model::common::Color> {
    let t = s.trim().trim_start_matches('#');
    if t.len() != 6 { return None; }
    let v = u32::from_str_radix(t, 16).ok()?;
    Some(roe_model::common::Color { r: (v >> 16) as u8, g: (v >> 8) as u8, b: v as u8, a: 255 })
}

/// fill: ""=不改, "none"=无填充, "#rrggbb"=纯色
/// stroke: ""=不改, "none"=无边框, "#rrggbb"=边框颜色
/// width: <0 不改, 否则边框粗细 pt
pub fn set_shape_style_core(doc: &mut WriterDocument, idx: usize, fill: &str, stroke: &str, width: f32) -> Result<usize, String> {
    use roe_model::common::{FillStyle, StrokeStyle, Color};
    match doc.body.get_mut(idx) {
        Some(BodyElement::Shape(sh)) => {
            if !fill.is_empty() {
                if let Some(g) = fill.strip_prefix("grad:") {
                    let parts: Vec<&str> = g.split(',').collect();
                    if parts.len() < 3 { return Err(format!("渐变参数无效: {g}")); }
                    let c1 = ws_parse_hex(parts[0]).ok_or_else(|| format!("颜色无效: {}", parts[0]))?;
                    let c2 = ws_parse_hex(parts[1]).ok_or_else(|| format!("颜色无效: {}", parts[1]))?;
                    let ang: f32 = parts[2].trim().parse().unwrap_or(0.0);
                    sh.style.fill = Some(FillStyle::Gradient {
                        stops: vec![
                            roe_model::common::GradientStop { pos: 0.0, color: c1 },
                            roe_model::common::GradientStop { pos: 1.0, color: c2 },
                        ],
                        kind: roe_model::common::GradientKind::Linear { angle: ang },
                        rotate_with_shape: true,
                    });
                } else if fill == "none" {
                    sh.style.fill = Some(FillStyle::None);
                } else {
                    let c = ws_parse_hex(fill).ok_or_else(|| format!("颜色无效: {fill}"))?;
                    sh.style.fill = Some(FillStyle::Solid(c));
                }
            }
            if !stroke.is_empty() {
                if stroke == "none" {
                    sh.style.stroke = None;
                } else {
                    let c = ws_parse_hex(stroke).ok_or_else(|| format!("颜色无效: {stroke}"))?;
                    match &mut sh.style.stroke {
                        Some(s) => { s.color = c; }
                        None => { sh.style.stroke = Some(StrokeStyle { color: c, width: 1.0 }); }
                    }
                }
            }
            if width >= 0.0 {
                match &mut sh.style.stroke {
                    Some(s) => { s.width = width; }
                    None => {
                        sh.style.stroke = Some(StrokeStyle { color: Color { r: 0x38, g: 0x5D, b: 0x8A, a: 255 }, width });
                    }
                }
            }
            Ok(idx)
        }
        _ => Err(format!("body[{idx}] 不是形状")),
    }
}

#[no_mangle]
pub extern "C" fn roe_writer_set_shape_style(handle_val: u64, body_index: u32, fill: *const c_char, stroke: *const c_char, stroke_w: f32) -> i32 {
    let fill = unsafe { cm_cstr(fill) }.to_string();
    let stroke = unsafe { cm_cstr(stroke) }.to_string();
    cm_run_with_undo(handle_val, "roe_writer_set_shape_style", move |doc| {
        set_shape_style_core(doc, body_index as usize, fill.trim(), stroke.trim(), stroke_w)
    })
}

/// 对象类型：1=内联图片 2=形状 0=其他/无效
#[no_mangle]
pub extern "C" fn roe_writer_object_kind(handle_val: u64, body_index: u32) -> i32 {
    handle::with_entry(handle_val, |entry| -> i32 {
        match &entry.doc {
            RoeDocument::Writer(doc) => match doc.body.get(body_index as usize) {
                Some(BodyElement::Image(_)) => 1,
                Some(BodyElement::Shape(_)) => 2,
                Some(BodyElement::InlineChart { .. }) | Some(BodyElement::InlineStatChart { .. }) => 3,
                _ => 0,
            },
            _ => 0,
        }
    }).unwrap_or(0)
}


// ───────── v308: 对象旋转 ─────────
#[no_mangle]
pub extern "C" fn roe_writer_set_object_rotation(handle_val: u64, body_index: u32, degrees: f32) -> i32 {
    cm_run_with_undo(handle_val, "roe_writer_set_object_rotation", move |doc| {
        let d = degrees.rem_euclid(360.0);
        let v = if d < 0.01 || d > 359.99 { None } else { Some(d) };
        match doc.body.get_mut(body_index as usize) {
            Some(BodyElement::Shape(sh)) => { sh.rotation = v; Ok(body_index as usize) }
            Some(BodyElement::Image(img)) => { img.rotation = v; Ok(body_index as usize) }
            _ => Err(format!("body[{body_index}] 不是图片或形状")),
        }
    })
}

#[no_mangle]
pub extern "C" fn roe_writer_object_rotation(handle_val: u64, body_index: u32) -> f32 {
    handle::with_entry(handle_val, |entry| -> f32 {
        match &entry.doc {
            RoeDocument::Writer(doc) => match doc.body.get(body_index as usize) {
                Some(BodyElement::Shape(sh)) => sh.rotation.unwrap_or(0.0),
                Some(BodyElement::Image(img)) => img.rotation.unwrap_or(0.0),
                _ => 0.0,
            },
            _ => 0.0,
        }
    }).unwrap_or(0.0)
}


// ───────── v309: Writer 插入表格 ─────────
/// 在 body[after_body_index] 之后插入 rows x cols 空表格，返回表格的 body 下标。带撤销。
#[no_mangle]
pub extern "C" fn roe_writer_insert_table(handle_val: u64, after_body_index: u32, rows: u32, cols: u32) -> i32 {
    cm_run_with_undo(handle_val, "roe_writer_insert_table", move |doc| {
        use roe_model::common::{CellContent, Color, StyleRef, Table, TableCell, TableRow};
        let rows = (rows as usize).clamp(1, 100);
        let cols = (cols as usize).clamp(1, 63);
        let black = Some(Color { r: 0, g: 0, b: 0, a: 255 });
        let col_w = (453.0f32 / cols as f32).max(20.0);
        let table = Table {
            float_pos: None,
            col_widths: vec![col_w; cols],
            style_id: None,
            first_row: false, first_col: false, last_row: false, last_col: false,
            band_row: false, band_col: false,
            default_border_color: black,
            default_border_width: Some(0.5),
            inside_h_color: black,
            inside_h_width: Some(0.5),
            inside_v_color: black,
            inside_v_width: Some(0.5),
            rows: (0..rows).map(|_| TableRow {
                cells: (0..cols).map(|_| TableCell {
                    content: vec![CellContent::Paragraph(roe_model::writer::Paragraph {
                        style: StyleRef("Default".into()),
                        runs: vec![],
                        ..Default::default()
                    })],
                    col_span: 1,
                    row_span: 1,
                    style: None,
                    hidden: false,
                    bg_color: None,
                    border_color: None, border_width: None,
                    pad_left: None, pad_right: None,
                    pad_top: None, pad_bottom: None,
                }).collect(),
                row_height: None,
            }).collect(),
        };
        let at = (after_body_index as usize + 1).min(doc.body.len());
        doc.body.insert(at, BodyElement::Table(table));
        if at + 1 >= doc.body.len() {
            doc.body.push(BodyElement::Paragraph(roe_model::writer::Paragraph::default()));
        }
        Ok(at)
    })
}


// ───────── v318f: Writer 插入图表 ─────────
/// 在 body[para] 的 offset 处插入内联图表。借用 insert_image_core 的拆段逻辑，
/// 先放一个占位图片，再把该位置换成图表元素。
pub fn insert_chart_core(doc: &mut WriterDocument, para: usize, offset: usize, json: &str, w: f32, h: f32, is_stat: bool) -> Result<usize, String> {
    let elem = if is_stat {
        let model = serde_json::from_str::<roe_model::stat_chart::StatChartModel>(json)
            .map_err(|e| format!("统计图 JSON 解析失败: {e}"))?;
        BodyElement::InlineStatChart { model, width_pt: w, height_pt: h }
    } else {
        let mut model = serde_json::from_str::<roe_model::impress::ChartModel>(json)
            .map_err(|e| format!("图表 JSON 解析失败: {e}"))?;
        model.width_pt = w;
        model.height_pt = h;
        BodyElement::InlineChart { model, width_pt: w, height_pt: h }
    };
    let first = insert_image_core(doc, para, offset, vec![0u8], "image/png".to_string(), w, h)?;
    let idx = if offset == 0 { first } else { first + 1 };
    if !matches!(doc.body.get(idx), Some(BodyElement::Image(_))) {
        return Err(format!("插入图表定位失败 idx={idx}"));
    }
    doc.body[idx] = elem;
    Ok(first)
}

// v318m: 读取内联图表的 ChartModel JSON（编辑数据弹窗用）。失败返回 null。
#[no_mangle]
pub extern "C" fn roe_writer_get_chart_json(handle_val: u64, body_index: u32) -> *mut c_char {
    let result = handle::with_entry(handle_val, |entry| -> Result<String, String> {
        let RoeDocument::Writer(doc) = &entry.doc else { return Err("非 Writer 文档".to_string()); };
        match doc.body.get(body_index as usize) {
            Some(BodyElement::InlineChart { model, .. }) => {
                serde_json::to_string(model).map_err(|e| format!("序列化失败: {e}"))
            }
            Some(BodyElement::InlineStatChart { .. }) => Err("统计图暂不支持编辑数据".to_string()),
            _ => Err("该位置不是图表".to_string()),
        }
    });
    match result {
        Some(Ok(s)) => match std::ffi::CString::new(s) { Ok(c) => c.into_raw(), Err(_) => std::ptr::null_mut() },
        Some(Err(m)) => { set_last_error(format!("roe_writer_get_chart_json: {m}")); std::ptr::null_mut() }
        None => { set_last_error("roe_writer_get_chart_json: 无效句柄"); std::ptr::null_mut() }
    }
}

// v318m: 用新的 ChartModel JSON 替换内联图表内容，保留原尺寸，计入撤销栈。
#[no_mangle]
pub extern "C" fn roe_writer_set_chart_json(handle_val: u64, body_index: u32, chart_json: *const c_char) -> i32 {
    let json = unsafe { cm_cstr(chart_json) };
    if json.is_empty() {
        set_last_error("roe_writer_set_chart_json: chart_json 为空");
        return -1;
    }
    cm_run_with_undo(handle_val, "roe_writer_set_chart_json", move |doc| {
        let mut nm = serde_json::from_str::<roe_model::impress::ChartModel>(&json)
            .map_err(|e| format!("图表 JSON 解析失败: {e}"))?;
        match doc.body.get_mut(body_index as usize) {
            Some(BodyElement::InlineChart { model, width_pt, height_pt }) => {
                nm.id = model.id;
                nm.x_pt = model.x_pt;
                nm.y_pt = model.y_pt;
                nm.width_pt = *width_pt;
                nm.height_pt = *height_pt;
                *model = nm;
                Ok(body_index as usize)
            }
            _ => Err("该位置不是可编辑的图表".to_string()),
        }
    })
}

#[no_mangle]
pub extern "C" fn roe_writer_insert_chart(handle_val: u64, para: u32, offset: u32, chart_json: *const c_char, width_pt: f32, height_pt: f32, is_stat: i32) -> i32 {
    let json = unsafe { cm_cstr(chart_json) };
    if json.is_empty() {
        set_last_error("roe_writer_insert_chart: chart_json 为空");
        return -1;
    }
    if !(width_pt > 0.0 && height_pt > 0.0) {
        set_last_error("roe_writer_insert_chart: 宽高必须大于 0");
        return -1;
    }
    cm_run_with_undo(handle_val, "roe_writer_insert_chart", move |doc| {
        insert_chart_core(doc, para as usize, offset as usize, &json, width_pt, height_pt, is_stat != 0)
    })
}
