//! 选区级字符格式设置/读取（WROOT-CHARFMT-01 起点：先落地 bold，
//! 后续 italic/underline/font_size/font_family/underline_style 陆续
//! 补齐，本次改为薄封装：跨 run 拆分/查询的核心逻辑已下沉到
//! roe_writer::char_format，本文件只负责 FFI 边界——解析
//! RoeLocation、定位 handle 对应的文档与段落、越界/非 BODY 校验、
//! 把核心逻辑返回的结果编码成本文件各函数各自约定的 C-ABI 哨兵值。
//! 每个函数的哨兵值编码、错误信息文案与下沉前完全一致，未做改动。
//!
//! 设计要点（供后续维护者/后续加字段时参考）：
//! - CharFormatPatch 每个字段都是 `Option<T>`：None 表示"这次调用不
//!   改这个属性"，Some(v) 表示"把这个属性设为 v"（v 本身可能是
//!   Option，如 bold: Option<Option<bool>>，内层 None 表示"清除该
//!   内联覆盖，回退到段落/字符样式"）。
//! - 跨 run 处理：选区可能横跨多个 run，边界处的 run 需要按字符边界
//!   拆分成"区间外前段 / 区间内中段 / 区间外后段"三部分，只有中段
//!   应用 patch，前后段原样保留。这与 text_edit.rs::replace_range_in_entry
//!   不同——那个函数明确只支持单 run 内替换（见其注释），字符格式
//!   设置必须支持跨 run，不能照抄同一限制。
//! - 三种查询语义（A/B/C类）互不兼容，见 roe_writer::char_format 模块
//!   顶部文档注释：bold/italic/underline 是 A 类（None 按 false 计入），
//!   font_size/font_family 是 B 类（None 跳过不计入），underline_style
//!   是 C 类（None 本身作为值参与比较）。加新属性时先判断该属性该用
//!   哪一类语义，不要想当然照抄邻近属性。

use crate::handle;
use roe_model::writer::{BodyElement, UnderlineStyle};
use roe_parser::RoeDocument;
pub use roe_writer::char_format::{
    CharFormatPatch, set_char_format_in_range,
    BoolFlagResult, RangeQueryResult,
    get_bool_flag_in_range, get_uniform_or_mixed_in_range, get_option_value_in_range,
};

use crate::cursor::RoeLocation;
use crate::error::{set_last_error, RoeErrorCode};

/// entry 层包装：定位段落、校验文档类型、调用 set_char_format_in_range，
/// 成功时返回 Ok(())，用于 with_entry 闭包内。
fn set_char_format_in_entry(
    entry: &mut handle::DocumentEntry,
    para_index: usize,
    range_start: usize,
    range_end: usize,
    patch: &CharFormatPatch,
) -> Result<(), String> {
    // 先只读校验文档类型和段落是否存在，校验通过后才clone快照——
    // 避免对根本不合法的调用也白白clone一次整份文档。
    {
        let RoeDocument::Writer(doc) = &entry.doc else {
            return Err("非 Writer 文档".to_string());
        };
        match doc.body.get(para_index) {
            Some(BodyElement::Paragraph(_)) => {}
            _ => return Err(format!("para_index={para_index} 越界或不存在")),
        }
    }

    // 落 undo 快照：与 roe_replace_range 等现有编辑接口同款做法，
    // 修改前先克隆整份文档。段落级别的字符格式修改数据量通常不大，
    // 与现有其它编辑接口的快照成本量级一致，未引入新的性能特征。
    entry.snapshot = Some(entry.doc.clone());

    let RoeDocument::Writer(doc) = &mut entry.doc else {
        unreachable!("上面已确认是 Writer 文档");
    };
    let Some(BodyElement::Paragraph(para)) = doc.body.get_mut(para_index) else {
        unreachable!("上面已确认 para_index 有效");
    };

    match set_char_format_in_range(para, range_start, range_end, patch) {
        Ok(()) => {
            if let Some(prev) = entry.snapshot.take() {
                let ctx = handle::EditCursorContext::range(para_index, range_start, range_end);
                entry.push_undo_snapshot(prev, ctx);
            }
            Ok(())
        }
        Err(e) => {
            // 失败：丢弃刚才存的快照（本次没有真正发生改动），
            // 与 replace_range_in_entry 失败路径的语义保持一致
            // （早期校验失败时该函数根本不会走到"存快照"这一步；
            // 这里因为快照是在调用 set_char_format_in_range 之前
            // 存的，失败时需要显式清掉，避免留下一个不会被
            // push 也不会被复用的悬空快照）。
            entry.snapshot = None;
            Err(e)
        }
    }
}

/// 只读查询辅助：定位 handle 对应文档的段落，供三类 get 函数共用。
fn with_para<R>(
    handle: u64,
    para_index: usize,
    range_end: usize,
    f: impl FnOnce(&roe_model::writer::Paragraph) -> Result<R, String>,
) -> Option<Result<R, String>> {
    handle::with_entry(handle, |entry| -> Result<R, String> {
        let RoeDocument::Writer(doc) = &entry.doc else {
            return Err("非 Writer 文档".to_string());
        };
        let para = match doc.body.get(para_index) {
            Some(BodyElement::Paragraph(p)) => p,
            _ => return Err(format!("para_index={para_index} 越界或不存在")),
        };
        let para_len: usize = para.runs.iter().map(|r| r.text.len()).sum();
        if range_end > para_len {
            return Err(format!(
                "range_end={range_end} 超出段落范围（段落总字节长度={para_len}）"
            ));
        }
        f(para)
    })
}

/// 设置 [start_char_offset, end_char_offset) 范围内的字符加粗状态。
/// bold 取值：0 = 取消加粗，1 = 设为加粗，2 = 清除内联覆盖（回退到
/// 段落/字符样式的默认值，对应 Run.bold = None）。其余值非法。
///
/// 本次仅支持 container_type = BODY（与 roe_replace_range 同范围）。
/// 成功返回 RoeErrorCode::Ok，失败返回对应错误码并可通过
/// roe_last_error_message 取详情。
#[no_mangle]
pub extern "C" fn roe_set_range_char_format_bold(
    handle: u64,
    container_type: u32,
    container_id: u32,
    local_para_index: u32,
    start_char_offset: u32,
    end_char_offset: u32,
    bold: u32,
) -> i32 {
    let loc = RoeLocation {
        container_type,
        container_id,
        local_para_index,
        char_offset: start_char_offset,
    };

    if !loc.is_body() {
        set_last_error("roe_set_range_char_format_bold: 本次仅支持 container_type=BODY（表格cell/文本框预留未实现）");
        return RoeErrorCode::Unsupported as i32;
    }

    if start_char_offset > end_char_offset {
        set_last_error(format!(
            "roe_set_range_char_format_bold: start_char_offset({start_char_offset}) > end_char_offset({end_char_offset})，非法区间"
        ));
        return RoeErrorCode::ScriptError as i32;
    }

    let bold_value: Option<bool> = match bold {
        0 => Some(false),
        1 => Some(true),
        2 => None,
        _ => {
            set_last_error(format!(
                "roe_set_range_char_format_bold: bold 参数非法值 {bold}（合法值 0/1/2）"
            ));
            return RoeErrorCode::ScriptError as i32;
        }
    };

    let patch = CharFormatPatch {
        bold: Some(bold_value),
        ..Default::default()
    };

    let (para_index, _) = loc.as_legacy_body();
    let start = start_char_offset as usize;
    let end = end_char_offset as usize;

    let result = handle::with_entry(handle, |entry| {
        set_char_format_in_entry(entry, para_index, start, end, &patch)
    });

    let is_ok = matches!(&result, Some(Ok(())));

    // 必须在 with_entry 闭包返回之后调用，避免同一 handle 的 RefCell
    // 嵌套借用 panic（与 roe_insert_text / roe_replace_range 等现有
    // 编辑接口相同的坑，见 text_edit.rs 对应注释）。
    if is_ok {
        handle::invalidate_writer_layout_cache(handle);
    }

    match result {
        Some(Ok(())) => RoeErrorCode::Ok as i32,
        Some(Err(e)) => {
            set_last_error(format!("roe_set_range_char_format_bold: {e}"));
            RoeErrorCode::ScriptError as i32
        }
        None => {
            set_last_error("roe_set_range_char_format_bold: 无效句柄");
            RoeErrorCode::InvalidHandle as i32
        }
    }
}

/// 查询 [start_char_offset, end_char_offset) 范围内的加粗状态。
/// 返回值：0 = 区间内全部为非加粗，1 = 区间内全部为加粗，
/// 2 = 区间内加粗状态不一致（混合），-1 = 出错（详情见
/// roe_last_error_message）。
/// 注意：Run.bold == None（未设置内联覆盖）在本查询中按"非加粗"
/// 处理，这是当前实现的简化：真实是否加粗还取决于段落/字符样式的
/// 继承值，本接口不解析样式继承链，只反映 Run 自身的内联覆盖状态。
/// 后续若 UI 需要"继承来的加粗"也参与判断，需要在这里接入
/// StyleRegistry 查询，当前范围（先支持内联覆盖的加粗按钮）不需要。
#[no_mangle]
pub extern "C" fn roe_get_range_char_format_bold(
    handle: u64,
    container_type: u32,
    container_id: u32,
    local_para_index: u32,
    start_char_offset: u32,
    end_char_offset: u32,
) -> i32 {
    let loc = RoeLocation {
        container_type,
        container_id,
        local_para_index,
        char_offset: start_char_offset,
    };

    if !loc.is_body() {
        set_last_error("roe_get_range_char_format_bold: 本次仅支持 container_type=BODY");
        return -1;
    }

    if start_char_offset > end_char_offset {
        set_last_error(format!(
            "roe_get_range_char_format_bold: start_char_offset({start_char_offset}) > end_char_offset({end_char_offset})，非法区间"
        ));
        return -1;
    }

    let (para_index, _) = loc.as_legacy_body();
    let range_start = start_char_offset as usize;
    let range_end = end_char_offset as usize;

    let result = with_para(handle, para_index, range_end, |para| {
        get_bool_flag_in_range(para, range_start, range_end, |r| r.bold)
            .map(|r| match r {
                BoolFlagResult::AllTrue => 1,
                BoolFlagResult::AllFalse => 0,
                BoolFlagResult::Mixed => 2,
            })
    });

    match result {
        Some(Ok(v)) => v,
        Some(Err(e)) => {
            set_last_error(format!("roe_get_range_char_format_bold: {e}"));
            -1
        }
        None => {
            set_last_error("roe_get_range_char_format_bold: 无效句柄");
            -1
        }
    }
}

