//! 字符格式跨 run 范围操作（核心逻辑层）。
//!
//! 背景（下沉自 roe-api/src/char_format.rs，原 WROOT-CHARFMT-01 起点，
//! v191/v192 各自续做 bold → italic/underline/font_size/font_family/
//! underline_style）：这套"跨 run 拆分 + patch 应用"以及"跨 run 范围
//! 查询 + 混合值判断"逻辑，原先直接写在 roe-api（C-ABI层）里，但它
//! 本身不依赖任何 FFI 概念（不涉及 handle、RoeLocation、CString等），
//! 只操作 roe_model::writer::Paragraph/Run，属于纯业务逻辑，理应被
//! roe-api（C-ABI）与 roe-script（Rhai）两层共同复用，因此下沉到
//! roe-writer 这一层（roe-model 按项目约定只放"纯数据结构，无业务
//! 逻辑"，故不下沉到 roe-model，见 roe-model/src/lib.rs 顶部注释）。
//!
//! 本模块提供两组能力：
//! - `set_char_format_in_range`：把一个 [range_start, range_end) 字节
//!   区间内的 run 应用 `CharFormatPatch`，边界 run 按字符边界拆分为
//!   最多三段（区间外前段 / 区间内中段 / 区间外后段）。
//! - 三个跨 run 范围查询骨架（A/B/C类，见下方说明）：v192 曾为
//!   bold/italic/underline/font_size/font_family/underline_style 六个
//!   属性各手写一份雷同的遍历循环，本次按其实际存在的三种统计语义
//!   收敛为三个通用函数，后续新增属性只需选对语义类别传入闭包，不
//!   需要再手抄一遍遍历逻辑。
//!
//! 两层调用方各自的职责（不下沉的部分）：
//! - roe-api（C-ABI）：解析 RoeLocation、定位到 handle 对应的文档与
//!   段落、越界/非 BODY 校验、把查询结果编码成 C-ABI 各自的哨兵值
//!   （如 font_size 用 -1.0/-2.0，bold 用 0/1/2）。
//! - roe-script（Rhai）：定位到当前文档与段落（经由 with_doc）、把
//!   查询结果编码成 JSON（混合值用 "mixed" 字符串，而不是 C-ABI 风格
//!   的数字哨兵——Rhai 面向 AI 脚本，JSON 本身就能自然表达三态，不
//!   需要为了对齐 C-ABI 牺牲可读性）。

use roe_model::writer::{Paragraph, Run, ScriptPosition, UnderlineStyle};

/// 本次调用要修改哪些字符格式属性；None 字段表示"不动"。
/// 内层 `Option<T>` 中的 None 表示"清除该内联覆盖，回退到段落/字符
/// 样式"，Some(v) 表示"把该属性设为 v"。
#[derive(Debug, Clone, Default)]
pub struct CharFormatPatch {
    pub bold: Option<Option<bool>>,
    pub italic: Option<Option<bool>>,
    pub underline: Option<Option<bool>>,
    /// 字号（单位pt）。内层 None 表示清除内联覆盖回退默认值；
    /// Some(v) 中 v<=0.0 视为非法，调用方（C-ABI/Rhai 层）需要在
    /// 传入前校验，本函数不重复校验（越界校验放在调用方入口，理由
    /// 见 roe-api 里 roe_set_range_char_format_font_size 的实现）。
    pub font_size: Option<Option<f32>>,
    pub font_family: Option<Option<String>>,
    pub font_family_ascii: Option<Option<String>>,
    pub underline_style: Option<Option<UnderlineStyle>>,
    /// v233: 删除线
    pub strikethrough: Option<Option<bool>>,
    /// v233: 上标/下标（同一字段，天然互斥）
    pub script: Option<Option<ScriptPosition>>,
    /// v238: 文字颜色 / 背景色（内层 None = 清除覆盖）
    pub color: Option<Option<roe_model::common::Color>>,
    pub background: Option<Option<roe_model::common::Color>>,
}

impl CharFormatPatch {
    pub fn is_empty(&self) -> bool {
        self.bold.is_none() && self.italic.is_none() && self.underline.is_none()
            && self.font_size.is_none() && self.font_family.is_none()
            && self.font_family_ascii.is_none()
            && self.underline_style.is_none()
            && self.strikethrough.is_none() && self.script.is_none()
            && self.color.is_none() && self.background.is_none()
    }
}

