//! Writer 全局查找替换（核心逻辑层）。
//!
//! 背景（Gemini 缺口清单第2条，ROE_Progress_v194.md §2.2）：与
//! char_format.rs 同一分层原则——本模块只操作 roe_model::writer 的
//! WriterDocument/Paragraph/Run，不涉及任何 FFI 概念（handle/
//! RoeLocation/CString），供 roe-api（C-ABI）与 roe-script（Rhai）
//! 两层共同复用。
//!
//! 设计要点（讨论收敛，见交接记录）：
//! - 支持正则表达式匹配（regex crate），大小写是否区分由调用方传参
//!   控制（对应 regex::RegexBuilder::case_insensitive）。
//! - 匹配范围限定在单个段落内，不支持跨段落匹配——这与 Word/LibreOffice
//!   Writer 内置查找替换的既有行为一致（两者内部都是逐段落搜索，
//!   一个匹配不能跨越段落边界，网上已有大量讨论证实这是行业惯例
//!   而非本项目的能力缺口）。
//! - 替换后的新文本格式：沿用匹配范围内第一个字符所在 run 的格式
//!   （不做跨 run 的逐字符格式插值，同样是 Word 的实际行为）。
//! - 作用范围：默认整个文档 body 中所有 Paragraph（跳过 Table/
//!   Section/Image/Frame/Shape/InlineChart，与 char_format 系列接口
//!   "仅支持 container_type=BODY 直属段落"的既有范围限制一致，
//!   表格 cell / 文本框内的替换未实现，属已知范围限制）；调用方可选
//!   传入 [start_para_index, end_para_index) 缩小处理范围，供 AI
//!   脚本做"只处理某一节"这类精细操作，前台目前只会传全文档范围。
//! - 返回值：核心逻辑层返回详细结果（`ReplaceTextResult`，含总次数
//!   + 每次替换发生的段落索引），供 Rhai 层编码为 JSON；C-ABI 层
//!   只取 `total_count` 暴露为整数，与"C-ABI 面向前台简单调用、Rhai
//!   面向 AI 需要更多上下文"的既有分工原则一致（同 char_format.rs
//!   的 "__MIXED__" vs "mixed" 分层惯例）。

use regex::{Regex, RegexBuilder};
use roe_model::writer::{BodyElement, Paragraph, Run, WriterDocument};

/// 一次 replace_text 调用的详细结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReplaceTextResult {
    /// 全部替换发生的次数（跨所有处理到的段落累加）。
    pub total_count: usize,
    /// 每个发生过至少一次替换的段落索引（按处理顺序去重排列，
    /// 不重复列出同一段落，方便调用方定位"改了哪些段落"而不需要
    /// 关心"某段落改了几次"这种更细粒度的信息）。
    pub affected_para_indices: Vec<usize>,
}

/// 编译查找用的正则表达式。
///
/// pattern 是调用方给出的正则表达式源串；case_sensitive=false 时
/// 等价于在正则前加 `(?i)`（用 RegexBuilder 显式设置，不用字符串
/// 拼接方式，避免调用方 pattern 本身含有 `(?i)` 之类标记时产生歧义）。
///
/// 正则语法错误时返回 Err，错误信息透传 regex crate 原始报错文案
/// （调用方通常需要看到具体是哪里写错了正则，不适合在这里改写成
/// project 自己的措辞）。
pub fn compile_pattern(pattern: &str, case_sensitive: bool) -> Result<Regex, String> {
    RegexBuilder::new(pattern)
        .case_insensitive(!case_sensitive)
        .build()
        .map_err(|e| format!("正则表达式编译失败: {e}"))
}