/// 设置 [start_char_offset, end_char_offset) 范围内的字符斜体状态。
/// 取值语义、错误处理与 roe_set_range_char_format_bold 完全一致：
/// 0=取消斜体，1=设为斜体，2=清除内联覆盖回退默认值。
#[no_mangle]
pub extern "C" fn roe_set_range_char_format_italic(
    handle: u64,
    container_type: u32,
    container_id: u32,
    local_para_index: u32,
    start_char_offset: u32,
    end_char_offset: u32,
    italic: u32,
) -> i32 {
    let loc = RoeLocation {
        container_type,
        container_id,
        local_para_index,
        char_offset: start_char_offset,
    };

    if !loc.is_body() {
        set_last_error("roe_set_range_char_format_italic: 本次仅支持 container_type=BODY（表格cell/文本框预留未实现）");
        return RoeErrorCode::Unsupported as i32;
    }

    if start_char_offset > end_char_offset {
        set_last_error(format!(
            "roe_set_range_char_format_italic: start_char_offset({start_char_offset}) > end_char_offset({end_char_offset})，非法区间"
        ));
        return RoeErrorCode::ScriptError as i32;
    }

    let italic_value: Option<bool> = match italic {
        0 => Some(false),
        1 => Some(true),
        2 => None,
        _ => {
            set_last_error(format!(
                "roe_set_range_char_format_italic: italic 参数非法值 {italic}（合法值 0/1/2）"
            ));
            return RoeErrorCode::ScriptError as i32;
        }
    };

    let patch = CharFormatPatch {
        italic: Some(italic_value),
        ..Default::default()
    };

    let (para_index, _) = loc.as_legacy_body();
    let start = start_char_offset as usize;
    let end = end_char_offset as usize;

    let result = handle::with_entry(handle, |entry| {
        set_char_format_in_entry(entry, para_index, start, end, &patch)
    });

    let is_ok = matches!(&result, Some(Ok(())));

    if is_ok {
        handle::invalidate_writer_layout_cache(handle);
    }

    match result {
        Some(Ok(())) => RoeErrorCode::Ok as i32,
        Some(Err(e)) => {
            set_last_error(format!("roe_set_range_char_format_italic: {e}"));
            RoeErrorCode::ScriptError as i32
        }
        None => {
            set_last_error("roe_set_range_char_format_italic: 无效句柄");
            RoeErrorCode::InvalidHandle as i32
        }
    }
}

/// 查询 [start_char_offset, end_char_offset) 范围内的斜体状态。
/// 返回值语义与 roe_get_range_char_format_bold 完全一致：
/// 0=全部非斜体，1=全部斜体，2=混合，-1=出错。
/// 同样的简化：Run.italic==None 按"非斜体"处理，不解析样式继承链。
#[no_mangle]
pub extern "C" fn roe_get_range_char_format_italic(
    handle: u64,
    container_type: u32,
    container_id: u32,
    local_para_index: u32,
    start_char_offset: u32,
    end_char_offset: u32,
) -> i32 {
    let loc = RoeLocation {
        container_type,
        container_id,
        local_para_index,
        char_offset: start_char_offset,
    };

    if !loc.is_body() {
        set_last_error("roe_get_range_char_format_italic: 本次仅支持 container_type=BODY");
        return -1;
    }

    if start_char_offset > end_char_offset {
        set_last_error(format!(
            "roe_get_range_char_format_italic: start_char_offset({start_char_offset}) > end_char_offset({end_char_offset})，非法区间"
        ));
        return -1;
    }

    let (para_index, _) = loc.as_legacy_body();
    let range_start = start_char_offset as usize;
    let range_end = end_char_offset as usize;

    let result = with_para(handle, para_index, range_end, |para| {
        get_bool_flag_in_range(para, range_start, range_end, |r| r.italic)
            .map(|r| match r {
                BoolFlagResult::AllTrue => 1,
                BoolFlagResult::AllFalse => 0,
                BoolFlagResult::Mixed => 2,
            })
    });

    match result {
        Some(Ok(v)) => v,
        Some(Err(e)) => {
            set_last_error(format!("roe_get_range_char_format_italic: {e}"));
            -1
        }
        None => {
            set_last_error("roe_get_range_char_format_italic: 无效句柄");
            -1
        }
    }
}

/// 设置 [start_char_offset, end_char_offset) 范围内的字符下划线状态。
/// 取值语义、错误处理与 roe_set_range_char_format_bold 完全一致：
/// 0=取消下划线，1=设为下划线，2=清除内联覆盖回退默认值。
/// 注意：本函数只设置 Run.underline（有/无），不涉及 underline_style
/// （下划线样式，如双线/波浪线等），underline_style 的接口另见下方。
#[no_mangle]
pub extern "C" fn roe_set_range_char_format_underline(
    handle: u64,
    container_type: u32,
    container_id: u32,
    local_para_index: u32,
    start_char_offset: u32,
    end_char_offset: u32,
    underline: u32,
) -> i32 {
    let loc = RoeLocation {
        container_type,
        container_id,
        local_para_index,
        char_offset: start_char_offset,
    };

    if !loc.is_body() {
        set_last_error("roe_set_range_char_format_underline: 本次仅支持 container_type=BODY（表格cell/文本框预留未实现）");
        return RoeErrorCode::Unsupported as i32;
    }

    if start_char_offset > end_char_offset {
        set_last_error(format!(
            "roe_set_range_char_format_underline: start_char_offset({start_char_offset}) > end_char_offset({end_char_offset})，非法区间"
        ));
        return RoeErrorCode::ScriptError as i32;
    }

    let underline_value: Option<bool> = match underline {
        0 => Some(false),
        1 => Some(true),
        2 => None,
        _ => {
            set_last_error(format!(
                "roe_set_range_char_format_underline: underline 参数非法值 {underline}（合法值 0/1/2）"
            ));
            return RoeErrorCode::ScriptError as i32;
        }
    };

    let patch = CharFormatPatch {
        underline: Some(underline_value),
        ..Default::default()
    };

    let (para_index, _) = loc.as_legacy_body();
    let start = start_char_offset as usize;
    let end = end_char_offset as usize;

    let result = handle::with_entry(handle, |entry| {
        set_char_format_in_entry(entry, para_index, start, end, &patch)
    });

    let is_ok = matches!(&result, Some(Ok(())));

    if is_ok {
        handle::invalidate_writer_layout_cache(handle);
    }

    match result {
        Some(Ok(())) => RoeErrorCode::Ok as i32,
        Some(Err(e)) => {
            set_last_error(format!("roe_set_range_char_format_underline: {e}"));
            RoeErrorCode::ScriptError as i32
        }
        None => {
            set_last_error("roe_set_range_char_format_underline: 无效句柄");
            RoeErrorCode::InvalidHandle as i32
        }
    }
}

/// 查询 [start_char_offset, end_char_offset) 范围内的下划线状态。
/// 返回值语义与 roe_get_range_char_format_bold 完全一致：
/// 0=全部无下划线，1=全部有下划线，2=混合，-1=出错。
#[no_mangle]
pub extern "C" fn roe_get_range_char_format_underline(
    handle: u64,
    container_type: u32,
    container_id: u32,
    local_para_index: u32,
    start_char_offset: u32,
    end_char_offset: u32,
) -> i32 {
    let loc = RoeLocation {
        container_type,
        container_id,
        local_para_index,
        char_offset: start_char_offset,
    };

    if !loc.is_body() {
        set_last_error("roe_get_range_char_format_underline: 本次仅支持 container_type=BODY");
        return -1;
    }

    if start_char_offset > end_char_offset {
        set_last_error(format!(
            "roe_get_range_char_format_underline: start_char_offset({start_char_offset}) > end_char_offset({end_char_offset})，非法区间"
        ));
        return -1;
    }

    let (para_index, _) = loc.as_legacy_body();
    let range_start = start_char_offset as usize;
    let range_end = end_char_offset as usize;

    let result = with_para(handle, para_index, range_end, |para| {
        get_bool_flag_in_range(para, range_start, range_end, |r| r.underline)
            .map(|r| match r {
                BoolFlagResult::AllTrue => 1,
                BoolFlagResult::AllFalse => 0,
                BoolFlagResult::Mixed => 2,
            })
    });

    match result {
        Some(Ok(v)) => v,
        Some(Err(e)) => {
            set_last_error(format!("roe_get_range_char_format_underline: {e}"));
            -1
        }
        None => {
            set_last_error("roe_get_range_char_format_underline: 无效句柄");
            -1
        }
    }
}

/// 设置 [start_char_offset, end_char_offset) 范围内的字符字号（单位pt）。
/// font_size 取值语义与 bold/italic/underline 的 0/1/2 三态不同——字号是
/// 连续值，用一个专门的负数哨兵表达"清除"：
///   font_size == -1.0  => 清除内联覆盖，回退到段落/字符样式默认值
///                          （对应 Run.font_size = None）
///   font_size > 0.0     => 设为该字号（对应 Run.font_size = Some(v)）
///   font_size == 0.0 或其它负值（非-1.0）均非法
/// 选择 -1.0 而不是新增一个单独的"clear"布尔参数，是为了让这个函数
/// 保持和 bold/italic/underline 同样的"一个数值参数决定全部行为"的
/// 调用形状，方便前台用同一套调用范式封装四个属性。
#[no_mangle]
pub extern "C" fn roe_set_range_char_format_font_size(
    handle: u64,
    container_type: u32,
    container_id: u32,
    local_para_index: u32,
    start_char_offset: u32,
    end_char_offset: u32,
    font_size: f32,
) -> i32 {
    let loc = RoeLocation {
        container_type,
        container_id,
        local_para_index,
        char_offset: start_char_offset,
    };

    if !loc.is_body() {
        set_last_error("roe_set_range_char_format_font_size: 本次仅支持 container_type=BODY（表格cell/文本框预留未实现）");
        return RoeErrorCode::Unsupported as i32;
    }

    if start_char_offset > end_char_offset {
        set_last_error(format!(
            "roe_set_range_char_format_font_size: start_char_offset({start_char_offset}) > end_char_offset({end_char_offset})，非法区间"
        ));
        return RoeErrorCode::ScriptError as i32;
    }

    let font_size_value: Option<f32> = if font_size == -1.0 {
        None
    } else if font_size > 0.0 {
        Some(font_size)
    } else {
        set_last_error(format!(
            "roe_set_range_char_format_font_size: font_size 参数非法值 {font_size}（合法值：>0.0 的字号，或 -1.0 表示清除覆盖）"
        ));
        return RoeErrorCode::ScriptError as i32;
    };

    let patch = CharFormatPatch {
        font_size: Some(font_size_value),
        ..Default::default()
    };

    let (para_index, _) = loc.as_legacy_body();
    let start = start_char_offset as usize;
    let end = end_char_offset as usize;

    let result = handle::with_entry(handle, |entry| {
        set_char_format_in_entry(entry, para_index, start, end, &patch)
    });

    let is_ok = matches!(&result, Some(Ok(())));

    if is_ok {
        handle::invalidate_writer_layout_cache(handle);
    }

    match result {
        Some(Ok(())) => RoeErrorCode::Ok as i32,
        Some(Err(e)) => {
            set_last_error(format!("roe_set_range_char_format_font_size: {e}"));
            RoeErrorCode::ScriptError as i32
        }
        None => {
            set_last_error("roe_set_range_char_format_font_size: 无效句柄");
            RoeErrorCode::InvalidHandle as i32
        }
    }
}

