//! v274: 段落排序

use crate::error::{set_last_error, RoeErrorCode};
use crate::handle;
use roe_model::writer::BodyElement;
use roe_parser::RoeDocument;

fn sort_text_key(s: &str) -> String {
    use pinyin::ToPinyin;
    let mut out = String::new();
    for c in s.trim().chars() {
        if c.is_ascii() {
            out.push(c.to_ascii_lowercase());
        } else if let Some(p) = c.to_pinyin() {
            out.push_str(p.plain());
            out.push(c);
        } else {
            out.push(c);
        }
    }
    out
}

fn sort_first_number(s: &str) -> Option<f64> {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_ascii_digit() {
            let neg = i > 0 && chars[i - 1] == '-';
            let mut t = String::new();
            let mut dot = false;
            while i < chars.len() && (chars[i].is_ascii_digit() || (chars[i] == '.' && !dot && i + 1 < chars.len() && chars[i + 1].is_ascii_digit())) {
                if chars[i] == '.' { dot = true; }
                t.push(chars[i]);
                i += 1;
            }
            return t.parse::<f64>().ok().map(|v| if neg { -v } else { v });
        }
        i += 1;
    }
    None
}

/// v274: 段落排序。start..=end 为 body 下标，须全是普通段落。
/// kind 0=文字（拼音/字母，忽略大小写） 1=数字；descending!=0 降序。带一次撤销快照。
#[no_mangle]
pub extern "C" fn roe_sort_paragraphs(handle: u64, start: u32, end: u32, kind: u32, descending: u32) -> i32 {
    if kind > 1 { set_last_error("roe_sort_paragraphs: kind 非法（合法值 0-1）"); return RoeErrorCode::ScriptError as i32; }
    if end <= start { set_last_error("roe_sort_paragraphs: 请选择多个段落"); return RoeErrorCode::ScriptError as i32; }
    let (s, e) = (start as usize, end as usize);
    let result = handle::with_entry(handle, |entry| -> Result<(), String> {
        {
            let RoeDocument::Writer(doc) = &entry.doc else { return Err("非 Writer 文档".to_string()); };
            if e >= doc.body.len() { return Err(format!("范围越界：end={e}")); }
            for i in s..=e {
                match &doc.body[i] {
                    BodyElement::Paragraph(p) => {
                        if p.runs.iter().any(|r| r.footnote_ref.is_some() || r.endnote_ref.is_some()) {
                            return Err("选区含脚注/尾注引用，暂不支持排序".to_string());
                        }
                    }
                    _ => return Err("选区含表格/图片/分节符等非普通段落，暂不支持排序".to_string()),
                }
            }
        }
        let prev = entry.doc.clone();
        let RoeDocument::Writer(doc) = &mut entry.doc else { unreachable!() };
        let items: Vec<BodyElement> = doc.body.drain(s..=e).collect();
        let mut keyed: Vec<(String, Option<f64>, BodyElement)> = items.into_iter().map(|el| {
            let text = match &el {
                BodyElement::Paragraph(p) => p.runs.iter().map(|r| r.text.as_str()).collect::<String>(),
                _ => String::new(),
            };
            (sort_text_key(&text), sort_first_number(&text), el)
        }).collect();
        let desc = descending != 0;
        keyed.sort_by(|a, b| {
            use std::cmp::Ordering;
            let o = if kind == 1 {
                match (a.1, b.1) {
                    (None, None) => a.0.cmp(&b.0),
                    (None, Some(_)) => Ordering::Less,
                    (Some(_), None) => Ordering::Greater,
                    (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal).then_with(|| a.0.cmp(&b.0)),
                }
            } else {
                a.0.cmp(&b.0)
            };
            if desc { o.reverse() } else { o }
        });
        for (i, (_, _, el)) in keyed.into_iter().enumerate() {
            doc.body.insert(s + i, el);
        }
        entry.push_undo_snapshot(prev, handle::EditCursorContext::at(s, 0));
        Ok(())
    });
    let ok = matches!(&result, Some(Ok(())));
    if ok { handle::invalidate_writer_layout_cache(handle); }
    match result {
        Some(Ok(())) => RoeErrorCode::Ok as i32,
        Some(Err(m)) => { set_last_error(format!("roe_sort_paragraphs: {m}")); RoeErrorCode::ScriptError as i32 }
        None => { set_last_error("roe_sort_paragraphs: 无效句柄"); RoeErrorCode::InvalidHandle as i32 }
    }
}