/// 在单个段落内做全局替换（段落内所有不重叠匹配都替换，regex crate
/// 的 `replace_all` 语义：从左到右扫描，已消费的字节不会被后续匹配
/// 再次覆盖）。
///
/// 返回：(是否发生过替换, 替换次数)。
/// 不做任何越界/非 BODY 校验——那是调用方（roe-api/roe-script）的
/// 职责，本函数只管"给一个 Paragraph，按 regex 替换，返回新 runs"。
pub fn replace_text_in_paragraph(
    para: &mut Paragraph,
    regex: &Regex,
    replacement: &str,
) -> usize {
    // 先把整段落的 runs 拼接成一份 full_text，同时记录每个 run 在
    // full_text 中的起止字节偏移，供后续把匹配命中的字节区间映射回
    // 具体某个 run（与 char_format.rs::set_char_format_in_range 的
    // "按累加字节长度定位 run" 思路一致，唯一区别是这里需要一次性
    // 拿到完整拼接文本用于 regex 匹配，char_format 那边不需要拼接，
    // 因为它的调用方直接给的就是 [range_start, range_end) 字节区间）。
    let full_text: String = para.runs.iter().map(|r| r.text.as_str()).collect();

    let matches: Vec<regex::Match> = regex.find_iter(&full_text).collect();
    if matches.is_empty() {
        return 0;
    }

    // run 起止字节偏移表：run_bounds[i] = (run_i的起始偏移, run_i的结束偏移)。
    let mut run_bounds: Vec<(usize, usize)> = Vec::with_capacity(para.runs.len());
    {
        let mut consumed = 0usize;
        for run in &para.runs {
            let len = run.text.len();
            run_bounds.push((consumed, consumed + len));
            consumed += len;
        }
    }

    // 找到字节偏移 pos 落在哪个 run 里，返回 run 下标（用于取格式模板）。
    // pos 落在两个 run 交界处（等于某个 run 的 end）时，归属到后一个
    // run——即匹配区间的"格式模板 run"取自区间起点覆盖到的第一个
    // run，与文件头注释"沿用匹配范围内第一个字符所在 run 的格式"
    // 一致（当匹配为空区间时不会走到这里，find_iter 不产生零宽匹配
    // 之外的空 match，正则本身允许零宽匹配，这种边界情形格式模板
    // 退化为取前一个 run，见下方 template_run_index 的 saturating 处理）。
    let find_run_index = |pos: usize| -> usize {
        for (i, (start, end)) in run_bounds.iter().enumerate() {
            if pos < *end || (pos == *end && *end == full_text.len()) {
                if pos >= *start {
                    return i;
                }
            }
        }
        run_bounds.len().saturating_sub(1)
    };

    // 按字节偏移重建 runs：把 full_text 切成 [未匹配片段, 匹配替换片段, 未匹配片段, ...]
    // 交替序列，未匹配片段保留原有的多 run 结构（直接复用 set_char_format_in_range
    // 同款的"边界 run 拆分"逻辑会更复杂，这里简化为：未匹配区间整体按原 run 切分
    // 保留，替换区间整体归并为一个新 run（格式取自区间起点所在 run 的 clone）。
    let mut new_runs: Vec<Run> = Vec::new();
    let mut cursor = 0usize;

    let push_original_slice = |new_runs: &mut Vec<Run>, run_bounds: &[(usize, usize)], para_runs: &[Run], slice_start: usize, slice_end: usize| {
        if slice_start >= slice_end {
            return;
        }
        for (i, (rs, re)) in run_bounds.iter().enumerate() {
            let lo = slice_start.max(*rs);
            let hi = slice_end.min(*re);
            if lo >= hi {
                continue;
            }
            let local_lo = lo - rs;
            let local_hi = hi - rs;
            let mut piece = para_runs[i].clone();
            piece.text = para_runs[i].text[local_lo..local_hi].to_string();
            new_runs.push(piece);
        }
    };

    let original_runs = para.runs.clone();

    for m in &matches {
        let (match_start, match_end) = (m.start(), m.end());

        // 匹配之前的未变化片段，原样保留（可能跨多个 run）。
        push_original_slice(&mut new_runs, &run_bounds, &original_runs, cursor, match_start);

        // 替换片段：格式取自匹配起点所在 run 的 clone（若 replacement
        // 为空字符串，即"纯删除"场景，不产生新 run，与 Word 的删除
        // 行为一致，不留一个空文本 run 占位）。
        if !replacement.is_empty() {
            let template_index = find_run_index(match_start);
            let mut new_run = original_runs[template_index].clone();
            new_run.text = replacement.to_string();
            new_runs.push(new_run);
        }

        cursor = match_end;
    }

    // 最后一个匹配之后的剩余片段。
    push_original_slice(&mut new_runs, &run_bounds, &original_runs, cursor, full_text.len());

    para.runs = new_runs;
    matches.len()
}