fn apply_patch_to_run(run: &mut Run, patch: &CharFormatPatch) {
    if let Some(bold) = patch.bold {
        run.bold = bold;
    }
    if let Some(italic) = patch.italic {
        run.italic = italic;
    }
    if let Some(underline) = patch.underline {
        run.underline = underline;
    }
    if let Some(font_size) = patch.font_size {
        run.font_size = font_size;
    }
    if let Some(font_family) = &patch.font_family {
        run.font_family = font_family.clone();
    }
    if let Some(font_family_ascii) = &patch.font_family_ascii {
        run.font_family_ascii = font_family_ascii.clone();
    }
    if let Some(underline_style) = patch.underline_style {
        run.underline_style = underline_style;
    }
    if let Some(v) = patch.strikethrough {
        run.strikethrough = v;
    }
    if let Some(v) = patch.script {
        run.script = v;
    }
    if let Some(v) = patch.color {
        run.color = v;
    }
    if let Some(v) = patch.background {
        run.background = v;
    }
}

/// 把 para.runs 中与 [range_start, range_end) 重叠的部分应用 patch，
/// 边界 run 按字符边界拆分为最多三段。
///
/// range_start >= range_end 时为空操作，直接返回 Ok（与调用方对空
/// 区间的宽松处理保持一致）。
pub fn set_char_format_in_range(
    para: &mut Paragraph,
    range_start: usize,
    range_end: usize,
    patch: &CharFormatPatch,
) -> Result<(), String> {
    if range_start >= range_end || patch.is_empty() {
        return Ok(());
    }

    let para_len: usize = para.runs.iter().map(|r| r.text.len()).sum();
    if range_end > para_len {
        return Err(format!(
            "range_end={range_end} 超出段落范围（段落总字节长度={para_len}）"
        ));
    }

    let mut new_runs: Vec<Run> = Vec::with_capacity(para.runs.len() + 2);
    let mut consumed = 0usize;

    for run in para.runs.drain(..) {
        let run_len = run.text.len();
        let run_start = consumed;
        let run_end = consumed + run_len;
        consumed += run_len;

        // 本 run 与选区完全不重叠：原样保留，不 clone 内容之外的开销。
        if run_end <= range_start || run_start >= range_end {
            new_runs.push(run);
            continue;
        }

        let local_lo = range_start.saturating_sub(run_start).min(run_len);
        let local_hi = range_end.saturating_sub(run_start).min(run_len);

        if !run.text.is_char_boundary(local_lo) || !run.text.is_char_boundary(local_hi) {
            return Err(format!(
                "选区边界不在字符边界上（run起始字节偏移={run_start}，local_lo={local_lo}，local_hi={local_hi}）"
            ));
        }

        // 前段（区间外，原格式不变）。
        if local_lo > 0 {
            let mut head = run.clone();
            head.text = run.text[..local_lo].to_string();
            new_runs.push(head);
        }

        // 中段（区间内，应用 patch）。
        if local_hi > local_lo {
            let mut mid = run.clone();
            mid.text = run.text[local_lo..local_hi].to_string();
            apply_patch_to_run(&mut mid, patch);
            new_runs.push(mid);
        }

        // 后段（区间外，原格式不变）。
        if local_hi < run_len {
            let mut tail = run.clone();
            tail.text = run.text[local_hi..].to_string();
            new_runs.push(tail);
        }
    }

    para.runs = new_runs;
    Ok(())
}

