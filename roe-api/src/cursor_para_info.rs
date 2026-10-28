//! roe_get_paragraph_info —— 查询段落文字长度与文档总段落数。
//!
//! 用途：供前端实现"光标在段首/段尾按左右方向键跨段落跳转"（final文档
//! 五.3提到的、当时未展开的交互细节，本次补上）。roe_cursor_move_inline
//! 在到达当前行/段边界时会原地不动（不自动跨段），前端检测到"位置未
//! 变化"后，需要知道：
//! - 跳到上一段时，上一段有多长（跳到其末尾，而不是猜一个offset）
//! - 跳到下一段时，是否已经是最后一段（避免越界）
//! 这两个信息由本接口一次性提供，避免前端分两次查询。
//!
//! 与 cursor_inline.rs::move_inline_in_entry 内部累加 frame_len 的逻辑
//! 不同：那里算的是"frame内文字长度"（服务于行内定位），这里算的是
//! "整个段落的文字长度"（段落内所有 Run.text 长度之和），语义更简单，
//! 不涉及 frame/行的排版结构，只读 roe_model::writer::Paragraph.runs。

use std::ffi::CString;
use std::os::raw::c_char;

use roe_model::writer::BodyElement;
use roe_parser::RoeDocument;

use crate::cursor::RoeLocation;
use crate::error::set_last_error;
use crate::handle;

/// 查询指定段落的文字字节长度 + 文档正文总段落数。
///
/// 参数：
/// - handle：文档句柄
/// - container_type/container_id/local_para_index/char_offset：拆解后的
///   RoeLocation（char_offset 本接口不使用，仅为与其他 cursor 系列接口
///   参数形式保持一致，便于前端调用风格统一）
///
/// 返回：JSON 字符串 `{"para_len":N,"total_para_count":N}`
/// - para_len：local_para_index 对应段落的文字字节长度（即该段落所有
///   Run.text 的字节长度之和，也就是该段落合法的最大 char_offset 值）
/// - total_para_count：doc.body 中 BodyElement::Paragraph 变体的总数
///   （注意：body 里可能混有 Table/Image 等非段落元素，这里只统计
///   段落数量，与 local_para_index 的编号语义保持一致——因为
///   move_inline_in_entry 里 body.get(para_index) 是直接按 body 下标
///   取的，不是"第N个段落"，这里为了与现有语义严格对齐，同样直接用
///   body 下标总数，不做"只数Paragraph变体"的过滤。见下方实现注释。
///
/// 失败返回 null，调用 roe_last_error_message 获取详情。
/// 调用方必须用 roe_free_string 释放返回值。
#[no_mangle]
pub extern "C" fn roe_get_paragraph_info(
    handle: u64,
    container_type: u32,
    container_id: u32,
    local_para_index: u32,
    char_offset: u32,
) -> *mut c_char {
    let loc = RoeLocation { container_type, container_id, local_para_index, char_offset };

    if !loc.is_body() {
        set_last_error("roe_get_paragraph_info: 本次仅支持 container_type=BODY（表格cell/文本框预留未实现）");
        return std::ptr::null_mut();
    }

    if !handle::ensure_writer_layout(handle) {
        set_last_error("roe_get_paragraph_info: 无效句柄或非 Writer 文档");
        return std::ptr::null_mut();
    }

    let (para_index, _char_offset) = loc.as_legacy_body();

    let result = handle::with_entry(handle, |entry| {
        get_paragraph_info_in_entry(entry, para_index)
    });

    match result {
        None => {
            set_last_error("roe_get_paragraph_info: 无效句柄");
            std::ptr::null_mut()
        }
        Some(Err(msg)) => {
            set_last_error(format!("roe_get_paragraph_info: {msg}"));
            std::ptr::null_mut()
        }
        Some(Ok(json)) => match CString::new(json) {
            Ok(cs) => cs.into_raw(),
            Err(e) => {
                set_last_error(format!("roe_get_paragraph_info: {e}"));
                std::ptr::null_mut()
            }
        },
    }
}

