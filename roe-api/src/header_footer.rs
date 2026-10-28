//! v278/v280: 页眉页脚（文字、页码独立段落、各自对齐、首页不同、奇偶页不同）

use crate::error::{set_last_error, RoeErrorCode};
use crate::handle;
use roe_model::style::Alignment;
use roe_model::writer::{FieldKind, HeaderFooterSet, Paragraph, Run};
use roe_parser::RoeDocument;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;

unsafe fn read_cstr(p: *const c_char) -> String {
    if p.is_null() { return String::new(); }
    CStr::from_ptr(p).to_string_lossy().into_owned()
}

fn plain(text: &str) -> Run {
    Run { text: text.to_string(), ..Default::default() }
}

fn field(kind: FieldKind, cache: &str) -> Run {
    Run { text: cache.to_string(), field: Some(kind), ..Default::default() }
}

fn align_of(a: i32) -> Alignment {
    match a { 0 => Alignment::Left, 2 => Alignment::Right, _ => Alignment::Center }
}

fn align_to(a: Option<Alignment>) -> i32 {
    match a { Some(Alignment::Left) => 0, Some(Alignment::Right) => 2, _ => 1 }
}

fn text_para(text: &str, al: i32) -> Paragraph {
    Paragraph { runs: vec![plain(text)], alignment: Some(align_of(al)), ..Default::default() }
}

/// mode 0=N 1=第N页 2=N/总页数
fn num_para(mode: i32, al: i32) -> Paragraph {
    let mut runs: Vec<Run> = Vec::new();
    match mode {
        1 => {
            runs.push(plain("第 "));
            runs.push(field(FieldKind::PageNumber, "1"));
            runs.push(plain(" 页"));
        }
        2 => {
            runs.push(field(FieldKind::PageNumber, "1"));
            runs.push(plain(" / "));
            runs.push(field(FieldKind::NumPages, "1"));
        }
        _ => runs.push(field(FieldKind::PageNumber, "1")),
    }
    Paragraph { runs, alignment: Some(align_of(al)), ..Default::default() }
}

fn build(text: &str, text_align: i32, with_num: bool, mode: i32, num_align: i32) -> Option<Vec<Paragraph>> {
    let mut v: Vec<Paragraph> = Vec::new();
    if !text.trim().is_empty() { v.push(text_para(text, text_align)); }
    if with_num { v.push(num_para(mode, num_align)); }
    if v.is_empty() { None } else { Some(v) }
}

fn apply(set: &mut HeaderFooterSet, content: Option<Vec<Paragraph>>, title_page: bool) {
    set.default = content;
    set.first = if title_page { Some(Vec::new()) } else { None };
    set.even = None;
}

/// v280: 设置页眉页脚。
/// header_align/footer_align/num_align: 0左 1中 2右；num_pos: 0不显示 1页眉 2页脚；
/// num_mode: 0=N 1=第N页 2=N/总页数。对顶层及所有分节统一设置，带一次撤销快照。
#[no_mangle]
pub extern "C" fn roe_set_header_footer(
    handle: u64,
    header: *const c_char,
    footer: *const c_char,
    header_align: i32,
    footer_align: i32,
    num_pos: i32,
    num_mode: i32,
    num_align: i32,
    title_page: i32,
    even_odd: i32,
) -> i32 {
    let header = unsafe { read_cstr(header) };
    let footer = unsafe { read_cstr(footer) };
    if !(0..=2).contains(&num_pos) || !(0..=2).contains(&num_mode) {
        set_last_error("roe_set_header_footer: 页码参数非法");
        return RoeErrorCode::ScriptError as i32;
    }
    let h = build(&header, header_align, num_pos == 1, num_mode, num_align);
    let f = build(&footer, footer_align, num_pos == 2, num_mode, num_align);
    let tp = title_page != 0;
    let eo = even_odd != 0;
    let result = handle::with_entry(handle, |entry| -> Result<(), String> {
        if !matches!(&entry.doc, RoeDocument::Writer(_)) { return Err("非 Writer 文档".to_string()); }
        let prev = entry.doc.clone();
        let RoeDocument::Writer(doc) = &mut entry.doc else { unreachable!() };
        apply(&mut doc.header, h.clone(), tp);
        apply(&mut doc.footer, f.clone(), tp);
        doc.title_page = tp;
        doc.even_and_odd_headers = eo;
        for s in doc.sections.iter_mut() {
            apply(s.header.get_or_insert_with(Default::default), h.clone(), tp);
            apply(s.footer.get_or_insert_with(Default::default), f.clone(), tp);
            s.title_page = Some(tp);
            s.even_and_odd_headers = Some(eo);
        }
        entry.push_undo_snapshot(prev, handle::EditCursorContext::at(0, 0));
        Ok(())
    });
    let ok = matches!(&result, Some(Ok(())));
    if ok { handle::invalidate_writer_layout_cache(handle); }
    match result {
        Some(Ok(())) => RoeErrorCode::Ok as i32,
        Some(Err(m)) => { set_last_error(format!("roe_set_header_footer: {m}")); RoeErrorCode::ScriptError as i32 }
        None => { set_last_error("roe_set_header_footer: 无效句柄"); RoeErrorCode::InvalidHandle as i32 }
    }
}