/// 跨 run 范围查询骨架：项目里实际存在**三种互不兼容**的统计语义
/// （v191/v192 各自独立设计、行为已被现有测试锁定，本次下沉只做去
/// 重复，不改变任何一种的对外行为，三种差异是否应该统一是产品设计
/// 问题，不在本次重构范围内）：
///
/// - [`get_bool_flag_in_range`]（A类，对应 v191 bold/italic/underline）：
///   `Run.xxx: Option<bool>`，`None` 按"否"计入统计（不是跳过），
///   用两个 flag（saw_true/saw_false）判断一致/混合。
/// - [`get_uniform_or_mixed_in_range`]（B类，对应 v192 font_size/
///   font_family）：`None` 完全跳过、不参与统计，只有出现两个不同
///   的 `Some` 值时才算 mixed，区间内全部为 `None` 时返回 `Empty`。
/// - [`get_option_value_in_range`]（C类，对应 v192 underline_style）：
///   把整个 `Option<T>`（含 `None` 本身）当作待比较的值，`None` 和
///   `Some(x)` 视为不同、触发 mixed；区间内全部为 `None` 时返回
///   `Uniform(None)`（不是 `Empty`——调用方按各自约定的哨兵值编码，
///   如 underline_style 把 `Uniform(None)` 编码成 `3`）。
///
/// 三者都不修改 run，也不需要按字符边界拆分——查询只关心"这个 run
/// 与区间是否有重叠"，不关心重叠了多少字节。

/// A类结果：区间内该 bool 属性的统计状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoolFlagResult {
    AllFalse,
    AllTrue,
    Mixed,
}

/// A类骨架（bold/italic/underline 同款）：`get_value` 从 run 取出
/// `Option<bool>`，`None` 按 false 计入。区间为空或没碰到任何 run
/// 时返回 `AllFalse`（对齐现有三个函数"空区间返回0/非"的既有行为）。
pub fn get_bool_flag_in_range<FGet>(
    para: &Paragraph,
    range_start: usize,
    range_end: usize,
    get_value: FGet,
) -> Result<BoolFlagResult, String>
where
    FGet: Fn(&Run) -> Option<bool>,
{
    let para_len: usize = para.runs.iter().map(|r| r.text.len()).sum();
    if range_end > para_len {
        return Err(format!(
            "range_end={range_end} 超出段落范围（段落总字节长度={para_len}）"
        ));
    }

    if range_start >= range_end {
        return Ok(BoolFlagResult::AllFalse);
    }

    let mut consumed = 0usize;
    let mut saw_true = false;
    let mut saw_false = false;

    for run in &para.runs {
        let run_len = run.text.len();
        let run_start = consumed;
        let run_end = consumed + run_len;
        consumed += run_len;

        if run_end <= range_start || run_start >= range_end {
            continue;
        }

        match get_value(run) {
            Some(true) => saw_true = true,
            _ => saw_false = true,
        }
    }

    Ok(match (saw_true, saw_false) {
        (true, false) => BoolFlagResult::AllTrue,
        (false, true) => BoolFlagResult::AllFalse,
        (true, true) => BoolFlagResult::Mixed,
        (false, false) => BoolFlagResult::AllFalse, // 区间非空但未碰到任何run，理论不会出现，保守按AllFalse
    })
}

/// B/C 两类共用的三态结果。
#[derive(Debug, Clone, PartialEq)]
pub enum RangeQueryResult<T> {
    /// B类：区间为空，或区间内没有任何 run 设置过该属性的内联覆盖
    /// （全部为 None，被跳过后什么都没统计到）。
    /// C类：不会产生这个变体（C类把 None 当作正常值参与统计，全
    /// None 时是 `Uniform(None)`，见下）。
    Empty,
    /// 区间内所有参与统计的值一致。
    Uniform(T),
    /// 区间内至少出现两个不同的值。
    Mixed,
}

/// B类骨架（font_size/font_family 同款）：`get_value` 返回
/// `Option<T>`，`None` 的 run 完全跳过、不参与统计（不算一致也不
/// 单独触发 mixed）；只有出现两个不同的 `Some` 值时才算 mixed；
/// 全部被跳过（区间内所有 run 都是 None）时返回 `Empty`。
pub fn get_uniform_or_mixed_in_range<T, FGet, FEq>(
    para: &Paragraph,
    range_start: usize,
    range_end: usize,
    get_value: FGet,
    values_equal: FEq,
) -> Result<RangeQueryResult<T>, String>
where
    T: Clone,
    FGet: Fn(&Run) -> Option<T>,
    FEq: Fn(&T, &T) -> bool,
{
    let para_len: usize = para.runs.iter().map(|r| r.text.len()).sum();
    if range_end > para_len {
        return Err(format!(
            "range_end={range_end} 超出段落范围（段落总字节长度={para_len}）"
        ));
    }

    if range_start >= range_end {
        return Ok(RangeQueryResult::Empty);
    }

    let mut consumed = 0usize;
    let mut seen: Option<T> = None;
    let mut mixed = false;

    for run in &para.runs {
        let run_len = run.text.len();
        let run_start = consumed;
        let run_end = consumed + run_len;
        consumed += run_len;

        if run_end <= range_start || run_start >= range_end {
            continue;
        }

        if let Some(v) = get_value(run) {
            match &seen {
                None => seen = Some(v),
                Some(prev) if !values_equal(prev, &v) => mixed = true,
                _ => {}
            }
        }
    }

    if mixed {
        Ok(RangeQueryResult::Mixed)
    } else {
        match seen {
            Some(v) => Ok(RangeQueryResult::Uniform(v)),
            None => Ok(RangeQueryResult::Empty),
        }
    }
}