/// 核心逻辑：
/// - para_len = doc.body[para_index] 对应段落所有 Run.text 的字节长度之和
///   （若该下标不是 Paragraph 变体，返回 Err，因为跨段跳转场景下
///   local_para_index 语义上应当总是指向段落——与 move_inline_in_entry
///   的 body.get(para_index) 用法严格对齐，见文件顶部注释）
/// - total_para_count = doc.body.len()（直接用 body 总长度，不单独过滤
///   Paragraph 变体，理由同上：与现有 cursor_inline.rs 的下标语义对齐，
///   避免"body下标"和"仅统计段落的序号"两套编号体系不一致导致新的bug）
fn get_paragraph_info_in_entry(
    entry: &mut handle::DocumentEntry,
    para_index: usize,
) -> Result<String, String> {
    let RoeDocument::Writer(doc) = &entry.doc else {
        return Err("非 Writer 文档".to_string());
    };

    let para = match doc.body.get(para_index) {
        Some(BodyElement::Paragraph(p)) => p,
        _ => return Err(format!("para_index={para_index} 越界或不是段落")),
    };

    let para_len: usize = para.runs.iter().map(|r| r.text.len()).sum();
    let total_para_count = doc.body.len();
    let is_list = para.list_info.is_some();

    let jn = |v: Option<f32>| v.map(|x| x.to_string()).unwrap_or_else(|| "null".to_string());
    let jb = |v: Option<bool>| v.map(|x| x.to_string()).unwrap_or_else(|| "null".to_string());
    Ok(format!(
        "{{\"para_len\":{},\"total_para_count\":{},\"is_list\":{},\"line_height\":{},\"line_abs_pt\":{},\"line_exact\":{},\"space_before\":{},\"space_after\":{}}}",
        para_len, total_para_count, is_list,
        jn(para.line_height), jn(para.line_abs_pt), jb(para.line_exact), jn(para.space_before), jn(para.space_after)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handle::WriterOptions;
    use roe_parser::odf::fodt::FodtParser;
    use std::collections::{HashMap, VecDeque};

    const SAMPLE_FODT: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document
    xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
    xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
    xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"
    office:mimetype="application/vnd.oasis.opendocument.text">
  <office:styles>
    <style:style style:name="Default" style:family="paragraph">
      <style:text-properties fo:font-size="12pt"/>
    </style:style>
  </office:styles>
  <office:body>
    <office:text>
      <text:p text:style-name="Default">Hello World</text:p>
      <text:p text:style-name="Default">第二段。</text:p>
    </office:text>
  </office:body>
</office:document>"#;

    fn open_writer() -> u64 {
        let doc = FodtParser::parse(SAMPLE_FODT).expect("parse failed");
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

    fn parse_field(json: &str, key: &str) -> usize {
        let pat = format!("\"{}\":", key);
        let start = json.find(&pat).unwrap_or_else(|| panic!("json 应含 {key} 字段: {json}")) + pat.len();
        let rest = &json[start..];
        let end = rest.find(|c: char| c == ',' || c == '}').unwrap_or(rest.len());
        rest[..end].trim().parse().unwrap_or_else(|_| panic!("{key} 应为数字: {json}"))
    }

    #[test]
    fn test_first_para_len_matches_hello_world() {
        let h = open_writer();
        let ptr = roe_get_paragraph_info(h, 0, 0, 0, 0);
        assert!(!ptr.is_null(), "查询第0段应该成功");
        let s = unsafe { CString::from_raw(ptr) }.into_string().unwrap();
        let para_len = parse_field(&s, "para_len");
        assert_eq!(para_len, 11, "\"Hello World\" 长度应为11，实际 {para_len}");
        handle::remove(h);
    }

    #[test]
    fn test_total_para_count_is_two() {
        let h = open_writer();
        let ptr = roe_get_paragraph_info(h, 0, 0, 0, 0);
        assert!(!ptr.is_null());
        let s = unsafe { CString::from_raw(ptr) }.into_string().unwrap();
        let total = parse_field(&s, "total_para_count");
        assert_eq!(total, 2, "样例文档共2段，实际 {total}");
        handle::remove(h);
    }

    #[test]
    fn test_second_para_len_matches_chinese_text() {
        let h = open_writer();
        // "第二段。" 3个汉字+1个句号，UTF-8下每个汉字3字节，句号（中文全角）3字节，共12字节
        let ptr = roe_get_paragraph_info(h, 0, 0, 1, 0);
        assert!(!ptr.is_null(), "查询第1段应该成功");
        let s = unsafe { CString::from_raw(ptr) }.into_string().unwrap();
        let para_len = parse_field(&s, "para_len");
        assert_eq!(para_len, 12, "\"第二段。\" UTF-8字节长度应为12，实际 {para_len}");
        handle::remove(h);
    }

    #[test]
    fn test_out_of_range_para_index_rejected() {
        let h = open_writer();
        let ptr = roe_get_paragraph_info(h, 0, 0, 999, 0);
        assert!(ptr.is_null(), "越界 para_index 应该返回 null");
        handle::remove(h);
    }

    #[test]
    fn test_non_body_container_type_rejected() {
        let h = open_writer();
        let ptr = roe_get_paragraph_info(h, 1, 0, 0, 0);
        assert!(ptr.is_null(), "非 BODY container_type 应该返回 null");
        handle::remove(h);
    }

    #[test]
    fn test_invalid_handle_returns_null() {
        let ptr = roe_get_paragraph_info(999_999, 0, 0, 0, 0);
        assert!(ptr.is_null(), "无效句柄应该返回 null");
    }
}
