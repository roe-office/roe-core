//! Writer 全局查找替换（C-ABI 层）。
//!
//! 背景（Gemini 缺口清单第2条，ROE_Progress_v195.md §2.1）：与
//! char_format.rs 同一分层原则——本文件只负责 FFI 边界（解析
//! handle、C 字符串、越界/范围哨兵值转换、错误码/last_error 编码），
//! 核心替换逻辑全部下沉在 roe_writer::find_replace（编译正则、
//! 单段落替换、文档级遍历），本文件不重新实现任何替换算法。
//!
//! 哨兵值约定（与 Calc 既有哨兵风格一致，v195 已拍板）：
//! - end_para_index == u32::MAX 表示"到文档末尾"；配合
//!   start_para_index == 0 时表示处理全文档，对应
//!   roe_writer::find_replace::replace_text_in_document 的
//!   para_range = None。
//! - 返回值：i32 替换次数，负数表示出错（不用 i32::MIN，避免和
//!   "合法替换 0 次"混淆，与本项目其它返回计数的 C-ABI 函数一致）。
//!   具体错误信息走 set_last_error + roe_last_error_message。
//!
//! 与 roe_replace_range（text_edit.rs）的区别：那个函数是 IME 组合
//! 场景用的区间替换，不支持跨 run；这里是查找替换（regex 全局替换），
//! 语义完全不同，故独立成模块，不塞进 text_edit.rs。

use crate::handle;
use roe_model::writer::BodyElement;
use roe_parser::RoeDocument;
use roe_writer::find_replace::{compile_pattern, replace_text_in_document};

use crate::error::{set_last_error, RoeErrorCode};

/// entry 层包装：校验文档类型、调用 replace_text_in_document，
/// 成功时返回 Ok(替换次数)，用于 with_entry 闭包内。
fn replace_text_in_entry(
    entry: &mut handle::DocumentEntry,
    pattern: &str,
    replacement: &str,
    case_sensitive: bool,
    para_range: Option<(usize, usize)>,
) -> Result<i32, String> {
    let regex = compile_pattern(pattern, case_sensitive)?;

    // 先只读校验文档类型，校验通过后才 clone 快照——与
    // set_char_format_in_entry 同款做法，避免对根本不合法的调用
    // 也白白 clone 一次整份文档。
    {
        let RoeDocument::Writer(_doc) = &entry.doc else {
            return Err("非 Writer 文档".to_string());
        };
    }

    entry.snapshot = Some(entry.doc.clone());

    let RoeDocument::Writer(doc) = &mut entry.doc else {
        unreachable!("上面已确认是 Writer 文档");
    };

    let result = replace_text_in_document(doc, &regex, replacement, para_range);

    if result.total_count > 0 {
        if let Some(prev) = entry.snapshot.take() {
            // 查找替换可能横跨多个不连续段落，没有单一的
            // [range_start, range_end) 可用，退化为用受影响的第一个
            // 段落做上下文标记（撤销快照本身是整份文档快照，
            // EditCursorContext 只是辅助定位，不影响撤销正确性）。
            let first_para = result.affected_para_indices.first().copied().unwrap_or(0);
            let ctx = handle::EditCursorContext::range(first_para, 0, 0);
            entry.push_undo_snapshot(prev, ctx);
        }
    } else {
        // 没有发生任何替换：丢弃刚才存的快照，未真正改动文档，
        // 与 set_char_format_in_entry 失败路径的语义保持一致
        // （区别是这里不算"失败"，只是 0 次匹配，仍返回 Ok(0)）。
        entry.snapshot = None;
    }

    Ok(result.total_count as i32)
}