/// C类骨架（underline_style 同款）：`get_value` 返回 `Option<T>`，
/// 但与 B 类不同——`None` 本身被当作一个正常值参与比较（不跳过），
/// 因此 `None` 和 `Some(x)` 会被视为"不同"从而触发 mixed。全部为
/// `None` 时返回 `Uniform(None)`（不是 `Empty`——因为 C 类语义里
/// "全部未设置内联覆盖"本身就是一种一致状态，调用方通常会把这个
/// `Uniform(None)` 编码成一个具体的业务哨兵值，如 underline_style
/// 用 3 表示）。
///
/// 已知简化（继承自原 v192 实现，未在本次改动）：这套语义无法区分
/// "区间内所有 run 都未设置内联覆盖" 与 "区间内所有 run 都显式设置
/// 成了与默认值相同的具体值"，两者都会落到 `Uniform(None)` 或
/// `Uniform(Some(同一个值))`，取决于 run 的真实字段值，调用方如需
/// 进一步区分，需要额外的位标记或拆成两次查询。
pub fn get_option_value_in_range<T, FGet>(
    para: &Paragraph,
    range_start: usize,
    range_end: usize,
    get_value: FGet,
) -> Result<RangeQueryResult<Option<T>>, String>
where
    T: Clone + PartialEq,
    FGet: Fn(&Run) -> Option<T>,
{
    let para_len: usize = para.runs.iter().map(|r| r.text.len()).sum();
    if range_end > para_len {
        return Err(format!(
            "range_end={range_end} 超出段落范围（段落总字节长度={para_len}）"
        ));
    }

    if range_start >= range_end {
        return Ok(RangeQueryResult::Uniform(None));
    }

    let mut consumed = 0usize;
    let mut seen: Option<Option<T>> = None; // 外层None=尚未碰到任何run
    let mut mixed = false;

    for run in &para.runs {
        let run_len = run.text.len();
        let run_start = consumed;
        let run_end = consumed + run_len;
        consumed += run_len;

        if run_end <= range_start || run_start >= range_end {
            continue;
        }

        let v = get_value(run);
        match &seen {
            None => seen = Some(v),
            Some(prev) if *prev != v => mixed = true,
            _ => {}
        }
    }

    if mixed {
        return Ok(RangeQueryResult::Mixed);
    }

    Ok(RangeQueryResult::Uniform(seen.unwrap_or(None)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(text: &str) -> Run {
        Run { text: text.to_string(), ..Default::default() }
    }

    fn para(runs: Vec<Run>) -> Paragraph {
        Paragraph { runs, ..Default::default() }
    }

    #[test]
    fn test_set_in_range_splits_boundary_run() {
        let mut p = para(vec![run("Hello World")]);
        let patch = CharFormatPatch { bold: Some(Some(true)), ..Default::default() };
        set_char_format_in_range(&mut p, 6, 11, &patch).unwrap();
        assert_eq!(p.runs.len(), 2, "应拆分成前段+中段两块（后段为空不产出）");
        assert_eq!(p.runs[0].text, "Hello ");
        assert_eq!(p.runs[0].bold, None);
        assert_eq!(p.runs[1].text, "World");
        assert_eq!(p.runs[1].bold, Some(true));
    }

    #[test]
    fn test_set_in_range_empty_range_is_noop() {
        let mut p = para(vec![run("Hello")]);
        let patch = CharFormatPatch { bold: Some(Some(true)), ..Default::default() };
        set_char_format_in_range(&mut p, 3, 3, &patch).unwrap();
        assert_eq!(p.runs.len(), 1);
        assert_eq!(p.runs[0].bold, None, "空区间应为空操作，不应用patch");
    }

    // ---- A类：get_bool_flag_in_range（bold/italic/underline 同款） ----
    // 对照组：None 按 false 计入统计（不是跳过），与原 v191 三个
    // roe_get_range_char_format_{bold,italic,underline} 的实际行为一致。

    #[test]
    fn test_bool_flag_all_true() {
        let mut r1 = run("Hello ");
        r1.bold = Some(true);
        let mut r2 = run("World");
        r2.bold = Some(true);
        let p = para(vec![r1, r2]);
        let result = get_bool_flag_in_range(&p, 0, 11, |r: &Run| r.bold).unwrap();
        assert_eq!(result, BoolFlagResult::AllTrue);
    }

    #[test]
    fn test_bool_flag_none_counts_as_false() {
        // 一个 run 显式 Some(true)，另一个 run 是 None（未设置内联
        // 覆盖）——None 应按 false 计入，与显式 Some(true) 混在一起
        // 应判定为 Mixed（这是 A 类与 B/C 类最大的行为差异点）。
        let mut r1 = run("Hello ");
        r1.bold = Some(true);
        let r2 = run("World"); // bold: None
        let p = para(vec![r1, r2]);
        let result = get_bool_flag_in_range(&p, 0, 11, |r: &Run| r.bold).unwrap();
        assert_eq!(result, BoolFlagResult::Mixed);
    }

    #[test]
    fn test_bool_flag_all_none_is_all_false() {
        let p = para(vec![run("Hello World")]); // bold: None
        let result = get_bool_flag_in_range(&p, 0, 11, |r: &Run| r.bold).unwrap();
        assert_eq!(result, BoolFlagResult::AllFalse, "全部未设置内联覆盖应按AllFalse处理，不是Empty");
    }

    #[test]
    fn test_bool_flag_empty_range_is_all_false() {
        let p = para(vec![run("Hello")]);
        let result = get_bool_flag_in_range(&p, 2, 2, |r: &Run| r.bold).unwrap();
        assert_eq!(result, BoolFlagResult::AllFalse, "空区间应返回AllFalse，对齐原有0/非加粗约定");
    }

    #[test]
    fn test_bool_flag_out_of_bounds_errs() {
        let p = para(vec![run("Hi")]);
        let result = get_bool_flag_in_range(&p, 0, 999, |r: &Run| r.bold);
        assert!(result.is_err());
    }

    // ---- B类：get_uniform_or_mixed_in_range（font_size/font_family 同款） ----

    #[test]
    fn test_uniform_or_mixed_uniform() {
        let mut r1 = run("Hello ");
        r1.font_size = Some(18.0);
        let mut r2 = run("World");
        r2.font_size = Some(18.0);
        let p = para(vec![r1, r2]);
        let result = get_uniform_or_mixed_in_range(
            &p, 0, 11,
            |r: &Run| r.font_size,
            |a: &f32, b: &f32| (a - b).abs() <= f32::EPSILON,
        ).unwrap();
        assert_eq!(result, RangeQueryResult::Uniform(18.0));
    }

    #[test]
    fn test_uniform_or_mixed_mixed() {
        let mut r1 = run("Hello ");
        r1.font_size = Some(12.0);
        let mut r2 = run("World");
        r2.font_size = Some(14.0);
        let p = para(vec![r1, r2]);
        let result = get_uniform_or_mixed_in_range(
            &p, 0, 11,
            |r: &Run| r.font_size,
            |a: &f32, b: &f32| (a - b).abs() <= f32::EPSILON,
        ).unwrap();
        assert_eq!(result, RangeQueryResult::Mixed);
    }

    #[test]
    fn test_uniform_or_mixed_none_is_skipped_not_counted() {
        // 一个 run 显式 Some(18.0)，另一个是 None——B类应跳过 None，
        // 不计入统计，结果仍应是 Uniform(18.0)，不是 Mixed（这是与
        // A 类"None按false计入"的核心行为差异）。
        let mut r1 = run("Hello ");
        r1.font_size = Some(18.0);
        let r2 = run("World"); // font_size: None
        let p = para(vec![r1, r2]);
        let result = get_uniform_or_mixed_in_range(
            &p, 0, 11,
            |r: &Run| r.font_size,
            |a: &f32, b: &f32| (a - b).abs() <= f32::EPSILON,
        ).unwrap();
        assert_eq!(result, RangeQueryResult::Uniform(18.0));
    }

    #[test]
    fn test_uniform_or_mixed_all_none_is_empty() {
        let p = para(vec![run("Hello World")]);
        let result = get_uniform_or_mixed_in_range(
            &p, 0, 11,
            |r: &Run| r.font_size,
            |a: &f32, b: &f32| (a - b).abs() <= f32::EPSILON,
        ).unwrap();
        assert_eq!(result, RangeQueryResult::Empty, "全部未设置内联覆盖应返回Empty");
    }

    #[test]
    fn test_uniform_or_mixed_empty_range() {
        let p = para(vec![run("Hello")]);
        let result = get_uniform_or_mixed_in_range(
            &p, 2, 2,
            |r: &Run| r.font_size,
            |a: &f32, b: &f32| (a - b).abs() <= f32::EPSILON,
        ).unwrap();
        assert_eq!(result, RangeQueryResult::Empty);
    }

    #[test]
    fn test_uniform_or_mixed_out_of_bounds_errs() {
        let p = para(vec![run("Hi")]);
        let result = get_uniform_or_mixed_in_range(
            &p, 0, 999,
            |r: &Run| r.font_size,
            |a: &f32, b: &f32| (a - b).abs() <= f32::EPSILON,
        );
        assert!(result.is_err());
    }

    // ---- C类：get_option_value_in_range（underline_style 同款） ----

    fn run_with_underline_style(text: &str, style: Option<UnderlineStyle>) -> Run {
        Run { text: text.to_string(), underline_style: style, ..Default::default() }
    }

    #[test]
    fn test_option_value_uniform_some() {
        let p = para(vec![
            run_with_underline_style("Hello ", Some(UnderlineStyle::Double)),
            run_with_underline_style("World", Some(UnderlineStyle::Double)),
        ]);
        let result = get_option_value_in_range(&p, 0, 11, |r: &Run| r.underline_style).unwrap();
        assert_eq!(result, RangeQueryResult::Uniform(Some(UnderlineStyle::Double)));
    }

    #[test]
    fn test_option_value_none_vs_some_is_mixed() {
        // C类的关键行为：None 本身作为值参与比较，None 与 Some(x)
        // 视为不同，触发 Mixed（这与 B 类"None跳过"完全相反）。
        let p = para(vec![
            run_with_underline_style("Hello ", None),
            run_with_underline_style("World", Some(UnderlineStyle::Single)),
        ]);
        let result = get_option_value_in_range(&p, 0, 11, |r: &Run| r.underline_style).unwrap();
        assert_eq!(result, RangeQueryResult::Mixed);
    }

    #[test]
    fn test_option_value_all_none_is_uniform_none() {
        // 全部未设置内联覆盖：C类返回 Uniform(None)，不是 Empty——
        // 调用方（如 underline_style）会把这个编码成具体业务哨兵值(3)。
        let p = para(vec![run_with_underline_style("Hello World", None)]);
        let result = get_option_value_in_range(&p, 0, 11, |r: &Run| r.underline_style).unwrap();
        assert_eq!(result, RangeQueryResult::Uniform(None));
    }

    #[test]
    fn test_option_value_empty_range_is_uniform_none() {
        let p = para(vec![run_with_underline_style("Hello", None)]);
        let result = get_option_value_in_range(&p, 2, 2, |r: &Run| r.underline_style).unwrap();
        assert_eq!(result, RangeQueryResult::Uniform(None));
    }

    #[test]
    fn test_option_value_out_of_bounds_errs() {
        let p = para(vec![run_with_underline_style("Hi", None)]);
        let result = get_option_value_in_range(&p, 0, 999, |r: &Run| r.underline_style);
        assert!(result.is_err());
    }
}