fn esc(s: &str) -> String {
    let mut o = String::new();
    for c in s.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            '"' => o.push_str("\\\""),
            '\n' => o.push_str("\\n"),
            '\r' => {}
            '\t' => o.push(' '),
            c if (c as u32) < 0x20 => {}
            c => o.push(c),
        }
    }
    o
}

const DECOR: [&str; 4] = ["第 ", " 页", " / ", "    "];

/// 扫描一组段落：(文字, 文字对齐, 页码(格式, 对齐))
fn scan(ps: &[Paragraph]) -> (String, i32, Option<(i32, i32)>) {
    let mut text = String::new();
    let mut text_align = 1;
    let mut got_text_align = false;
    let mut num: Option<(i32, i32)> = None;
    let mut extra = String::new();
    for p in ps {
        let has_pn = p.runs.iter().any(|r| r.field == Some(FieldKind::PageNumber));
        let has_np = p.runs.iter().any(|r| r.field == Some(FieldKind::NumPages));
        let plain_txt: String = p.runs.iter()
            .filter(|r| r.field.is_none() && !DECOR.contains(&r.text.as_str()))
            .map(|r| r.text.as_str()).collect();
        if has_pn || has_np {
            if num.is_none() {
                let has_di = p.runs.iter().any(|r| r.field.is_none() && r.text == "第 ");
                let mode = if has_np { 2 } else if has_di { 1 } else { 0 };
                num = Some((mode, align_to(p.alignment)));
            }
            extra.push_str(&plain_txt);
        } else {
            let t: String = p.runs.iter().map(|r| r.text.as_str()).collect();
            if !t.is_empty() {
                if !text.is_empty() { text.push('\n'); }
                text.push_str(&t);
                if !got_text_align { text_align = align_to(p.alignment); got_text_align = true; }
            }
        }
    }
    if text.is_empty() && !extra.is_empty() { text = extra; }
    (text, text_align, num)
}

/// 读取页眉页脚（顶层 default 档），JSON：
/// {"header","footer","header_align","footer_align","num_pos","num_mode","num_align","title_page","even_odd"}
#[no_mangle]
pub extern "C" fn roe_get_header_footer(handle: u64) -> *mut c_char {
    let result = handle::with_entry(handle, |entry| -> Result<String, String> {
        let RoeDocument::Writer(doc) = &entry.doc else { return Err("非 Writer 文档".to_string()); };
        let (ht, ha, hn) = doc.header.default.as_deref().map(scan).unwrap_or((String::new(), 1, None));
        let (ft, fa, fnum) = doc.footer.default.as_deref().map(scan).unwrap_or((String::new(), 1, None));
        let (num_pos, num_mode, num_align) = if let Some((m, a)) = fnum { (2, m, a) }
            else if let Some((m, a)) = hn { (1, m, a) } else { (0, 0, 1) };
        Ok(format!(
            "{{\"header\":\"{}\",\"footer\":\"{}\",\"header_align\":{},\"footer_align\":{},\"num_pos\":{},\"num_mode\":{},\"num_align\":{},\"title_page\":{},\"even_odd\":{}}}",
            esc(&ht), esc(&ft), ha, fa, num_pos, num_mode, num_align, doc.title_page, doc.even_and_odd_headers
        ))
    });
    match result {
        Some(Ok(json)) => match CString::new(json) {
            Ok(cs) => cs.into_raw(),
            Err(e) => { set_last_error(format!("roe_get_header_footer: {e}")); std::ptr::null_mut() }
        },
        Some(Err(m)) => { set_last_error(format!("roe_get_header_footer: {m}")); std::ptr::null_mut() }
        None => { set_last_error("roe_get_header_footer: 无效句柄"); std::ptr::null_mut() }
    }
}