/// 查询 [start_char_offset, end_char_offset) 范围内的字号。
/// 返回值语义（注意与 roe_get_range_char_format_bold 的 0/1/2/-1 不同，
/// 字号是连续值不能复用那套离散状态码）：
///   >= 0.0  => 区间内字号一致，就是这个值（Run.font_size==None 的 run
///              视为"未设置"，本查询把它当作不参与统计的空值处理，见下）
///   -1.0    => 区间内字号不一致（混合）
///   -2.0    => 出错（无效句柄/越界/非法区间等，详情见 roe_last_error_message）
/// 简化：Run.font_size==None（未设置内联覆盖）本查询按"不参与统计"处理，
/// 不是"混合"也不是某个默认值——如果区间内所有 run 都是 None，返回 -1.0
/// （视为混合/不确定，因为不解析样式继承链，无法给出确定的"一致值"）。
#[no_mangle]
pub extern "C" fn roe_get_range_char_format_font_size(
    handle: u64,
    container_type: u32,
    container_id: u32,
    local_para_index: u32,
    start_char_offset: u32,
    end_char_offset: u32,
) -> f32 {
    let loc = RoeLocation {
        container_type,
        container_id,
        local_para_index,
        char_offset: start_char_offset,
    };

    if !loc.is_body() {
        set_last_error("roe_get_range_char_format_font_size: 本次仅支持 container_type=BODY");
        return -2.0;
    }

    if start_char_offset > end_char_offset {
        set_last_error(format!(
            "roe_get_range_char_format_font_size: start_char_offset({start_char_offset}) > end_char_offset({end_char_offset})，非法区间"
        ));
        return -2.0;
    }

    let (para_index, _) = loc.as_legacy_body();
    let range_start = start_char_offset as usize;
    let range_end = end_char_offset as usize;

    let result = with_para(handle, para_index, range_end, |para| {
        get_uniform_or_mixed_in_range(
            para, range_start, range_end,
            |r| r.font_size,
            |a: &f32, b: &f32| (a - b).abs() <= f32::EPSILON,
        ).map(|r| match r {
            RangeQueryResult::Uniform(v) => v,
            RangeQueryResult::Mixed => -1.0,
            RangeQueryResult::Empty => -1.0,
        })
    });

    match result {
        Some(Ok(v)) => v,
        Some(Err(e)) => {
            set_last_error(format!("roe_get_range_char_format_font_size: {e}"));
            -2.0
        }
        None => {
            set_last_error("roe_get_range_char_format_font_size: 无效句柄");
            -2.0
        }
    }
}

/// 设置 [start_char_offset, end_char_offset) 范围内的字符主字体名。
/// font_family 传空指针（null）表示"清除内联覆盖，回退到段落/字符样式
/// 默认字体"（对应 Run.font_family = None）；传非空指针按 UTF-8 解析为
/// 字体名字符串（非法 UTF-8 报错）。空字符串 "" 被当作合法的具体字体名
/// 处理（不等同于清除），调用方如果想清除必须传 null 指针，不能传空串——
/// 这是与 font_size 用 -1.0 表达"清除"同一设计思路的字符串版本。
/// 只处理 Run.font_family（主字体），不涉及 font_family_ascii（西文字体
/// 覆盖），后者本次未做，属已知范围限制。
#[no_mangle]
pub extern "C" fn roe_set_range_char_format_font_family(
    handle: u64,
    container_type: u32,
    container_id: u32,
    local_para_index: u32,
    start_char_offset: u32,
    end_char_offset: u32,
    font_family: *const std::os::raw::c_char,
) -> i32 {
    let loc = RoeLocation {
        container_type,
        container_id,
        local_para_index,
        char_offset: start_char_offset,
    };

    if !loc.is_body() {
        set_last_error("roe_set_range_char_format_font_family: 本次仅支持 container_type=BODY（表格cell/文本框预留未实现）");
        return RoeErrorCode::Unsupported as i32;
    }

    if start_char_offset > end_char_offset {
        set_last_error(format!(
            "roe_set_range_char_format_font_family: start_char_offset({start_char_offset}) > end_char_offset({end_char_offset})，非法区间"
        ));
        return RoeErrorCode::ScriptError as i32;
    }

    let font_family_value: Option<String> = if font_family.is_null() {
        None
    } else {
        // Safety: 非空指针，调用方需保证指向以NUL结尾的合法内存，
        // 与项目内其它接受 *const c_char 的 C-ABI 函数同样的调用约定。
        match unsafe { std::ffi::CStr::from_ptr(font_family) }.to_str() {
            Ok(s) => Some(s.to_string()),
            Err(_) => {
                set_last_error("roe_set_range_char_format_font_family: font_family 不是合法 UTF-8");
                return RoeErrorCode::Utf8Error as i32;
            }
        }
    };

    let patch = CharFormatPatch {
        font_family: Some(font_family_value),
        ..Default::default()
    };

    let (para_index, _) = loc.as_legacy_body();
    let start = start_char_offset as usize;
    let end = end_char_offset as usize;

    let result = handle::with_entry(handle, |entry| {
        set_char_format_in_entry(entry, para_index, start, end, &patch)
    });

    let is_ok = matches!(&result, Some(Ok(())));

    if is_ok {
        handle::invalidate_writer_layout_cache(handle);
    }

    match result {
        Some(Ok(())) => RoeErrorCode::Ok as i32,
        Some(Err(e)) => {
            set_last_error(format!("roe_set_range_char_format_font_family: {e}"));
            RoeErrorCode::ScriptError as i32
        }
        None => {
            set_last_error("roe_set_range_char_format_font_family: 无效句柄");
            RoeErrorCode::InvalidHandle as i32
        }
    }
}

/// 查询 [start_char_offset, end_char_offset) 范围内的主字体名。
/// 返回值需要调用方用 roe_free_string 释放（与 roe_last_error_message
/// 同样的调用约定）：
///   非空指针，内容为具体字体名  => 区间内字体一致，就是这个字体
///   非空指针，内容为 "__MIXED__" => 区间内字体不一致（混合）。选用
///     这个哨兵字符串而不是空指针，是为了和"出错"/"全部未设置"两种
///     同样返回空指针的情况区分开——见下方两种空指针场景的说明。
///   空指针，且 roe_last_error_message 有内容 => 出错
///   空指针，且 roe_last_error_message 为空 => 区间内所有 run 均未
///     设置内联字体覆盖（Run.font_family 全为 None），不是"混合"也
///     不是"出错"，只是没有可返回的具体值（不解析样式继承链，与
///     其它 get 接口的简化范围一致）。
#[no_mangle]
pub extern "C" fn roe_get_range_char_format_font_family(
    handle: u64,
    container_type: u32,
    container_id: u32,
    local_para_index: u32,
    start_char_offset: u32,
    end_char_offset: u32,
) -> *mut std::os::raw::c_char {
    let loc = RoeLocation {
        container_type,
        container_id,
        local_para_index,
        char_offset: start_char_offset,
    };

    if !loc.is_body() {
        set_last_error("roe_get_range_char_format_font_family: 本次仅支持 container_type=BODY");
        return std::ptr::null_mut();
    }

    if start_char_offset > end_char_offset {
        set_last_error(format!(
            "roe_get_range_char_format_font_family: start_char_offset({start_char_offset}) > end_char_offset({end_char_offset})，非法区间"
        ));
        return std::ptr::null_mut();
    }

    let (para_index, _) = loc.as_legacy_body();
    let range_start = start_char_offset as usize;
    let range_end = end_char_offset as usize;

    let result: Option<Result<Option<String>, String>> = with_para(handle, para_index, range_end, |para| {
        get_uniform_or_mixed_in_range(
            para, range_start, range_end,
            |r| r.font_family.clone(),
            |a: &String, b: &String| a == b,
        ).map(|r| match r {
            RangeQueryResult::Uniform(v) => Some(v),
            RangeQueryResult::Mixed => Some("__MIXED__".to_string()),
            RangeQueryResult::Empty => None,
        })
    });

    match result {
        Some(Ok(Some(s))) => match std::ffi::CString::new(s) {
            Ok(cs) => cs.into_raw(),
            Err(_) => std::ptr::null_mut(),
        },
        Some(Ok(None)) => std::ptr::null_mut(),
        Some(Err(e)) => {
            set_last_error(format!("roe_get_range_char_format_font_family: {e}"));
            std::ptr::null_mut()
        }
        None => {
            set_last_error("roe_get_range_char_format_font_family: 无效句柄");
            std::ptr::null_mut()
        }
    }
}

/// 设置 [start_char_offset, end_char_offset) 范围内的下划线样式。
/// underline_style 取值：0=Single，1=Double，2=Wave，3=None（枚举自身
/// 的"无样式"值，与"未设置内联覆盖"是两回事——3表示run确实携带了
/// UnderlineStyle::None这个具体值），4=清除内联覆盖回退默认值（对应
/// Run.underline_style = None）。其余值非法。
/// 注意：本函数只管 Run.underline_style 字段本身，不处理"underline_style
/// 为None时若underline=true按Single处理"这层渲染期兜底推导——那是
/// 渲染逻辑的职责，不是这个读写接口的职责。
#[no_mangle]
pub extern "C" fn roe_set_range_char_format_underline_style(
    handle: u64,
    container_type: u32,
    container_id: u32,
    local_para_index: u32,
    start_char_offset: u32,
    end_char_offset: u32,
    underline_style: u32,
) -> i32 {
    let loc = RoeLocation {
        container_type,
        container_id,
        local_para_index,
        char_offset: start_char_offset,
    };

    if !loc.is_body() {
        set_last_error("roe_set_range_char_format_underline_style: 本次仅支持 container_type=BODY（表格cell/文本框预留未实现）");
        return RoeErrorCode::Unsupported as i32;
    }

    if start_char_offset > end_char_offset {
        set_last_error(format!(
            "roe_set_range_char_format_underline_style: start_char_offset({start_char_offset}) > end_char_offset({end_char_offset})，非法区间"
        ));
        return RoeErrorCode::ScriptError as i32;
    }

    let style_value: Option<UnderlineStyle> = match underline_style {
        0 => Some(UnderlineStyle::Single),
        1 => Some(UnderlineStyle::Double),
        2 => Some(UnderlineStyle::Wave),
        3 => Some(UnderlineStyle::None),
        4 => None,
        _ => {
            set_last_error(format!(
                "roe_set_range_char_format_underline_style: underline_style 参数非法值 {underline_style}（合法值 0/1/2/3/4）"
            ));
            return RoeErrorCode::ScriptError as i32;
        }
    };

    let patch = CharFormatPatch {
        underline_style: Some(style_value),
        ..Default::default()
    };

    let (para_index, _) = loc.as_legacy_body();
    let start = start_char_offset as usize;
    let end = end_char_offset as usize;

    let result = handle::with_entry(handle, |entry| {
        set_char_format_in_entry(entry, para_index, start, end, &patch)
    });

    let is_ok = matches!(&result, Some(Ok(())));

    if is_ok {
        handle::invalidate_writer_layout_cache(handle);
    }

    match result {
        Some(Ok(())) => RoeErrorCode::Ok as i32,
        Some(Err(e)) => {
            set_last_error(format!("roe_set_range_char_format_underline_style: {e}"));
            RoeErrorCode::ScriptError as i32
        }
        None => {
            set_last_error("roe_set_range_char_format_underline_style: 无效句柄");
            RoeErrorCode::InvalidHandle as i32
        }
    }
}