/// 在文档内做全局查找替换（正则表达式）。
///
/// 参数：
/// - pattern: 正则表达式源串（UTF-8），语法遵循 regex crate。
/// - replacement: 替换文本（UTF-8），可以是空字符串（表示删除匹配到
///   的内容，不留空 run 占位）。
/// - case_sensitive: 是否区分大小写。
/// - start_para_index / end_para_index: 处理范围为 body 下标
///   [start_para_index, end_para_index)。end_para_index == u32::MAX
///   是哨兵值，表示"到文档末尾"；配合 start_para_index == 0 表示
///   处理全文档（对应核心逻辑层 para_range = None）。范围本身按
///   body 下标计算，与 char_format 系列接口的 local_para_index
///   语义对齐：body 里混杂 Table/Section 等非段落元素时，下标依然
///   是 body 数组下标，不是"仅统计 Paragraph 时的第几个"。
///
/// 返回：替换发生的总次数（i32）；负数表示出错，具体错误信息
/// 通过 roe_last_error_message 获取。
///
/// 匹配范围限定在单个段落内，不支持跨段落匹配——这是 Word/
/// LibreOffice Writer 内置查找替换的既有行为，不是本项目的能力
/// 缺口（已在 roe-writer::find_replace 模块文档中说明并经 web
/// 搜索确认）。非 Paragraph 的 BodyElement（Table/Section/Image/
/// Frame/Shape/InlineChart）一律跳过，不视为错误——表格 cell / 
/// 文本框内的替换未实现，属已知范围限制，与字符格式系列接口
/// "仅支持 BODY 直属段落"一致。
#[no_mangle]
pub extern "C" fn roe_replace_text(
    handle: u64,
    pattern: *const std::os::raw::c_char,
    replacement: *const std::os::raw::c_char,
    case_sensitive: bool,
    start_para_index: u32,
    end_para_index: u32,
) -> i32 {
    if pattern.is_null() {
        set_last_error("roe_replace_text: pattern 不能为空指针");
        return RoeErrorCode::NullPointer as i32;
    }
    // Safety: 非空指针，调用方需保证指向以NUL结尾的合法内存，
    // 与项目内其它接受 *const c_char 的 C-ABI 函数同样的调用约定。
    let pattern_str = match unsafe { std::ffi::CStr::from_ptr(pattern) }.to_str() {
        Ok(s) => s,
        Err(_) => {
            set_last_error("roe_replace_text: pattern 不是合法 UTF-8");
            return RoeErrorCode::Utf8Error as i32;
        }
    };

    if replacement.is_null() {
        set_last_error("roe_replace_text: replacement 不能为空指针（如需删除匹配内容，传空字符串而非空指针）");
        return RoeErrorCode::NullPointer as i32;
    }
    let replacement_str = match unsafe { std::ffi::CStr::from_ptr(replacement) }.to_str() {
        Ok(s) => s,
        Err(_) => {
            set_last_error("roe_replace_text: replacement 不是合法 UTF-8");
            return RoeErrorCode::Utf8Error as i32;
        }
    };

    let para_range: Option<(usize, usize)> =
        if start_para_index == 0 && end_para_index == u32::MAX {
            None
        } else {
            Some((start_para_index as usize, end_para_index as usize))
        };

    let result = handle::with_entry(handle, |entry| {
        // para_range 里的 end 需要 clamp 到 doc.body.len()，与
        // roe-writer::find_replace::replace_text_in_document 内部的
        // range_end.min(doc.body.len()) 呼应（这里提前 clamp 是为了
        // 让 replace_text_in_entry 的签名保持 Option<(usize, usize)>
        // 简洁，不需要再往下传 doc 长度）。
        let clamped_range = para_range.map(|(s, e)| {
            let RoeDocument::Writer(doc) = &entry.doc else {
                return (s, e);
            };
            (s, e.min(doc.body.len()))
        });
        replace_text_in_entry(entry, pattern_str, replacement_str, case_sensitive, clamped_range)
    });

    let is_ok = matches!(&result, Some(Ok(n)) if *n > 0);
    if is_ok {
        handle::invalidate_writer_layout_cache(handle);
    }

    match result {
        Some(Ok(n)) => n,
        Some(Err(e)) => {
            set_last_error(format!("roe_replace_text: {e}"));
            RoeErrorCode::ScriptError as i32
        }
        None => {
            set_last_error("roe_replace_text: 无效句柄");
            RoeErrorCode::InvalidHandle as i32
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::roe_last_error_message;
    use roe_model::writer::BodyElement;
    use roe_parser::odf::fodt::FodtParser;
    use std::collections::VecDeque;
    use std::collections::HashMap;
    use std::ffi::CString;
    use crate::handle::WriterOptions;

    /// 与 text_edit.rs::open_writer 同款做法：用一份最小 FODT 样本
    /// 解析出 WriterDocument，插入 handle 表。para_texts 依次作为
    /// 各段落的纯文本内容（每段一个 text:p，不含内联格式，够用即可，
    /// 不需要真实 docx 样本）。
    fn open_writer_with_paragraphs(para_texts: &[&str]) -> u64 {
        let paras_xml: String = para_texts
            .iter()
            .map(|t| format!(r#"<text:p text:style-name="Default">{t}</text:p>"#))
            .collect();
        let fodt = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
    xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
    xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"
    office:version="1.2"
    office:mimetype="application/vnd.oasis.opendocument.text">
  <office:styles>
    <style:style style:name="Default" style:family="paragraph">
      <style:text-properties fo:font-size="12pt"/>
    </style:style>
  </office:styles>
  <office:body>
    <office:text>
      {paras_xml}
    </office:text>
  </office:body>
</office:document>"#
        );
        let doc = FodtParser::parse(&fodt).expect("parse failed");
        handle::insert(handle::DocumentEntry {
            doc: RoeDocument::Writer(doc),
            snapshot: None,
            is_shadow: false,
            events: VecDeque::new(),
            animation: HashMap::new(),
            writer_layout_cache: None,
            writer_font_registry: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            writer_options: WriterOptions::default(),
            impress_font_registry: None,
            writer_parley_font_context: None,
            calc_engine: None,
            pending_comment_hits: Vec::new(),
        })
    }

    fn cstr(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    #[test]
    fn replace_text_normal_path_whole_document() {
        let h = open_writer_with_paragraphs(&["foo one", "foo two"]);
        let pattern = cstr("foo");
        let replacement = cstr("bar");
        let n = roe_replace_text(h, pattern.as_ptr(), replacement.as_ptr(), true, 0, u32::MAX);
        assert_eq!(n, 2);
    }

    #[test]
    fn replace_text_case_insensitive_option() {
        let h = open_writer_with_paragraphs(&["Hello WORLD hello"]);
        let pattern = cstr("hello");
        let replacement = cstr("Hi");
        let n = roe_replace_text(h, pattern.as_ptr(), replacement.as_ptr(), false, 0, u32::MAX);
        assert_eq!(n, 2);
    }

    #[test]
    fn replace_text_invalid_regex_returns_negative_and_sets_last_error() {
        let h = open_writer_with_paragraphs(&["foo"]);
        let pattern = cstr("(unclosed");
        let replacement = cstr("bar");
        let n = roe_replace_text(h, pattern.as_ptr(), replacement.as_ptr(), true, 0, u32::MAX);
        assert!(n < 0);
        let msg_ptr = roe_last_error_message();
        assert!(!msg_ptr.is_null());
    }

    #[test]
    fn replace_text_respects_explicit_para_range() {
        let h = open_writer_with_paragraphs(&["foo one", "foo two"]);
        let pattern = cstr("foo");
        let replacement = cstr("X");
        // 只处理下标 [1, 2)：第二段。
        let n = roe_replace_text(h, pattern.as_ptr(), replacement.as_ptr(), true, 1, 2);
        assert_eq!(n, 1);
        let RoeDocument_ignore = (); let _ = RoeDocument_ignore;
        // 校验第一段确实未被改动。
        crate::handle::with_entry(h, |entry| {
            let RoeDocument::Writer(doc) = &entry.doc else { unreachable!() };
            let Some(BodyElement::Paragraph(p0)) = doc.body.get(0) else { unreachable!() };
            let text: String = p0.runs.iter().map(|r| r.text.as_str()).collect();
            assert_eq!(text, "foo one");
        });
    }

    #[test]
    fn replace_text_invalid_handle_returns_invalid_handle_code() {
        let pattern = cstr("foo");
        let replacement = cstr("bar");
        let n = roe_replace_text(0, pattern.as_ptr(), replacement.as_ptr(), true, 0, u32::MAX);
        assert_eq!(n, RoeErrorCode::InvalidHandle as i32);
    }

    #[test]
    fn replace_text_empty_replacement_is_deletion() {
        let h = open_writer_with_paragraphs(&["Hello, World!"]);
        let pattern = cstr(", World");
        let replacement = cstr("");
        let n = roe_replace_text(h, pattern.as_ptr(), replacement.as_ptr(), true, 0, u32::MAX);
        assert_eq!(n, 1);
        crate::handle::with_entry(h, |entry| {
            let RoeDocument::Writer(doc) = &entry.doc else { unreachable!() };
            let Some(BodyElement::Paragraph(p0)) = doc.body.get(0) else { unreachable!() };
            let text: String = p0.runs.iter().map(|r| r.text.as_str()).collect();
            assert_eq!(text, "Hello!");
        });
    }
}


// ───────── v245: 查找 / 带选项的全部替换 ─────────
fn find_build_pattern(p: &str, whole_word: bool, use_regex: bool) -> String {
    let mut s = if use_regex {
        p.to_string()
    } else {
        let mut o = String::new();
        for ch in p.chars() {
            if "\\.+*?()|[]{}^$#&-~".contains(ch) { o.push('\\'); }
            o.push(ch);
        }
        o
    };
    if whole_word { s = format!(r"\b(?:{s})\b"); }
    s
}

/// 查找。返回 JSON：{"found":false,"total":0} 或
/// {"found":true,"para":N,"start":N,"end":N,"index":k(1起),"total":N}。
/// forward=true 取 (from_para,from_offset) 之后（含）的第一个匹配，没有则回到文首；
/// forward=false 取之前的最后一个匹配，没有则回到文尾。偏移为段内字节偏移（与格式接口一致）。
#[no_mangle]
pub extern "C" fn roe_find_text(
    handle: u64,
    pattern: *const std::os::raw::c_char,
    case_sensitive: bool,
    whole_word: bool,
    use_regex: bool,
    from_para: u32,
    from_offset: u32,
    forward: bool,
) -> *mut std::os::raw::c_char {
    if pattern.is_null() {
        set_last_error("roe_find_text: pattern 不能为空指针");
        return std::ptr::null_mut();
    }
    let pat = match unsafe { std::ffi::CStr::from_ptr(pattern) }.to_str() {
        Ok(s) => s.to_string(),
        Err(_) => { set_last_error("roe_find_text: pattern 不是合法 UTF-8"); return std::ptr::null_mut(); }
    };
    if pat.is_empty() {
        set_last_error("roe_find_text: 查找内容为空");
        return std::ptr::null_mut();
    }
    let re = match roe_writer::find_replace::compile_pattern(&find_build_pattern(&pat, whole_word, use_regex), case_sensitive) {
        Ok(r) => r,
        Err(e) => { set_last_error(e); return std::ptr::null_mut(); }
    };
    let result = handle::with_entry(handle, |entry| -> Result<String, String> {
        let RoeDocument::Writer(doc) = &entry.doc else { return Err("非 Writer 文档".to_string()); };
        let mut ms: Vec<(usize, usize, usize)> = Vec::new();
        for (i, el) in doc.body.iter().enumerate() {
            if let roe_model::writer::BodyElement::Paragraph(p) = el {
                let full: String = p.runs.iter().map(|r| r.text.as_str()).collect();
                for m in re.find_iter(&full) {
                    if m.end() > m.start() { ms.push((i, m.start(), m.end())); }
                }
            }
        }
        if ms.is_empty() { return Ok("{\"found\":false,\"total\":0}".to_string()); }
        let key = (from_para as usize, from_offset as usize);
        let idx = if forward {
            ms.iter().position(|m| (m.0, m.1) >= key).unwrap_or(0)
        } else {
            ms.iter().rposition(|m| (m.0, m.1) < key).unwrap_or(ms.len() - 1)
        };
        let m = ms[idx];
        Ok(format!("{{\"found\":true,\"para\":{},\"start\":{},\"end\":{},\"index\":{},\"total\":{}}}", m.0, m.1, m.2, idx + 1, ms.len()))
    });
    match result {
        Some(Ok(j)) => match std::ffi::CString::new(j) {
            Ok(c) => c.into_raw(),
            Err(_) => std::ptr::null_mut(),
        },
        Some(Err(m)) => { set_last_error(format!("roe_find_text: {m}")); std::ptr::null_mut() }
        None => { set_last_error("roe_find_text: 无效句柄"); std::ptr::null_mut() }
    }
}

/// 带选项的全部替换。返回替换次数（>=0），失败返回负的错误码
#[no_mangle]
pub extern "C" fn roe_replace_text_ex(
    handle: u64,
    pattern: *const std::os::raw::c_char,
    replacement: *const std::os::raw::c_char,
    case_sensitive: bool,
    whole_word: bool,
    use_regex: bool,
) -> i32 {
    if pattern.is_null() || replacement.is_null() {
        set_last_error("roe_replace_text_ex: 空指针");
        return RoeErrorCode::NullPointer as i32;
    }
    let pat = match unsafe { std::ffi::CStr::from_ptr(pattern) }.to_str() {
        Ok(s) => s.to_string(),
        Err(_) => { set_last_error("roe_replace_text_ex: pattern 不是合法 UTF-8"); return RoeErrorCode::Utf8Error as i32; }
    };
    if pat.is_empty() {
        set_last_error("roe_replace_text_ex: 查找内容为空");
        return RoeErrorCode::ScriptError as i32;
    }
    let built = match std::ffi::CString::new(find_build_pattern(&pat, whole_word, use_regex)) {
        Ok(c) => c,
        Err(_) => { set_last_error("roe_replace_text_ex: pattern 含 NUL"); return RoeErrorCode::ScriptError as i32; }
    };
    roe_replace_text(handle, built.as_ptr(), replacement, case_sensitive, 0, u32::MAX)
}