/// 在整个文档（或指定段落范围）内做全局替换。
///
/// para_range: None 表示处理 doc.body 中所有 Paragraph；
/// Some((start, end)) 表示只处理 body 下标 [start, end) 范围内、且
/// 恰好是 BodyElement::Paragraph 的元素（范围本身按 body 下标计算，
/// 与 char_format 系列接口的 local_para_index 语义对齐：body 里
/// 混杂 Table/Section 等非段落元素时，下标依然是 body 数组下标，
/// 不是"仅统计 Paragraph 时的第几个"，调用方需自行注意）。
///
/// 非 Paragraph 的 BodyElement（Table/Section/Image/Frame/Shape/
/// InlineChart）一律跳过，不视为错误——这是"仅支持 BODY 直属段落"
/// 既有范围限制的自然结果，不是需要报错提示的异常情况。
pub fn replace_text_in_document(
    doc: &mut WriterDocument,
    regex: &Regex,
    replacement: &str,
    para_range: Option<(usize, usize)>,
) -> ReplaceTextResult {
    let (range_start, range_end) = para_range.unwrap_or((0, doc.body.len()));

    let mut result = ReplaceTextResult::default();

    for idx in range_start..range_end.min(doc.body.len()) {
        let BodyElement::Paragraph(para) = &mut doc.body[idx] else {
            continue;
        };
        let count = replace_text_in_paragraph(para, regex, replacement);
        if count > 0 {
            result.total_count += count;
            result.affected_para_indices.push(idx);
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use roe_model::writer::Paragraph;

    fn run(text: &str) -> Run {
        Run { text: text.to_string(), ..Default::default() }
    }

    fn run_bold(text: &str) -> Run {
        Run { text: text.to_string(), bold: Some(true), ..Default::default() }
    }

    fn para_texts(para: &Paragraph) -> Vec<String> {
        para.runs.iter().map(|r| r.text.clone()).collect()
    }

    fn joined(para: &Paragraph) -> String {
        para.runs.iter().map(|r| r.text.as_str()).collect()
    }

    #[test]
    fn replace_within_single_run() {
        let mut para = Paragraph { runs: vec![run("Hello World")], ..Default::default() };
        let re = compile_pattern("World", true).unwrap();
        let count = replace_text_in_paragraph(&mut para, &re, "Rust");
        assert_eq!(count, 1);
        assert_eq!(joined(&para), "Hello Rust");
    }

    #[test]
    fn replace_all_multiple_matches_in_one_paragraph() {
        let mut para = Paragraph { runs: vec![run("foo bar foo baz foo")], ..Default::default() };
        let re = compile_pattern("foo", true).unwrap();
        let count = replace_text_in_paragraph(&mut para, &re, "X");
        assert_eq!(count, 3);
        assert_eq!(joined(&para), "X bar X baz X");
    }

    #[test]
    fn replace_spans_two_runs_keeps_template_format_from_match_start_run() {
        // "He" + "llo World" 两个 run 拼接为 "Hello World"，匹配
        // "llo Wo" 从字节偏移2开始（"He"长度为2），即匹配到的第一个
        // 字符 'l' 落在第二个 run（"llo World"，不加粗）内部，不是
        // 落在两个 run 的交界处——用这种"起点严格落在某个 run 内部"
        // 的例子，避免"交界处到底算前一个还是后一个 run"这种边界
        // 情形的歧义，测试意图更清晰。
        let mut para = Paragraph {
            runs: vec![run_bold("He"), run("llo World")],
            ..Default::default()
        };
        let re = compile_pattern("llo Wo", true).unwrap();
        let count = replace_text_in_paragraph(&mut para, &re, "XX");
        assert_eq!(count, 1);
        assert_eq!(joined(&para), "HeXXrld");
        // 替换片段应沿用匹配起点所在 run（第二个 run，不加粗）的格式，
        // 而不是第一个 run（加粗）的格式——验证"格式模板取自匹配区间
        // 第一个字符所在 run"，不是"取自段落里第一个 run"。
        let replaced_run = para.runs.iter().find(|r| r.text == "XX").unwrap();
        assert_eq!(replaced_run.bold, None);
    }

    #[test]
    fn replace_match_starting_exactly_at_run_boundary_uses_second_run_format() {
        // "Hel" + "lo World"：匹配 "lo Wo" 恰好从 run 边界（字节偏移3）
        // 开始，即匹配到的第一个字符属于第二个 run——专门验证这个
        // 边界情形按"第一个字符归属"而非"前一个 run 兜底"处理。
        let mut para = Paragraph {
            runs: vec![run_bold("Hel"), run("lo World")],
            ..Default::default()
        };
        let re = compile_pattern("lo Wo", true).unwrap();
        let count = replace_text_in_paragraph(&mut para, &re, "XX");
        assert_eq!(count, 1);
        assert_eq!(joined(&para), "HelXXrld");
        let replaced_run = para.runs.iter().find(|r| r.text == "XX").unwrap();
        assert_eq!(replaced_run.bold, None);
    }

    #[test]
    fn replace_with_empty_string_is_pure_deletion_no_empty_run_left() {
        let mut para = Paragraph { runs: vec![run("Hello, World!")], ..Default::default() };
        let re = compile_pattern(", World", true).unwrap();
        let count = replace_text_in_paragraph(&mut para, &re, "");
        assert_eq!(count, 1);
        assert_eq!(joined(&para), "Hello!");
        assert!(para.runs.iter().all(|r| !r.text.is_empty()));
    }

    #[test]
    fn no_match_returns_zero_and_leaves_runs_untouched() {
        let mut para = Paragraph { runs: vec![run("Hello World")], ..Default::default() };
        let re = compile_pattern("xyz", true).unwrap();
        let count = replace_text_in_paragraph(&mut para, &re, "abc");
        assert_eq!(count, 0);
        assert_eq!(para_texts(&para), vec!["Hello World".to_string()]);
    }

    #[test]
    fn case_sensitive_false_matches_regardless_of_case() {
        let mut para = Paragraph { runs: vec![run("Hello WORLD hello")], ..Default::default() };
        let re = compile_pattern("hello", false).unwrap();
        let count = replace_text_in_paragraph(&mut para, &re, "Hi");
        assert_eq!(count, 2);
        assert_eq!(joined(&para), "Hi WORLD Hi");
    }

    #[test]
    fn case_sensitive_true_does_not_match_different_case() {
        let mut para = Paragraph { runs: vec![run("Hello hello")], ..Default::default() };
        let re = compile_pattern("hello", true).unwrap();
        let count = replace_text_in_paragraph(&mut para, &re, "Hi");
        assert_eq!(count, 1);
        assert_eq!(joined(&para), "Hello Hi");
    }

    #[test]
    fn regex_pattern_support_digit_class() {
        let mut para = Paragraph { runs: vec![run("order 42 and order 7")], ..Default::default() };
        let re = compile_pattern(r"\d+", true).unwrap();
        let count = replace_text_in_paragraph(&mut para, &re, "N");
        assert_eq!(count, 2);
        assert_eq!(joined(&para), "order N and order N");
    }

    #[test]
    fn invalid_regex_pattern_returns_error() {
        let result = compile_pattern("(unclosed", true);
        assert!(result.is_err());
    }

    #[test]
    fn replace_in_document_default_range_covers_all_paragraphs() {
        let mut doc = WriterDocument::default();
        doc.body.push(BodyElement::Paragraph(Paragraph { runs: vec![run("foo one")], ..Default::default() }));
        doc.body.push(BodyElement::Paragraph(Paragraph { runs: vec![run("foo two")], ..Default::default() }));
        let re = compile_pattern("foo", true).unwrap();
        let result = replace_text_in_document(&mut doc, &re, "bar", None);
        assert_eq!(result.total_count, 2);
        assert_eq!(result.affected_para_indices, vec![0, 1]);
    }

    #[test]
    fn replace_in_document_respects_para_range() {
        let mut doc = WriterDocument::default();
        doc.body.push(BodyElement::Paragraph(Paragraph { runs: vec![run("foo one")], ..Default::default() }));
        doc.body.push(BodyElement::Paragraph(Paragraph { runs: vec![run("foo two")], ..Default::default() }));
        let re = compile_pattern("foo", true).unwrap();
        // 只处理下标 [1, 2) —— 第二段。
        let result = replace_text_in_document(&mut doc, &re, "bar", Some((1, 2)));
        assert_eq!(result.total_count, 1);
        assert_eq!(result.affected_para_indices, vec![1]);
        // 第一段应保持未变。
        let BodyElement::Paragraph(p0) = &doc.body[0] else { panic!() };
        assert_eq!(joined(p0), "foo one");
    }

    #[test]
    fn replace_in_document_skips_non_paragraph_body_elements() {
        use roe_model::writer::Section;
        let mut doc = WriterDocument::default();
        doc.body.push(BodyElement::Paragraph(Paragraph { runs: vec![run("foo")], ..Default::default() }));
        // 用 Section 代表"任意非 Paragraph 的 BodyElement"，验证跳过逻辑
        // 与具体元素种类无关（Table/Image/Frame/Shape/InlineChart 同理，
        // 不需要逐一构造，Section 恰好 derive 了 Default，构造成本最低）。
        doc.body.push(BodyElement::Section(Section::default()));
        doc.body.push(BodyElement::Paragraph(Paragraph { runs: vec![run("foo again")], ..Default::default() }));
        let re = compile_pattern("foo", true).unwrap();
        let result = replace_text_in_document(&mut doc, &re, "X", None);
        // 应只处理下标0和2两个段落，Section（下标1）被跳过不报错。
        assert_eq!(result.total_count, 2);
        assert_eq!(result.affected_para_indices, vec![0, 2]);
    }
}