/// 查询 [start_char_offset, end_char_offset) 范围内的下划线样式。
/// 返回值：0=全部Single，1=全部Double，2=全部Wave，3=全部None（枚举
/// 自身的"无样式"值），4=混合（区间内出现了不同的具体样式，或者
/// 同时出现"已设置某样式"和"未设置内联覆盖"两种run），-1=出错。
/// 简化：Run.underline_style==None（未设置内联覆盖）在本查询中按
/// "参与混合判定"处理——如果区间内既有None又有Some(某样式)，算混合；
/// 如果全部是None，返回3（约定与UnderlineStyle::default()=Single不同，
/// 这里选择"全None"返回3而不是0，是不想让"完全没设置"看起来像是
/// "明确设置成了Single"，3这个值本身是合法业务值(None样式)，两者
/// 用同一个数字返回确实有歧义，已知简化，如后续UI需要区分这两种
/// "3"，需要改成额外的位标记或者拆成两个查询）。
#[no_mangle]
pub extern "C" fn roe_get_range_char_format_underline_style(
    handle: u64,
    container_type: u32,
    container_id: u32,
    local_para_index: u32,
    start_char_offset: u32,
    end_char_offset: u32,
) -> i32 {
    let loc = RoeLocation {
        container_type,
        container_id,
        local_para_index,
        char_offset: start_char_offset,
    };

    if !loc.is_body() {
        set_last_error("roe_get_range_char_format_underline_style: 本次仅支持 container_type=BODY");
        return -1;
    }

    if start_char_offset > end_char_offset {
        set_last_error(format!(
            "roe_get_range_char_format_underline_style: start_char_offset({start_char_offset}) > end_char_offset({end_char_offset})，非法区间"
        ));
        return -1;
    }

    let (para_index, _) = loc.as_legacy_body();
    let range_start = start_char_offset as usize;
    let range_end = end_char_offset as usize;

    let result = with_para(handle, para_index, range_end, |para| {
        get_option_value_in_range(para, range_start, range_end, |r| r.underline_style)
            .map(|r| match r {
                RangeQueryResult::Mixed => 4,
                RangeQueryResult::Uniform(Some(UnderlineStyle::Single)) => 0,
                RangeQueryResult::Uniform(Some(UnderlineStyle::Double)) => 1,
                RangeQueryResult::Uniform(Some(UnderlineStyle::Wave)) => 2,
                RangeQueryResult::Uniform(Some(UnderlineStyle::None)) => 3,
                RangeQueryResult::Uniform(None) => 3,
                RangeQueryResult::Empty => 3, // C类不产生Empty，保留分支仅为类型完整性
            })
    });

    match result {
        Some(Ok(v)) => v,
        Some(Err(e)) => {
            set_last_error(format!("roe_get_range_char_format_underline_style: {e}"));
            -1
        }
        None => {
            set_last_error("roe_get_range_char_format_underline_style: 无效句柄");
            -1
        }
    }
}


/// 设置 [start_char_offset, end_char_offset) 范围内的字符西文字体名
/// （font_family_ascii，与 font_family 是"东亚字体/西文字体对"关系，
/// 语义与 font_family 完全一致，仅字段不同——照抄 font_family 版本）。
/// font_family_ascii 传空指针（null）表示"清除内联覆盖，回退到段落/
/// 字符样式默认字体"（对应 Run.font_family_ascii = None）；传非空指针
/// 按 UTF-8 解析为字体名字符串（非法 UTF-8 报错）。空字符串 "" 被当作
/// 合法的具体字体名处理（不等同于清除），调用方如果想清除必须传 null
/// 指针，不能传空串。
#[no_mangle]
pub extern "C" fn roe_set_range_char_format_font_family_ascii(
    handle: u64,
    container_type: u32,
    container_id: u32,
    local_para_index: u32,
    start_char_offset: u32,
    end_char_offset: u32,
    font_family_ascii: *const std::os::raw::c_char,
) -> i32 {
    let loc = RoeLocation {
        container_type,
        container_id,
        local_para_index,
        char_offset: start_char_offset,
    };

    if !loc.is_body() {
        set_last_error("roe_set_range_char_format_font_family_ascii: 本次仅支持 container_type=BODY（表格cell/文本框预留未实现）");
        return RoeErrorCode::Unsupported as i32;
    }

    if start_char_offset > end_char_offset {
        set_last_error(format!(
            "roe_set_range_char_format_font_family_ascii: start_char_offset({start_char_offset}) > end_char_offset({end_char_offset})，非法区间"
        ));
        return RoeErrorCode::ScriptError as i32;
    }

    let font_family_ascii_value: Option<String> = if font_family_ascii.is_null() {
        None
    } else {
        // Safety: 非空指针，调用方需保证指向以NUL结尾的合法内存，
        // 与项目内其它接受 *const c_char 的 C-ABI 函数同样的调用约定。
        match unsafe { std::ffi::CStr::from_ptr(font_family_ascii) }.to_str() {
            Ok(s) => Some(s.to_string()),
            Err(_) => {
                set_last_error("roe_set_range_char_format_font_family_ascii: font_family_ascii 不是合法 UTF-8");
                return RoeErrorCode::Utf8Error as i32;
            }
        }
    };

    let patch = CharFormatPatch {
        font_family_ascii: Some(font_family_ascii_value),
        ..Default::default()
    };

    let (para_index, _) = loc.as_legacy_body();
    let start = start_char_offset as usize;
    let end = end_char_offset as usize;

    let result = handle::with_entry(handle, |entry| {
        set_char_format_in_entry(entry, para_index, start, end, &patch)
    });

    let is_ok = matches!(&result, Some(Ok(())));

    if is_ok {
        handle::invalidate_writer_layout_cache(handle);
    }

    match result {
        Some(Ok(())) => RoeErrorCode::Ok as i32,
        Some(Err(e)) => {
            set_last_error(format!("roe_set_range_char_format_font_family_ascii: {e}"));
            RoeErrorCode::ScriptError as i32
        }
        None => {
            set_last_error("roe_set_range_char_format_font_family_ascii: 无效句柄");
            RoeErrorCode::InvalidHandle as i32
        }
    }
}

/// 查询 [start_char_offset, end_char_offset) 范围内的西文字体名
/// （font_family_ascii）。返回值需要调用方用 roe_free_string 释放
/// （与 roe_last_error_message 同样的调用约定）：
///   非空指针，内容为具体字体名  => 区间内字体一致，就是这个字体
///   非空指针，内容为 "__MIXED__" => 区间内字体不一致（混合）
///   空指针，且 roe_last_error_message 有内容 => 出错
///   空指针，且 roe_last_error_message 为空 => 区间内所有 run 均未
///     设置内联字体覆盖（Run.font_family_ascii 全为 None）
#[no_mangle]
pub extern "C" fn roe_get_range_char_format_font_family_ascii(
    handle: u64,
    container_type: u32,
    container_id: u32,
    local_para_index: u32,
    start_char_offset: u32,
    end_char_offset: u32,
) -> *mut std::os::raw::c_char {
    let loc = RoeLocation {
        container_type,
        container_id,
        local_para_index,
        char_offset: start_char_offset,
    };

    if !loc.is_body() {
        set_last_error("roe_get_range_char_format_font_family_ascii: 本次仅支持 container_type=BODY");
        return std::ptr::null_mut();
    }

    if start_char_offset > end_char_offset {
        set_last_error(format!(
            "roe_get_range_char_format_font_family_ascii: start_char_offset({start_char_offset}) > end_char_offset({end_char_offset})，非法区间"
        ));
        return std::ptr::null_mut();
    }

    let (para_index, _) = loc.as_legacy_body();
    let range_start = start_char_offset as usize;
    let range_end = end_char_offset as usize;

    let result: Option<Result<Option<String>, String>> = with_para(handle, para_index, range_end, |para| {
        get_uniform_or_mixed_in_range(
            para, range_start, range_end,
            |r| r.font_family_ascii.clone(),
            |a: &String, b: &String| a == b,
        ).map(|r| match r {
            RangeQueryResult::Uniform(v) => Some(v),
            RangeQueryResult::Mixed => Some("__MIXED__".to_string()),
            RangeQueryResult::Empty => None,
        })
    });

    match result {
        Some(Ok(Some(s))) => match std::ffi::CString::new(s) {
            Ok(cs) => cs.into_raw(),
            Err(_) => std::ptr::null_mut(),
        },
        Some(Ok(None)) => std::ptr::null_mut(),
        Some(Err(e)) => {
            set_last_error(format!("roe_get_range_char_format_font_family_ascii: {e}"));
            std::ptr::null_mut()
        }
        None => {
            set_last_error("roe_get_range_char_format_font_family_ascii: 无效句柄");
            std::ptr::null_mut()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roe_model::writer::{Paragraph, Run};

    fn run(text: &str, bold: Option<bool>) -> Run {
        Run {
            text: text.to_string(),
            bold,
            ..Default::default()
        }
    }

    fn para_text(para: &Paragraph) -> Vec<(String, Option<bool>)> {
        para.runs.iter().map(|r| (r.text.clone(), r.bold)).collect()
    }

    #[test]
    fn set_bold_within_single_run_middle() {
        // "Hello World" 单个 run，对中间 "lo Wo" (offset 3..8) 加粗，
        // 应该拆成三段："Hel"(None) / "lo Wo"(Some(true)) / "rld"(None)。
        let mut para = Paragraph {
            runs: vec![run("Hello World", None)],
            ..Default::default()
        };
        let patch = CharFormatPatch { bold: Some(Some(true)), ..Default::default() };
        set_char_format_in_range(&mut para, 3, 8, &patch).unwrap();

        assert_eq!(
            para_text(&para),
            vec![
                ("Hel".to_string(), None),
                ("lo Wo".to_string(), Some(true)),
                ("rld".to_string(), None),
            ]
        );
    }

    #[test]
    fn set_bold_spans_two_runs() {
        // 两个 run："Hello"(5字节) + " World"(6字节)，选区 [3, 8) 横跨两者：
        // 落在第一个run的[3,5)、第二个run的[0,3)。
        let mut para = Paragraph {
            runs: vec![run("Hello", None), run(" World", None)],
            ..Default::default()
        };
        let patch = CharFormatPatch { bold: Some(Some(true)), ..Default::default() };
        set_char_format_in_range(&mut para, 3, 8, &patch).unwrap();

        assert_eq!(
            para_text(&para),
            vec![
                ("Hel".to_string(), None),
                ("lo".to_string(), Some(true)),
                (" Wo".to_string(), Some(true)),
                ("rld".to_string(), None),
            ]
        );
    }

    #[test]
    fn set_bold_spans_three_runs_middle_fully_covered() {
        // 三个run："AAA" + "BBB" + "CCC"，选区覆盖整个中间run加两头各一部分：
        // 选区 [1, 8)：第一个run留"A"，第二个run"BBB"全覆盖，第三个run留"C" + "CC"要正确切分。
        let mut para = Paragraph {
            runs: vec![run("AAA", None), run("BBB", None), run("CCC", None)],
            ..Default::default()
        };
        let patch = CharFormatPatch { bold: Some(Some(true)), ..Default::default() };
        // 总长 9，range [1, 8) 覆盖 "AA BBB CC" 部分（字节索引1到7）
        set_char_format_in_range(&mut para, 1, 8, &patch).unwrap();

        assert_eq!(
            para_text(&para),
            vec![
                ("A".to_string(), None),
                ("AA".to_string(), Some(true)),
                ("BBB".to_string(), Some(true)),
                ("CC".to_string(), Some(true)),
                ("C".to_string(), None),
            ]
        );
    }

    #[test]
    fn set_bold_exact_run_boundary_no_extra_split() {
        // 选区恰好等于某个run的完整范围，不应产生多余的空run。
        let mut para = Paragraph {
            runs: vec![run("Hello", None), run("World", None)],
            ..Default::default()
        };
        let patch = CharFormatPatch { bold: Some(Some(true)), ..Default::default() };
        // range [5, 10) 正好是第二个run "World" 的完整范围
        set_char_format_in_range(&mut para, 5, 10, &patch).unwrap();

        assert_eq!(
            para_text(&para),
            vec![
                ("Hello".to_string(), None),
                ("World".to_string(), Some(true)),
            ]
        );
    }

    #[test]
    fn set_bold_clear_override_back_to_none() {
        // bold=None（清除内联覆盖）能正确把已有的 Some(true) 清掉。
        let mut para = Paragraph {
            runs: vec![run("Hello", Some(true))],
            ..Default::default()
        };
        let patch = CharFormatPatch { bold: Some(None), ..Default::default() };
        set_char_format_in_range(&mut para, 0, 5, &patch).unwrap();

        assert_eq!(para_text(&para), vec![("Hello".to_string(), None)]);
    }

    #[test]
    fn set_bold_out_of_range_errors_not_panics() {
        let mut para = Paragraph {
            runs: vec![run("Hi", None)],
            ..Default::default()
        };
        let patch = CharFormatPatch { bold: Some(Some(true)), ..Default::default() };
        let result = set_char_format_in_range(&mut para, 0, 100, &patch);
        assert!(result.is_err());
    }

    #[test]
    fn set_bold_empty_range_is_noop() {
        let mut para = Paragraph {
            runs: vec![run("Hello", None)],
            ..Default::default()
        };
        let patch = CharFormatPatch { bold: Some(Some(true)), ..Default::default() };
        set_char_format_in_range(&mut para, 3, 3, &patch).unwrap();
        assert_eq!(para_text(&para), vec![("Hello".to_string(), None)]);
    }

    #[test]
    fn set_bold_empty_patch_is_noop() {
        let mut para = Paragraph {
            runs: vec![run("Hello", None)],
            ..Default::default()
        };
        let patch = CharFormatPatch::default();
        set_char_format_in_range(&mut para, 0, 5, &patch).unwrap();
        assert_eq!(para_text(&para), vec![("Hello".to_string(), None)]);
    }

    // ---- italic ----

    fn run_italic(text: &str, italic: Option<bool>) -> Run {
        Run { text: text.to_string(), italic, ..Default::default() }
    }

    fn para_italic(para: &Paragraph) -> Vec<(String, Option<bool>)> {
        para.runs.iter().map(|r| (r.text.clone(), r.italic)).collect()
    }

    #[test]
    fn set_italic_spans_two_runs() {
        let mut para = Paragraph {
            runs: vec![run_italic("Hello", None), run_italic(" World", None)],
            ..Default::default()
        };
        let patch = CharFormatPatch { italic: Some(Some(true)), ..Default::default() };
        set_char_format_in_range(&mut para, 3, 8, &patch).unwrap();

        assert_eq!(
            para_italic(&para),
            vec![
                ("Hel".to_string(), None),
                ("lo".to_string(), Some(true)),
                (" Wo".to_string(), Some(true)),
                ("rld".to_string(), None),
            ]
        );
    }

    #[test]
    fn set_italic_clear_override_back_to_none() {
        let mut para = Paragraph {
            runs: vec![run_italic("Hello", Some(true))],
            ..Default::default()
        };
        let patch = CharFormatPatch { italic: Some(None), ..Default::default() };
        set_char_format_in_range(&mut para, 0, 5, &patch).unwrap();
        assert_eq!(para_italic(&para), vec![("Hello".to_string(), None)]);
    }

    // ---- underline ----

    fn run_underline(text: &str, underline: Option<bool>) -> Run {
        Run { text: text.to_string(), underline, ..Default::default() }
    }

    fn para_underline(para: &Paragraph) -> Vec<(String, Option<bool>)> {
        para.runs.iter().map(|r| (r.text.clone(), r.underline)).collect()
    }

    #[test]
    fn set_underline_spans_two_runs() {
        let mut para = Paragraph {
            runs: vec![run_underline("Hello", None), run_underline(" World", None)],
            ..Default::default()
        };
        let patch = CharFormatPatch { underline: Some(Some(true)), ..Default::default() };
        set_char_format_in_range(&mut para, 3, 8, &patch).unwrap();

        assert_eq!(
            para_underline(&para),
            vec![
                ("Hel".to_string(), None),
                ("lo".to_string(), Some(true)),
                (" Wo".to_string(), Some(true)),
                ("rld".to_string(), None),
            ]
        );
    }

    #[test]
    fn set_underline_clear_override_back_to_none() {
        let mut para = Paragraph {
            runs: vec![run_underline("Hello", Some(true))],
            ..Default::default()
        };
        let patch = CharFormatPatch { underline: Some(None), ..Default::default() };
        set_char_format_in_range(&mut para, 0, 5, &patch).unwrap();
        assert_eq!(para_underline(&para), vec![("Hello".to_string(), None)]);
    }

    // ---- font_size ----

    fn run_font_size(text: &str, font_size: Option<f32>) -> Run {
        Run { text: text.to_string(), font_size, ..Default::default() }
    }

    fn para_font_size(para: &Paragraph) -> Vec<(String, Option<f32>)> {
        para.runs.iter().map(|r| (r.text.clone(), r.font_size)).collect()
    }

    #[test]
    fn set_font_size_spans_two_runs() {
        let mut para = Paragraph {
            runs: vec![run_font_size("Hello", None), run_font_size(" World", None)],
            ..Default::default()
        };
        let patch = CharFormatPatch { font_size: Some(Some(18.0)), ..Default::default() };
        set_char_format_in_range(&mut para, 3, 8, &patch).unwrap();

        assert_eq!(
            para_font_size(&para),
            vec![
                ("Hel".to_string(), None),
                ("lo".to_string(), Some(18.0)),
                (" Wo".to_string(), Some(18.0)),
                ("rld".to_string(), None),
            ]
        );
    }

    #[test]
    fn set_font_size_clear_override_back_to_none() {
        let mut para = Paragraph {
            runs: vec![run_font_size("Hello", Some(24.0))],
            ..Default::default()
        };
        let patch = CharFormatPatch { font_size: Some(None), ..Default::default() };
        set_char_format_in_range(&mut para, 0, 5, &patch).unwrap();
        assert_eq!(para_font_size(&para), vec![("Hello".to_string(), None)]);
    }

    // ---- font_size: 混合值/全None 查询语义（复刻
    // roe_get_range_char_format_font_size 内部对
    // get_uniform_or_mixed_in_range 结果的编码，验证符合函数注释里
    // 约定的语义）。

    fn compute_font_size_query(para: &Paragraph, range_start: usize, range_end: usize) -> f32 {
        get_uniform_or_mixed_in_range(
            para, range_start, range_end,
            |r| r.font_size,
            |a: &f32, b: &f32| (a - b).abs() <= f32::EPSILON,
        ).map(|r| match r {
            RangeQueryResult::Uniform(v) => v,
            RangeQueryResult::Mixed => -1.0,
            RangeQueryResult::Empty => -1.0,
        }).unwrap()
    }

    #[test]
    fn get_font_size_uniform_returns_value() {
        let para = Paragraph {
            runs: vec![run_font_size("Hello", Some(18.0)), run_font_size(" World", Some(18.0))],
            ..Default::default()
        };
        assert_eq!(compute_font_size_query(&para, 0, 11), 18.0);
    }

    #[test]
    fn get_font_size_mixed_returns_negative_one() {
        let para = Paragraph {
            runs: vec![run_font_size("Hello", Some(18.0)), run_font_size(" World", Some(24.0))],
            ..Default::default()
        };
        assert_eq!(compute_font_size_query(&para, 0, 11), -1.0);
    }

    #[test]
    fn get_font_size_all_none_returns_negative_one() {
        let para = Paragraph {
            runs: vec![run_font_size("Hello", None)],
            ..Default::default()
        };
        assert_eq!(compute_font_size_query(&para, 0, 5), -1.0);
    }

    // ---- font_family ----

    fn run_font_family(text: &str, font_family: Option<&str>) -> Run {
        Run { text: text.to_string(), font_family: font_family.map(|s| s.to_string()), ..Default::default() }
    }

    fn para_font_family(para: &Paragraph) -> Vec<(String, Option<String>)> {
        para.runs.iter().map(|r| (r.text.clone(), r.font_family.clone())).collect()
    }

    #[test]
    fn set_font_family_spans_two_runs() {
        let mut para = Paragraph {
            runs: vec![run_font_family("Hello", None), run_font_family(" World", None)],
            ..Default::default()
        };
        let patch = CharFormatPatch { font_family: Some(Some("Arial".to_string())), ..Default::default() };
        set_char_format_in_range(&mut para, 3, 8, &patch).unwrap();

        assert_eq!(
            para_font_family(&para),
            vec![
                ("Hel".to_string(), None),
                ("lo".to_string(), Some("Arial".to_string())),
                (" Wo".to_string(), Some("Arial".to_string())),
                ("rld".to_string(), None),
            ]
        );
    }

    #[test]
    fn set_font_family_clear_override_back_to_none() {
        let mut para = Paragraph {
            runs: vec![run_font_family("Hello", Some("Times New Roman"))],
            ..Default::default()
        };
        let patch = CharFormatPatch { font_family: Some(None), ..Default::default() };
        set_char_format_in_range(&mut para, 0, 5, &patch).unwrap();
        assert_eq!(para_font_family(&para), vec![("Hello".to_string(), None)]);
    }

    fn compute_font_family_query(para: &Paragraph, range_start: usize, range_end: usize) -> Option<String> {
        get_uniform_or_mixed_in_range(
            para, range_start, range_end,
            |r| r.font_family.clone(),
            |a: &String, b: &String| a == b,
        ).map(|r| match r {
            RangeQueryResult::Uniform(v) => Some(v),
            RangeQueryResult::Mixed => Some("__MIXED__".to_string()),
            RangeQueryResult::Empty => None,
        }).unwrap()
    }

    #[test]
    fn get_font_family_uniform_returns_value() {
        let para = Paragraph {
            runs: vec![run_font_family("Hello", Some("Arial")), run_font_family(" World", Some("Arial"))],
            ..Default::default()
        };
        assert_eq!(compute_font_family_query(&para, 0, 11), Some("Arial".to_string()));
    }

    #[test]
    fn get_font_family_mixed_returns_sentinel() {
        let para = Paragraph {
            runs: vec![run_font_family("Hello", Some("Arial")), run_font_family(" World", Some("Courier"))],
            ..Default::default()
        };
        assert_eq!(compute_font_family_query(&para, 0, 11), Some("__MIXED__".to_string()));
    }

    #[test]
    fn get_font_family_all_none_returns_none() {
        let para = Paragraph {
            runs: vec![run_font_family("Hello", None)],
            ..Default::default()
        };
        assert_eq!(compute_font_family_query(&para, 0, 5), None);
    }

    // ---- font_family_ascii ----

    fn run_font_family_ascii(text: &str, font_family_ascii: Option<&str>) -> Run {
        Run { text: text.to_string(), font_family_ascii: font_family_ascii.map(|s| s.to_string()), ..Default::default() }
    }

    fn para_font_family_ascii(para: &Paragraph) -> Vec<(String, Option<String>)> {
        para.runs.iter().map(|r| (r.text.clone(), r.font_family_ascii.clone())).collect()
    }

    #[test]
    fn set_font_family_ascii_spans_two_runs() {
        let mut para = Paragraph {
            runs: vec![run_font_family_ascii("Hello", None), run_font_family_ascii(" World", None)],
            ..Default::default()
        };
        let patch = CharFormatPatch { font_family_ascii: Some(Some("Arial".to_string())), ..Default::default() };
        set_char_format_in_range(&mut para, 3, 8, &patch).unwrap();

        assert_eq!(
            para_font_family_ascii(&para),
            vec![
                ("Hel".to_string(), None),
                ("lo".to_string(), Some("Arial".to_string())),
                (" Wo".to_string(), Some("Arial".to_string())),
                ("rld".to_string(), None),
            ]
        );
    }

    #[test]
    fn set_font_family_ascii_clear_override_back_to_none() {
        let mut para = Paragraph {
            runs: vec![run_font_family_ascii("Hello", Some("Times New Roman"))],
            ..Default::default()
        };
        let patch = CharFormatPatch { font_family_ascii: Some(None), ..Default::default() };
        set_char_format_in_range(&mut para, 0, 5, &patch).unwrap();
        assert_eq!(para_font_family_ascii(&para), vec![("Hello".to_string(), None)]);
    }

    fn compute_font_family_ascii_query(para: &Paragraph, range_start: usize, range_end: usize) -> Option<String> {
        get_uniform_or_mixed_in_range(
            para, range_start, range_end,
            |r| r.font_family_ascii.clone(),
            |a: &String, b: &String| a == b,
        ).map(|r| match r {
            RangeQueryResult::Uniform(v) => Some(v),
            RangeQueryResult::Mixed => Some("__MIXED__".to_string()),
            RangeQueryResult::Empty => None,
        }).unwrap()
    }

    #[test]
    fn get_font_family_ascii_uniform_returns_value() {
        let para = Paragraph {
            runs: vec![run_font_family_ascii("Hello", Some("Arial")), run_font_family_ascii(" World", Some("Arial"))],
            ..Default::default()
        };
        assert_eq!(compute_font_family_ascii_query(&para, 0, 11), Some("Arial".to_string()));
    }

    #[test]
    fn get_font_family_ascii_mixed_returns_sentinel() {
        let para = Paragraph {
            runs: vec![run_font_family_ascii("Hello", Some("Arial")), run_font_family_ascii(" World", Some("Courier"))],
            ..Default::default()
        };
        assert_eq!(compute_font_family_ascii_query(&para, 0, 11), Some("__MIXED__".to_string()));
    }

    #[test]
    fn get_font_family_ascii_all_none_returns_none() {
        let para = Paragraph {
            runs: vec![run_font_family_ascii("Hello", None)],
            ..Default::default()
        };
        assert_eq!(compute_font_family_ascii_query(&para, 0, 5), None);
    }

    #[test]
    fn font_family_and_font_family_ascii_are_independent() {
        let mut para = Paragraph {
            runs: vec![run_font_family("Hello", Some("MS Mincho"))],
            ..Default::default()
        };
        // 只设置 font_family_ascii，不应影响已有的 font_family
        let patch = CharFormatPatch { font_family_ascii: Some(Some("Times New Roman".to_string())), ..Default::default() };
        set_char_format_in_range(&mut para, 0, 5, &patch).unwrap();

        assert_eq!(para.runs[0].font_family, Some("MS Mincho".to_string()));
        assert_eq!(para.runs[0].font_family_ascii, Some("Times New Roman".to_string()));
    }

    // ---- underline_style ----

    fn run_underline_style(text: &str, style: Option<UnderlineStyle>) -> Run {
        Run { text: text.to_string(), underline_style: style, ..Default::default() }
    }

    fn para_underline_style(para: &Paragraph) -> Vec<(String, Option<UnderlineStyle>)> {
        para.runs.iter().map(|r| (r.text.clone(), r.underline_style)).collect()
    }

    #[test]
    fn set_underline_style_spans_two_runs() {
        let mut para = Paragraph {
            runs: vec![run_underline_style("Hello", None), run_underline_style(" World", None)],
            ..Default::default()
        };
        let patch = CharFormatPatch { underline_style: Some(Some(UnderlineStyle::Wave)), ..Default::default() };
        set_char_format_in_range(&mut para, 3, 8, &patch).unwrap();

        assert_eq!(
            para_underline_style(&para),
            vec![
                ("Hel".to_string(), None),
                ("lo".to_string(), Some(UnderlineStyle::Wave)),
                (" Wo".to_string(), Some(UnderlineStyle::Wave)),
                ("rld".to_string(), None),
            ]
        );
    }

    #[test]
    fn set_underline_style_clear_override_back_to_none() {
        let mut para = Paragraph {
            runs: vec![run_underline_style("Hello", Some(UnderlineStyle::Double))],
            ..Default::default()
        };
        let patch = CharFormatPatch { underline_style: Some(None), ..Default::default() };
        set_char_format_in_range(&mut para, 0, 5, &patch).unwrap();
        assert_eq!(para_underline_style(&para), vec![("Hello".to_string(), None)]);
    }

    fn compute_underline_style_query(para: &Paragraph, range_start: usize, range_end: usize) -> i32 {
        get_option_value_in_range(para, range_start, range_end, |r| r.underline_style)
            .map(|r| match r {
                RangeQueryResult::Mixed => 4,
                RangeQueryResult::Uniform(Some(UnderlineStyle::Single)) => 0,
                RangeQueryResult::Uniform(Some(UnderlineStyle::Double)) => 1,
                RangeQueryResult::Uniform(Some(UnderlineStyle::Wave)) => 2,
                RangeQueryResult::Uniform(Some(UnderlineStyle::None)) => 3,
                RangeQueryResult::Uniform(None) => 3,
                RangeQueryResult::Empty => 3,
            }).unwrap()
    }

    #[test]
    fn get_underline_style_uniform_returns_value() {
        let para = Paragraph {
            runs: vec![
                run_underline_style("Hello", Some(UnderlineStyle::Double)),
                run_underline_style(" World", Some(UnderlineStyle::Double)),
            ],
            ..Default::default()
        };
        assert_eq!(compute_underline_style_query(&para, 0, 11), 1);
    }

    #[test]
    fn get_underline_style_mixed_returns_four() {
        let para = Paragraph {
            runs: vec![
                run_underline_style("Hello", Some(UnderlineStyle::Single)),
                run_underline_style(" World", Some(UnderlineStyle::Wave)),
            ],
            ..Default::default()
        };
        assert_eq!(compute_underline_style_query(&para, 0, 11), 4);
    }

    #[test]
    fn get_underline_style_all_none_returns_three() {
        let para = Paragraph {
            runs: vec![run_underline_style("Hello", None)],
            ..Default::default()
        };
        assert_eq!(compute_underline_style_query(&para, 0, 5), 3);
    }
}


// ───────── v233: 删除线 / 上下标 / 段落对齐 ─────────

fn set_patch_range(fname: &str, handle: u64, ct: u32, cid: u32, para: u32, s: u32, e: u32, patch: CharFormatPatch) -> i32 {
    let loc = RoeLocation { container_type: ct, container_id: cid, local_para_index: para, char_offset: s };
    if !loc.is_body() {
        set_last_error(format!("{fname}: 本次仅支持 container_type=BODY"));
        return RoeErrorCode::Unsupported as i32;
    }
    if s > e {
        set_last_error(format!("{fname}: start({s}) > end({e})，非法区间"));
        return RoeErrorCode::ScriptError as i32;
    }
    let (para_index, _) = loc.as_legacy_body();
    let result = handle::with_entry(handle, |entry| {
        set_char_format_in_entry(entry, para_index, s as usize, e as usize, &patch)
    });
    let ok = matches!(&result, Some(Ok(())));
    if ok {
        handle::invalidate_writer_layout_cache(handle);
    }
    match result {
        Some(Ok(())) => RoeErrorCode::Ok as i32,
        Some(Err(m)) => { set_last_error(format!("{fname}: {m}")); RoeErrorCode::ScriptError as i32 }
        None => { set_last_error(format!("{fname}: 无效句柄")); RoeErrorCode::InvalidHandle as i32 }
    }
}

/// strikethrough: 0=取消 1=设置 2=清除覆盖
#[no_mangle]
pub extern "C" fn roe_set_range_char_format_strikethrough(
    handle: u64, container_type: u32, container_id: u32,
    local_para_index: u32, start_char_offset: u32, end_char_offset: u32, strikethrough: u32,
) -> i32 {
    let v = match strikethrough {
        0 => Some(false), 1 => Some(true), 2 => None,
        _ => { set_last_error("roe_set_range_char_format_strikethrough: 参数非法（合法值 0/1/2）"); return RoeErrorCode::ScriptError as i32; }
    };
    let patch = CharFormatPatch { strikethrough: Some(v), ..Default::default() };
    set_patch_range("roe_set_range_char_format_strikethrough", handle, container_type, container_id, local_para_index, start_char_offset, end_char_offset, patch)
}

/// 返回 0=全否 1=全是 2=混合 -1=出错
#[no_mangle]
pub extern "C" fn roe_get_range_char_format_strikethrough(
    handle: u64, container_type: u32, container_id: u32,
    local_para_index: u32, start_char_offset: u32, end_char_offset: u32,
) -> i32 {
    let loc = RoeLocation { container_type, container_id, local_para_index, char_offset: start_char_offset };
    if !loc.is_body() || start_char_offset > end_char_offset {
        set_last_error("roe_get_range_char_format_strikethrough: 参数非法");
        return -1;
    }
    let (para_index, _) = loc.as_legacy_body();
    let (rs, re) = (start_char_offset as usize, end_char_offset as usize);
    let result = with_para(handle, para_index, re, |para| {
        get_bool_flag_in_range(para, rs, re, |r| r.strikethrough).map(|r| match r {
            BoolFlagResult::AllTrue => 1, BoolFlagResult::AllFalse => 0, BoolFlagResult::Mixed => 2,
        })
    });
    match result {
        Some(Ok(v)) => v,
        Some(Err(m)) => { set_last_error(format!("roe_get_range_char_format_strikethrough: {m}")); -1 }
        None => { set_last_error("roe_get_range_char_format_strikethrough: 无效句柄"); -1 }
    }
}

/// script: 0=正常 1=上标 2=下标 3=清除覆盖
#[no_mangle]
pub extern "C" fn roe_set_range_char_format_script(
    handle: u64, container_type: u32, container_id: u32,
    local_para_index: u32, start_char_offset: u32, end_char_offset: u32, script: u32,
) -> i32 {
    use roe_model::writer::ScriptPosition as SP;
    let v = match script {
        0 => Some(SP::Normal), 1 => Some(SP::Superscript), 2 => Some(SP::Subscript), 3 => None,
        _ => { set_last_error("roe_set_range_char_format_script: 参数非法（合法值 0/1/2/3）"); return RoeErrorCode::ScriptError as i32; }
    };
    let patch = CharFormatPatch { script: Some(v), ..Default::default() };
    set_patch_range("roe_set_range_char_format_script", handle, container_type, container_id, local_para_index, start_char_offset, end_char_offset, patch)
}

/// which: 1=查上标 2=查下标。返回 0=全否 1=全是 2=混合 -1=出错
#[no_mangle]
pub extern "C" fn roe_get_range_char_format_script(
    handle: u64, container_type: u32, container_id: u32,
    local_para_index: u32, start_char_offset: u32, end_char_offset: u32, which: u32,
) -> i32 {
    use roe_model::writer::ScriptPosition as SP;
    let target = match which { 1 => SP::Superscript, 2 => SP::Subscript, _ => { set_last_error("roe_get_range_char_format_script: which 须为1或2"); return -1; } };
    let loc = RoeLocation { container_type, container_id, local_para_index, char_offset: start_char_offset };
    if !loc.is_body() || start_char_offset > end_char_offset {
        set_last_error("roe_get_range_char_format_script: 参数非法");
        return -1;
    }
    let (para_index, _) = loc.as_legacy_body();
    let (rs, re) = (start_char_offset as usize, end_char_offset as usize);
    let result = with_para(handle, para_index, re, |para| {
        get_bool_flag_in_range(para, rs, re, |r| Some(r.script == Some(target))).map(|r| match r {
            BoolFlagResult::AllTrue => 1, BoolFlagResult::AllFalse => 0, BoolFlagResult::Mixed => 2,
        })
    });
    match result {
        Some(Ok(v)) => v,
        Some(Err(m)) => { set_last_error(format!("roe_get_range_char_format_script: {m}")); -1 }
        None => { set_last_error("roe_get_range_char_format_script: 无效句柄"); -1 }
    }
}

/// 设置整段对齐。align: 0=左 1=中 2=右 3=两端 4=清除覆盖
#[no_mangle]
pub extern "C" fn roe_set_paragraph_alignment(handle: u64, local_para_index: u32, align: u32) -> i32 {
    use roe_model::style::Alignment as AL;
    let v = match align {
        0 => Some(AL::Left), 1 => Some(AL::Center), 2 => Some(AL::Right), 3 => Some(AL::Justify), 4 => None,
        _ => { set_last_error("roe_set_paragraph_alignment: 参数非法（合法值 0-4）"); return RoeErrorCode::ScriptError as i32; }
    };
    let para_index = local_para_index as usize;
    let result = handle::with_entry(handle, |entry| -> Result<(), String> {
        {
            let RoeDocument::Writer(doc) = &entry.doc else { return Err("非 Writer 文档".to_string()); };
            match doc.body.get(para_index) {
                Some(BodyElement::Paragraph(_)) => {}
                _ => return Err(format!("para_index={para_index} 越界或不是段落")),
            }
        }
        let prev = entry.doc.clone();
        let RoeDocument::Writer(doc) = &mut entry.doc else { unreachable!() };
        if let Some(BodyElement::Paragraph(p)) = doc.body.get_mut(para_index) {
            p.alignment = v;
        }
        entry.push_undo_snapshot(prev, handle::EditCursorContext::at(para_index, 0));
        Ok(())
    });
    let ok = matches!(&result, Some(Ok(())));
    if ok {
        handle::invalidate_writer_layout_cache(handle);
    }
    match result {
        Some(Ok(())) => RoeErrorCode::Ok as i32,
        Some(Err(m)) => { set_last_error(format!("roe_set_paragraph_alignment: {m}")); RoeErrorCode::ScriptError as i32 }
        None => { set_last_error("roe_set_paragraph_alignment: 无效句柄"); RoeErrorCode::InvalidHandle as i32 }
    }
}


/// v273: 行距规则。mode 0=倍数(value 0.5-10) 1=固定值(pt) 2=最小值(pt)。带一次撤销快照。
#[no_mangle]
pub extern "C" fn roe_set_paragraph_line_rule(handle: u64, local_para_index: u32, mode: u32, value: f32) -> i32 {
    if mode > 2 { set_last_error("roe_set_paragraph_line_rule: mode 非法（合法值 0-2）"); return RoeErrorCode::ScriptError as i32; }
    if !value.is_finite() || value <= 0.0 || value > 1000.0 { set_last_error("roe_set_paragraph_line_rule: 数值非法"); return RoeErrorCode::ScriptError as i32; }
    if mode == 0 && !(0.5..=10.0).contains(&value) { set_last_error("roe_set_paragraph_line_rule: 行距倍数须在 0.5-10"); return RoeErrorCode::ScriptError as i32; }
    let para_index = local_para_index as usize;
    let result = handle::with_entry(handle, |entry| -> Result<(), String> {
        {
            let RoeDocument::Writer(doc) = &entry.doc else { return Err("非 Writer 文档".to_string()); };
            match doc.body.get(para_index) {
                Some(BodyElement::Paragraph(_)) => {}
                _ => return Err(format!("para_index={para_index} 越界或不是段落")),
            }
        }
        let prev = entry.doc.clone();
        let RoeDocument::Writer(doc) = &mut entry.doc else { unreachable!() };
        if let Some(BodyElement::Paragraph(p)) = doc.body.get_mut(para_index) {
            if mode == 0 {
                p.line_height = Some(value);
                p.line_abs_pt = None;
                p.line_exact = None;
            } else {
                p.line_abs_pt = Some(value);
                p.line_exact = Some(mode == 1);
            }
        }
        entry.push_undo_snapshot(prev, handle::EditCursorContext::at(para_index, 0));
        Ok(())
    });
    let ok = matches!(&result, Some(Ok(())));
    if ok { handle::invalidate_writer_layout_cache(handle); }
    match result {
        Some(Ok(())) => RoeErrorCode::Ok as i32,
        Some(Err(m)) => { set_last_error(format!("roe_set_paragraph_line_rule: {m}")); RoeErrorCode::ScriptError as i32 }
        None => { set_last_error("roe_set_paragraph_line_rule: 无效句柄"); RoeErrorCode::InvalidHandle as i32 }
    }
}


/// v272: 段落底纹。rgb=0xRRGGBB；clear!=0 清除（rgb 忽略）。带一次撤销快照。
#[no_mangle]
pub extern "C" fn roe_set_paragraph_shading(handle: u64, local_para_index: u32, rgb: u32, clear: u32) -> i32 {
    let v: Option<[u8; 4]> = if clear != 0 { None } else { Some([((rgb >> 16) & 0xff) as u8, ((rgb >> 8) & 0xff) as u8, (rgb & 0xff) as u8, 255]) };
    let para_index = local_para_index as usize;
    let result = handle::with_entry(handle, |entry| -> Result<(), String> {
        {
            let RoeDocument::Writer(doc) = &entry.doc else { return Err("非 Writer 文档".to_string()); };
            match doc.body.get(para_index) {
                Some(BodyElement::Paragraph(_)) => {}
                _ => return Err(format!("para_index={para_index} 越界或不是段落")),
            }
        }
        let prev = entry.doc.clone();
        let RoeDocument::Writer(doc) = &mut entry.doc else { unreachable!() };
        if let Some(BodyElement::Paragraph(p)) = doc.body.get_mut(para_index) {
            p.shading = v;
        }
        entry.push_undo_snapshot(prev, handle::EditCursorContext::at(para_index, 0));
        Ok(())
    });
    let ok = matches!(&result, Some(Ok(())));
    if ok { handle::invalidate_writer_layout_cache(handle); }
    match result {
        Some(Ok(())) => RoeErrorCode::Ok as i32,
        Some(Err(m)) => { set_last_error(format!("roe_set_paragraph_shading: {m}")); RoeErrorCode::ScriptError as i32 }
        None => { set_last_error("roe_set_paragraph_shading: 无效句柄"); RoeErrorCode::InvalidHandle as i32 }
    }
}


/// 段落间距。kind: 0=行距倍数 1=段前(pt) 2=段后(pt)；value<0 表示清除覆盖
#[no_mangle]
pub extern "C" fn roe_set_paragraph_spacing(handle: u64, local_para_index: u32, kind: u32, value: f32) -> i32 {
    if kind > 2 { set_last_error("roe_set_paragraph_spacing: kind 非法（合法值 0-2）"); return RoeErrorCode::ScriptError as i32; }
    if value >= 0.0 && (!value.is_finite() || value > 1000.0) { set_last_error("roe_set_paragraph_spacing: 数值非法"); return RoeErrorCode::ScriptError as i32; }
    if kind == 0 && value >= 0.0 && !(0.5..=10.0).contains(&value) { set_last_error("roe_set_paragraph_spacing: 行距倍数须在 0.5-10"); return RoeErrorCode::ScriptError as i32; }
    let v: Option<f32> = if value < 0.0 { None } else { Some(value) };
    let para_index = local_para_index as usize;
    let result = handle::with_entry(handle, |entry| -> Result<(), String> {
        {
            let RoeDocument::Writer(doc) = &entry.doc else { return Err("非 Writer 文档".to_string()); };
            match doc.body.get(para_index) {
                Some(BodyElement::Paragraph(_)) => {}
                _ => return Err(format!("para_index={para_index} 越界或不是段落")),
            }
        }
        let prev = entry.doc.clone();
        let RoeDocument::Writer(doc) = &mut entry.doc else { unreachable!() };
        if let Some(BodyElement::Paragraph(p)) = doc.body.get_mut(para_index) {
            match kind { 0 => p.line_height = v, 1 => p.space_before = v, _ => p.space_after = v }
        }
        entry.push_undo_snapshot(prev, handle::EditCursorContext::at(para_index, 0));
        Ok(())
    });
    let ok = matches!(&result, Some(Ok(())));
    if ok { handle::invalidate_writer_layout_cache(handle); }
    match result {
        Some(Ok(())) => RoeErrorCode::Ok as i32,
        Some(Err(m)) => { set_last_error(format!("roe_set_paragraph_spacing: {m}")); RoeErrorCode::ScriptError as i32 }
        None => { set_last_error("roe_set_paragraph_spacing: 无效句柄"); RoeErrorCode::InvalidHandle as i32 }
    }
}


// ───────── v238: 文字颜色 / 背景色 ─────────
fn rgb_to_color(rgb: u32) -> roe_model::common::Color {
    roe_model::common::Color { r: ((rgb >> 16) & 0xff) as u8, g: ((rgb >> 8) & 0xff) as u8, b: (rgb & 0xff) as u8, a: 255 }
}

/// 文字颜色。rgb=0xRRGGBB；clear!=0 表示清除覆盖（rgb 忽略）
#[no_mangle]
pub extern "C" fn roe_set_range_char_format_color(
    handle: u64, container_type: u32, container_id: u32,
    local_para_index: u32, start_char_offset: u32, end_char_offset: u32, rgb: u32, clear: u32,
) -> i32 {
    let v = if clear != 0 { None } else { Some(rgb_to_color(rgb)) };
    let patch = CharFormatPatch { color: Some(v), ..Default::default() };
    set_patch_range("roe_set_range_char_format_color", handle, container_type, container_id, local_para_index, start_char_offset, end_char_offset, patch)
}

/// 背景色（高亮）。语义同上
#[no_mangle]
pub extern "C" fn roe_set_range_char_format_background(
    handle: u64, container_type: u32, container_id: u32,
    local_para_index: u32, start_char_offset: u32, end_char_offset: u32, rgb: u32, clear: u32,
) -> i32 {
    let v = if clear != 0 { None } else { Some(rgb_to_color(rgb)) };
    let patch = CharFormatPatch { background: Some(v), ..Default::default() };
    set_patch_range("roe_set_range_char_format_background", handle, container_type, container_id, local_para_index, start_char_offset, end_char_offset, patch)
}


// ───────── v239: 清除全部字符内联格式（一次调用，一个撤销快照）─────────
#[no_mangle]
pub extern "C" fn roe_clear_range_char_format_all(
    handle: u64, container_type: u32, container_id: u32,
    local_para_index: u32, start_char_offset: u32, end_char_offset: u32,
) -> i32 {
    let patch = CharFormatPatch {
        bold: Some(None),
        italic: Some(None),
        underline: Some(None),
        underline_style: Some(None),
        font_size: Some(None),
        font_family: Some(None),
        font_family_ascii: Some(None),
        strikethrough: Some(None),
        script: Some(None),
        color: Some(None),
        background: Some(None),
    };
    set_patch_range("roe_clear_range_char_format_all", handle, container_type, container_id, local_para_index, start_char_offset, end_char_offset, patch)
}


// ───────── v240: 撤销分组（前台批量改多段前后各调用一次）─────────
#[no_mangle]
pub extern "C" fn roe_undo_group_begin() -> i32 {
    handle::undo_group_begin();
    0
}

#[no_mangle]
pub extern "C" fn roe_undo_group_end() -> i32 {
    handle::undo_group_end();
    0
}


// ───────── v242: 项目符号 / 编号 ─────────
fn roe_list_style(kind: u32, variant: u32) -> (String, roe_model::writer::ListStyle) {
    use roe_model::writer::{ListStyle, ListLevelStyle, NumberListLevel, BulletListLevel, ListIndent, NumFormat};
    let name = if kind == 1 { format!("ROE_Bullet_{variant}") } else { format!("ROE_Number_{variant}") };
    let mut levels = Vec::new();
    for lv in 0..9u32 {
        let indent = ListIndent { margin_left: 36.0 * (lv as f32 + 1.0), text_indent: -18.0, min_label_width: 18.0 };
        if kind == 1 {
            let ch = match variant { 1 => "●", 2 => "○", 3 => "■", 4 => "–", _ => "✓" };
            levels.push(ListLevelStyle::Bullet(BulletListLevel {
                bullet_char: ch.to_string(), prefix: String::new(), suffix: String::new(), indent,
            }));
        } else {
            let (format, suffix) = match variant {
                1 => (NumFormat::Arabic, "."),
                2 => (NumFormat::Arabic, ")"),
                3 => (NumFormat::LowerAlpha, "."),
                4 => (NumFormat::UpperAlpha, "."),
                5 => (NumFormat::LowerRoman, "."),
                _ => (NumFormat::UpperRoman, "."),
            };
            levels.push(ListLevelStyle::Number(NumberListLevel {
                format, prefix: String::new(), suffix: suffix.to_string(),
                start_value: 1, display_levels: 1, indent,
            }));
        }
    }
    let ls = ListStyle { name: name.clone(), levels };
    (name, ls)
}

/// 设置段落列表。kind: 0=取消 1=项目符号 2=编号；variant: 1..=6（kind=0 时忽略）
#[no_mangle]
pub extern "C" fn roe_set_paragraph_list(handle: u64, local_para_index: u32, kind: u32, variant: u32) -> i32 {
    if kind > 2 || (kind > 0 && (variant == 0 || variant > 6)) {
        set_last_error("roe_set_paragraph_list: 参数非法");
        return RoeErrorCode::ScriptError as i32;
    }
    let para_index = local_para_index as usize;
    let result = handle::with_entry(handle, |entry| -> Result<(), String> {
        let (existing, prev_info) = {
            let RoeDocument::Writer(doc) = &entry.doc else { return Err("非 Writer 文档".to_string()); };
            let existing = match doc.body.get(para_index) {
                Some(BodyElement::Paragraph(p)) => p.list_info.clone(),
                _ => return Err(format!("para_index={para_index} 越界或不是段落")),
            };
            let prev_info = if para_index > 0 {
                match doc.body.get(para_index - 1) {
                    Some(BodyElement::Paragraph(p)) => p.list_info.clone(),
                    _ => None,
                }
            } else { None };
            (existing, prev_info)
        };
        // v256: 导入的列表也允许修改；编号文字已烧进首个 run，修改时一并清掉
        let strip_imported = existing.as_ref().map_or(false, |li| !li.style_name.starts_with("ROE_"));
        if kind == 0 && existing.is_none() { return Ok(()); }
        let prev = entry.doc.clone();
        let RoeDocument::Writer(doc) = &mut entry.doc else { unreachable!() };
        let new_info = if kind == 0 {
            None
        } else {
            let (name, ls) = roe_list_style(kind, variant);
            doc.styles.list_styles.entry(name.clone()).or_insert(ls);
            let list_id = match &prev_info {
                Some(pi) if pi.style_name == name => pi.list_id.clone(),
                _ => {
                    let nanos = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
                    format!("ROE_L{nanos}")
                }
            };
            Some(roe_model::writer::ListInfo { list_id, level: 0, style_name: name })
        };
        if let Some(BodyElement::Paragraph(p)) = doc.body.get_mut(para_index) {
            p.list_info = new_info;
            if strip_imported {
                let drop_first = p.runs.first().map_or(false, |r| {
                    r.text.ends_with(' ') && !r.text.trim_end().contains(' ') && !r.text.trim().is_empty()
                });
                if drop_first { p.runs.remove(0); }
                p.indent_left = None;
                p.indent_first = None;
            }
        }
        entry.push_undo_snapshot(prev, handle::EditCursorContext::at(para_index, 0));
        Ok(())
    });
    let ok = matches!(&result, Some(Ok(())));
    if ok {
        handle::invalidate_writer_layout_cache(handle);
    }
    match result {
        Some(Ok(())) => RoeErrorCode::Ok as i32,
        Some(Err(m)) => { set_last_error(format!("roe_set_paragraph_list: {m}")); RoeErrorCode::ScriptError as i32 }
        None => { set_last_error("roe_set_paragraph_list: 无效句柄"); RoeErrorCode::InvalidHandle as i32 }
    }
}


// ───────── v244: 段落样式（正文 / 标题 1-4 / 标题 / 副标题）─────────
fn roe_builtin_style(kind: u32) -> Option<(&'static [&'static str], roe_model::style::ParagraphStyle)> {
    use roe_model::style::ParagraphStyle;
    use roe_model::common::Color;
    let grey = Color { r: 0x59, g: 0x59, b: 0x59, a: 255 };
    Some(match kind {
        0 => (&["Normal"], ParagraphStyle::default()),
        1 => (&["Heading1", "heading 1", "Heading 1"], ParagraphStyle { font_size: Some(16.0), bold: true, space_before: Some(12.0), space_after: Some(6.0), ..Default::default() }),
        2 => (&["Heading2", "heading 2", "Heading 2"], ParagraphStyle { font_size: Some(14.0), bold: true, space_before: Some(10.0), space_after: Some(4.0), ..Default::default() }),
        3 => (&["Heading3", "heading 3", "Heading 3"], ParagraphStyle { font_size: Some(13.0), bold: true, space_before: Some(8.0), space_after: Some(4.0), ..Default::default() }),
        4 => (&["Heading4", "heading 4", "Heading 4"], ParagraphStyle { font_size: Some(12.0), bold: true, space_before: Some(6.0), space_after: Some(3.0), ..Default::default() }),
        5 => (&["Title", "title"], ParagraphStyle { font_size: Some(22.0), bold: true, space_after: Some(8.0), ..Default::default() }),
        6 => (&["Subtitle", "subtitle"], ParagraphStyle { font_size: Some(12.0), italic: true, color: Some(grey), space_after: Some(6.0), ..Default::default() }),
        _ => return None,
    })
}

/// 设置段落样式。kind: 0=正文 1-4=标题1-4 5=标题 6=副标题
#[no_mangle]
pub extern "C" fn roe_set_paragraph_style(handle: u64, local_para_index: u32, kind: u32) -> i32 {
    let Some((cands, mut def)) = roe_builtin_style(kind) else {
        set_last_error("roe_set_paragraph_style: 参数非法（合法值 0-6）");
        return RoeErrorCode::ScriptError as i32;
    };
    let para_index = local_para_index as usize;
    let result = handle::with_entry(handle, |entry| -> Result<(), String> {
        let cur = {
            let RoeDocument::Writer(doc) = &entry.doc else { return Err("非 Writer 文档".to_string()); };
            match doc.body.get(para_index) {
                Some(BodyElement::Paragraph(p)) => p.style.0.clone(),
                _ => return Err(format!("para_index={para_index} 越界或不是段落")),
            }
        };
        let key = {
            let RoeDocument::Writer(doc) = &entry.doc else { unreachable!() };
            cands.iter().find(|c| doc.styles.paragraph_styles.contains_key(**c)).map(|c| c.to_string())
        };
        let key_final = key.clone().unwrap_or_else(|| cands[0].to_string());
        if cur == key_final { return Ok(()); }
        let prev = entry.doc.clone();
        let RoeDocument::Writer(doc) = &mut entry.doc else { unreachable!() };
        if key.is_none() && kind != 0 {
            def.name = key_final.clone();
            doc.styles.paragraph_styles.insert(key_final.clone(), def);
        }
        if let Some(BodyElement::Paragraph(p)) = doc.body.get_mut(para_index) {
            p.style = roe_model::common::StyleRef(key_final);
        }
        entry.push_undo_snapshot(prev, handle::EditCursorContext::at(para_index, 0));
        Ok(())
    });
    let ok = matches!(&result, Some(Ok(())));
    if ok { handle::invalidate_writer_layout_cache(handle); }
    match result {
        Some(Ok(())) => RoeErrorCode::Ok as i32,
        Some(Err(m)) => { set_last_error(format!("roe_set_paragraph_style: {m}")); RoeErrorCode::ScriptError as i32 }
        None => { set_last_error("roe_set_paragraph_style: 无效句柄"); RoeErrorCode::InvalidHandle as i32 }
    }
}


// ───────── v253: 缩进 / 列表级别 ─────────
/// delta=+1 增加缩进，-1 减少缩进。ROE_ 列表项调整级别(0..8)；普通段落左缩进 ±36pt（最小0）。
#[no_mangle]
pub extern "C" fn roe_change_list_level(handle: u64, local_para_index: u32, delta: i32) -> i32 {
    let para_index = local_para_index as usize;
    let result = handle::with_entry(handle, |entry| -> Result<(), String> {
        {
            let RoeDocument::Writer(doc) = &entry.doc else { return Err("非 Writer 文档".to_string()); };
            match doc.body.get(para_index) {
                Some(BodyElement::Paragraph(_)) => {}
                _ => return Err(format!("para_index={para_index} 越界或不是段落")),
            }
        }
        let prev = entry.doc.clone();
        let RoeDocument::Writer(doc) = &mut entry.doc else { unreachable!() };
        if let Some(BodyElement::Paragraph(p)) = doc.body.get_mut(para_index) {
            let imported = p.list_info.as_ref().map_or(false, |li| !li.style_name.starts_with("ROE_"));
            if imported {
                // v256: 导入列表保留编号，只整体调左缩进
                let cur = p.indent_left.unwrap_or(0.0);
                p.indent_left = Some((cur + 36.0 * delta as f32).max(0.0));
            } else if let Some(li) = p.list_info.as_mut() {
                let cur = li.level as i32;
                let nl = (cur + delta).clamp(0, 8);
                li.level = nl as _;
            } else {
                let cur = p.indent_left.unwrap_or(0.0);
                let nv = (cur + 36.0 * delta as f32).max(0.0);
                p.indent_left = if nv <= 0.0 { None } else { Some(nv) };
            }
        }
        entry.push_undo_snapshot(prev, handle::EditCursorContext::at(para_index, 0));
        Ok(())
    });
    if matches!(&result, Some(Ok(()))) {
        handle::invalidate_writer_layout_cache(handle);
    }
    match result {
        Some(Ok(())) => RoeErrorCode::Ok as i32,
        Some(Err(m)) => { set_last_error(format!("roe_change_list_level: {m}")); RoeErrorCode::ScriptError as i32 }
        None => { set_last_error("roe_change_list_level: 无效句柄"); RoeErrorCode::InvalidHandle as i32 }
    }
}
