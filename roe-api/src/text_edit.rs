//! roe_insert_char —— 光标处插入单个字符（v100 最小切面）。
//!
//! 范围（有意收窄，见 ROE_Progress_v99.md「四、下次开工建议起点」）：
//! - 仅支持 container_type=BODY（表格cell/文本框预留未实现，同
//!   cursor_inline.rs 的既有限制）。
//! - 仅插入单个字符（Unicode 码点），不支持批量插入字符串、不支持
//!   删除、不含 IME 组字。
//! - 不处理跨 run 插入的复杂情况之外的其他特殊情况（列表编号、表格、
//!   分栏等），这些场景本次不保证正确，留给后续排期（v99 排期第4项）。
//!
//! 事务处理：本函数内部自行完成"克隆快照 -> 修改 -> 推入 undo_stack ->
//! 清空 redo_stack"这套等价于 roe_begin_edit+roe_commit_edit 的流程，
//! 不直接调用这两个导出函数——因为它们各自会独立调用 handle::with_entry，
//! 嵌套调用会导致同一 handle 的 RefCell 重复借用而 panic（snapshot.rs
//! 注释里反复强调的坑）。所以这里手写一遍等价逻辑，全部在同一个
//! with_entry 闭包内完成。
//!
//! char_offset 语义：与 cursor_inline.rs / cursor_para_info.rs 一致，
//! 是"段落 full_text"（Paragraph.runs 里所有 Run.text 拼接后）的全局
//! 字节偏移，不是单个 run 内部的偏移。插入时需要遍历 runs 按每个
//! run.text.len() 累加，定位 char_offset 落在哪个 run 内部的哪个字节
//! 位置。

use std::ffi::CString;
use std::os::raw::c_char;

use roe_model::writer::BodyElement;
use roe_parser::RoeDocument;

use crate::cursor::RoeLocation;
use crate::error::set_last_error;
use crate::handle;

/// 在光标位置插入一个字符。
///
/// 参数：
/// - handle：文档句柄
/// - container_type/container_id/local_para_index/char_offset：拆解后
///   的 RoeLocation（当前位置，即插入发生前的光标位置）
/// - ch：待插入字符的 Unicode 码点（比如 ASCII 'a' 是 97）
///
/// 返回：JSON 字符串
/// `{"container_type":0,"container_id":0,"local_para_index":N,"char_offset":N}`
/// 即插入完成后光标应移动到的新位置（原 char_offset + 新插入字符的
/// UTF-8 字节长度）。
///
/// 失败返回 null，调用 roe_last_error_message 获取详情。失败场景：
/// - 非 BODY container_type
/// - 无效句柄或非 Writer 文档
/// - para_index 越界
/// - char_offset 不在任何 run 的字符边界上（不应发生，除非调用方传入
///   了非法偏移；用 char_boundary 校验防止 String::insert 内部 panic）
/// - ch 不是合法 Unicode 码点
///
/// 调用方必须用 roe_free_string 释放返回值。
#[no_mangle]
pub extern "C" fn roe_insert_char(
    handle: u64,
    container_type: u32,
    container_id: u32,
    local_para_index: u32,
    char_offset: u32,
    ch: u32,
) -> *mut c_char {
    let loc = RoeLocation { container_type, container_id, local_para_index, char_offset };

    if !loc.is_body() {
        set_last_error("roe_insert_char: 本次仅支持 container_type=BODY（表格cell/文本框预留未实现）");
        return std::ptr::null_mut();
    }

    let Some(inserted_char) = char::from_u32(ch) else {
        set_last_error(format!("roe_insert_char: 非法 Unicode 码点 {ch}"));
        return std::ptr::null_mut();
    };

    let (para_index, char_offset) = loc.as_legacy_body();

    let result = handle::with_entry(handle, |entry| {
        insert_char_in_entry(entry, para_index, char_offset, inserted_char)
    });

    let is_writer_ok = matches!(&result, Some(Ok(_)));

    // 必须在 with_entry 闭包返回之后调用 invalidate_writer_layout_cache，
    // 避免同一 handle 的 RefCell 嵌套借用 panic（cursor_inline.rs /
    // snapshot.rs 反复强调的同一个坑）。
    if is_writer_ok {
        handle::invalidate_writer_layout_cache(handle);
    }

    match result {
        None => {
            set_last_error("roe_insert_char: 无效句柄");
            std::ptr::null_mut()
        }
        Some(Err(msg)) => {
            set_last_error(format!("roe_insert_char: {msg}"));
            std::ptr::null_mut()
        }
        Some(Ok(json)) => match CString::new(json) {
            Ok(cs) => cs.into_raw(),
            Err(e) => {
                set_last_error(format!("roe_insert_char: {e}"));
                std::ptr::null_mut()
            }
        },
    }
}

/// 核心逻辑：定位 char_offset 落在 para.runs 的哪个 run 内部哪个字节
/// 位置，插入字符，返回新的 (para_index, char_offset) JSON。
///
/// 事务处理在这里一并完成：修改前先克隆快照存入 entry.snapshot（等价
/// roe_begin_edit），修改后把快照推入 undo_stack、清空 redo_stack
/// （等价 roe_commit_edit 的 undo/redo 栈维护部分）。与 roe_commit_edit
/// 的差异：这里不检查 entry.snapshot 是否已存在——每次插入字符都是一次
/// 独立可撤销操作（一次 roe_undo 撤销一个字符），不依赖调用方先手动
/// begin_edit。如果调用方在外层已经开了 begin_edit（entry.snapshot
/// 是 Some），这里会覆盖掉那个快照——当前设计下 roe_insert_char 不
/// 支持嵌套在调用方自己的编辑事务内，这是本次已知的范围限制，未来如
/// 果需要"多次插入合并成一次撤销"（比如连续打字合并成一个 undo 步骤）
/// 需要重新设计，本次不做。
fn insert_char_in_entry(
    entry: &mut handle::DocumentEntry,
    para_index: usize,
    char_offset: usize,
    ch: char,
) -> Result<String, String> {
    let RoeDocument::Writer(doc) = &entry.doc else {
        return Err("非 Writer 文档".to_string());
    };

    // 先只读校验一遍能不能插入（定位 run + 边界检查），校验通过后再
    // clone 快照 + 实际修改，避免"校验失败也 clone 了一次快照"的浪费。
    let para = match doc.body.get(para_index) {
        Some(BodyElement::Paragraph(p)) => p,
        _ => return Err(format!("para_index={para_index} 越界或不存在")),
    };

    let mut consumed = 0usize;
    let mut target_run: Option<(usize, usize)> = None; // (run_index, offset_in_run)
    for (i, run) in para.runs.iter().enumerate() {
        let run_len = run.text.len();
        if char_offset <= consumed + run_len {
            let offset_in_run = char_offset - consumed;
            if !run.text.is_char_boundary(offset_in_run) {
                return Err(format!(
                    "char_offset={char_offset} 不在 run[{i}] 的字符边界上（内部字节偏移={offset_in_run}）"
                ));
            }
            target_run = Some((i, offset_in_run));
            break;
        }
        consumed += run_len;
    }

    let Some((run_index, offset_in_run)) = target_run else {
        return Err(format!(
            "char_offset={char_offset} 超出段落范围（段落总字节长度={consumed}）"
        ));
    };

    // 校验通过，现在真正开始修改：克隆快照（等价 roe_begin_edit）。
    entry.snapshot = Some(entry.doc.clone());

    let RoeDocument::Writer(doc) = &mut entry.doc else {
        unreachable!("上面已确认是 Writer 文档");
    };
    let Some(BodyElement::Paragraph(para)) = doc.body.get_mut(para_index) else {
        unreachable!("上面已确认 para_index 有效");
    };
    let run = &mut para.runs[run_index];
    run.text.insert(offset_in_run, ch);

    let ch_len = ch.len_utf8();
    let new_char_offset = char_offset + ch_len;

    // 等价 roe_commit_edit：快照进 undo_stack，redo_stack 清空。
    if let Some(prev) = entry.snapshot.take() {
        let ctx = handle::EditCursorContext::at(para_index, char_offset);
        entry.push_undo_snapshot(prev, ctx);
    }

    Ok(format!(
        "{{\"container_type\":0,\"container_id\":0,\"local_para_index\":{},\"char_offset\":{}}}",
        para_index, new_char_offset
    ))
}

/// 在光标处批量插入一段文本（v101新增，供CJK输入法组合完成后一次性提交
/// 多字符使用，比如"你好"两个字）。与 roe_insert_char 并存，不是后者的
/// 替换——roe_insert_char 已有稳定测试覆盖，英文逐字符输入路径
/// （on_writer_key_input）工作正常，无需为复用而牵动已验证代码。
///
/// 与 roe_insert_char 的核心差异：整段文本一次性插入到目标run，只做一次
/// clone快照+一次push undo_stack，即"一次组合完成=一次可撤销操作"，不会
/// 像逐字符调用 roe_insert_char 那样把一次中文组合拆成N个undo步骤。
///
/// 参数：
/// - handle：文档句柄
/// - container_type/container_id/local_para_index/char_offset：拆解后
///   的 RoeLocation（插入发生前的光标位置）
/// - text_utf8：待插入文本的UTF-8字节，配合 text_len 使用（不依赖C字符串
///   null结尾，因为文本内容本身可能合法包含'\0'——虽然实际CJK场景不会，
///   但接口设计上不应该依赖这个假设）
/// - text_len：text_utf8 的字节长度
///
/// 返回：JSON字符串，同 roe_insert_char 的返回格式，char_offset为插入完
/// 成后光标应移动到的新位置（原offset + 插入文本的UTF-8字节总长度）。
///
/// 失败返回 null，调用 roe_last_error_message 获取详情。失败场景同
/// roe_insert_char，另加：text_utf8 不是合法UTF-8、text_len为0（空文本
/// 插入没有意义，视为调用方错误而非静默成功）。
///
/// 调用方必须用 roe_free_string 释放返回值。
#[no_mangle]
pub extern "C" fn roe_insert_text(
    handle: u64,
    container_type: u32,
    container_id: u32,
    local_para_index: u32,
    char_offset: u32,
    text_utf8: *const u8,
    text_len: u32,
) -> *mut c_char {
    let loc = RoeLocation { container_type, container_id, local_para_index, char_offset };

    if !loc.is_body() {
        set_last_error("roe_insert_text: 本次仅支持 container_type=BODY（表格cell/文本框预留未实现）");
        return std::ptr::null_mut();
    }

    if text_len == 0 {
        set_last_error("roe_insert_text: text_len 为 0，空文本插入没有意义");
        return std::ptr::null_mut();
    }

    if text_utf8.is_null() {
        set_last_error("roe_insert_text: text_utf8 为空指针");
        return std::ptr::null_mut();
    }

    let bytes = unsafe { std::slice::from_raw_parts(text_utf8, text_len as usize) };
    let inserted_text = match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(e) => {
            set_last_error(format!("roe_insert_text: text_utf8 不是合法UTF-8: {e}"));
            return std::ptr::null_mut();
        }
    };

    let (para_index, char_offset) = loc.as_legacy_body();

    let result = handle::with_entry(handle, |entry| {
        insert_text_in_entry(entry, para_index, char_offset, &inserted_text)
    });

    let is_writer_ok = matches!(&result, Some(Ok(_)));

    if is_writer_ok {
        handle::invalidate_writer_layout_cache(handle);
    }

    match result {
        None => {
            set_last_error("roe_insert_text: 无效句柄");
            std::ptr::null_mut()
        }
        Some(Err(msg)) => {
            set_last_error(format!("roe_insert_text: {msg}"));
            std::ptr::null_mut()
        }
        Some(Ok(json)) => match CString::new(json) {
            Ok(cs) => cs.into_raw(),
            Err(e) => {
                set_last_error(format!("roe_insert_text: {e}"));
                std::ptr::null_mut()
            }
        },
    }
}

/// 核心逻辑：与 insert_char_in_entry 同构，差异仅在于一次性 insert_str
/// 整段文本而不是单个char，且新光标偏移量按插入文本的UTF-8字节总长度
/// 计算（而不是单个char.len_utf8()）。
fn insert_text_in_entry(
    entry: &mut handle::DocumentEntry,
    para_index: usize,
    char_offset: usize,
    text: &str,
) -> Result<String, String> {
    let RoeDocument::Writer(doc) = &entry.doc else {
        return Err("非 Writer 文档".to_string());
    };

    let para = match doc.body.get(para_index) {
        Some(BodyElement::Paragraph(p)) => p,
        _ => return Err(format!("para_index={para_index} 越界或不存在")),
    };

    // v115 修复：空段落（para.runs 为空，通常是被 delete_char 删空到最后
    // 一个字符后触发了空 run 清理逻辑）特判。原逻辑遍历 para.runs 定位
    // 目标 run，空 vec 时循环一次都不执行，target_run 恒为 None，导致
    // "段首/空段落无法打字" bug（v113/v114 文档记录的已知问题）。
    // 空段落时唯一合法的 char_offset 是 0（没有任何 run 可供其它偏移量
    // 落入），直接新建一个 Default 样式的 run 并 push，不走"定位到已有
    // run 内部偏移"这条路径——段落级样式在 para.style 上，run 不需要
    // 显式继承什么，用 Default 是唯一合理选择（Run 已实现 Default，
    // 项目里大量代码这样构造，如 roe-api/src/document.rs）。
    if para.runs.is_empty() {
        if char_offset != 0 {
            return Err(format!(
                "char_offset={char_offset} 超出段落范围（空段落，仅支持 char_offset=0）"
            ));
        }

        entry.snapshot = Some(entry.doc.clone());

        let RoeDocument::Writer(doc) = &mut entry.doc else {
            unreachable!("上面已确认是 Writer 文档");
        };
        let Some(BodyElement::Paragraph(para)) = doc.body.get_mut(para_index) else {
            unreachable!("上面已确认 para_index 有效");
        };
        para.runs.push(roe_model::writer::Run {
            text: text.to_string(),
            ..Default::default()
        });

        let new_char_offset = text.len();

        if let Some(prev) = entry.snapshot.take() {
            let ctx = handle::EditCursorContext::at(para_index, char_offset);
            entry.push_undo_snapshot(prev, ctx);
        }

        return Ok(format!(
            "{{\"container_type\":0,\"container_id\":0,\"local_para_index\":{},\"char_offset\":{}}}",
            para_index, new_char_offset
        ));
    }

    let mut consumed = 0usize;
    let mut target_run: Option<(usize, usize)> = None;
    for (i, run) in para.runs.iter().enumerate() {
        let run_len = run.text.len();
        if char_offset <= consumed + run_len {
            let offset_in_run = char_offset - consumed;
            if !run.text.is_char_boundary(offset_in_run) {
                return Err(format!(
                    "char_offset={char_offset} 不在 run[{i}] 的字符边界上（内部字节偏移={offset_in_run}）"
                ));
            }
            target_run = Some((i, offset_in_run));
            break;
        }
        consumed += run_len;
    }

    let Some((run_index, offset_in_run)) = target_run else {
        return Err(format!(
            "char_offset={char_offset} 超出段落范围（段落总字节长度={consumed}）"
        ));
    };

    entry.snapshot = Some(entry.doc.clone());

    let RoeDocument::Writer(doc) = &mut entry.doc else {
        unreachable!("上面已确认是 Writer 文档");
    };
    let Some(BodyElement::Paragraph(para)) = doc.body.get_mut(para_index) else {
        unreachable!("上面已确认 para_index 有效");
    };
    let run = &mut para.runs[run_index];
    run.text.insert_str(offset_in_run, text);

    let text_len = text.len();
    let new_char_offset = char_offset + text_len;

    if let Some(prev) = entry.snapshot.take() {
        let ctx = handle::EditCursorContext::at(para_index, char_offset);
        entry.push_undo_snapshot(prev, ctx);
    }

    Ok(format!(
        "{{\"container_type\":0,\"container_id\":0,\"local_para_index\":{},\"char_offset\":{}}}",
        para_index, new_char_offset
    ))
}

/// 删除方向：Backspace（删除 char_offset 之前一个字符）或
/// Delete（删除 char_offset 处/之后一个字符）。
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteDirection {
    /// 退格：删除光标前一个字符，光标左移一个字符位置。
    Backward = 0,
    /// 删除键：删除光标后一个字符（当前位置的字符），光标位置不变。
    Forward = 1,
}

impl DeleteDirection {
    fn from_u32(v: u32) -> Option<Self> {
        match v {
            0 => Some(Self::Backward),
            1 => Some(Self::Forward),
            _ => None,
        }
    }
}

/// 在光标位置删除一个字符（v100 最小切面，续插入功能之后的对称操作）。
///
/// 参数：
/// - handle：文档句柄
/// - container_type/container_id/local_para_index/char_offset：拆解后
///   的 RoeLocation（当前光标位置，删除发生前）
/// - direction：DeleteDirection 的 u32 编码（0=Backward退格，1=Forward删除键）
///
/// 返回：JSON 字符串
/// `{"container_type":0,"container_id":0,"local_para_index":N,"char_offset":N}`
/// 即删除完成后光标应移动到的新位置：
/// - Backward：新 offset = 原 offset - 被删字符的字节长度
/// - Forward：新 offset = 原 offset（不变，删的是后面的字符）
///
/// 边界行为（本次范围限制，同 insert_char 一致的"不跨段"原则，跨段合并
/// 段落属于更复杂的操作，本次不做）：
/// - Backward 在段落开头（offset=0）：无字符可删，返回错误（不是"原地
///   不动"的静默成功，因为调用方前端应该在检测到 offset=0 时自行决定
///   要不要触发"合并到上一段"这类跨段行为，本次不提供该行为，明确报错
///   让前端知道"这里没有更多可删的了"）
/// - Forward 在段落末尾（offset=段落总长度）：同理，无字符可删，返回错误
///
/// 删除后如果某个 run 被删空（text变成空字符串），会顺便从 para.runs 中
/// 移除这个空 run（保持数据干净，避免空 run 在渲染层被当正常 run 反复
/// 处理，累积成技术债）。
///
/// 失败返回 null，调用 roe_last_error_message 获取详情。
/// 调用方必须用 roe_free_string 释放返回值。
#[no_mangle]
pub extern "C" fn roe_delete_char(
    handle: u64,
    container_type: u32,
    container_id: u32,
    local_para_index: u32,
    char_offset: u32,
    direction: u32,
) -> *mut c_char {
    let loc = RoeLocation { container_type, container_id, local_para_index, char_offset };

    if !loc.is_body() {
        set_last_error("roe_delete_char: 本次仅支持 container_type=BODY（表格cell/文本框预留未实现）");
        return std::ptr::null_mut();
    }

    let Some(dir) = DeleteDirection::from_u32(direction) else {
        set_last_error(format!("roe_delete_char: 非法 direction 值 {direction}"));
        return std::ptr::null_mut();
    };

    let (para_index, char_offset) = loc.as_legacy_body();

    let result = handle::with_entry(handle, |entry| {
        delete_char_in_entry(entry, para_index, char_offset, dir)
    });

    let is_writer_ok = matches!(&result, Some(Ok(_)));

    // 必须在 with_entry 闭包返回之后调用，避免同一 handle 的 RefCell
    // 嵌套借用 panic（与 roe_insert_char 相同的坑）。
    if is_writer_ok {
        handle::invalidate_writer_layout_cache(handle);
    }

    match result {
        None => {
            set_last_error("roe_delete_char: 无效句柄");
            std::ptr::null_mut()
        }
        Some(Err(msg)) => {
            set_last_error(format!("roe_delete_char: {msg}"));
            std::ptr::null_mut()
        }
        Some(Ok(json)) => match CString::new(json) {
            Ok(cs) => cs.into_raw(),
            Err(e) => {
                set_last_error(format!("roe_delete_char: {e}"));
                std::ptr::null_mut()
            }
        },
    }
}

/// 核心逻辑：定位待删除字符所在的 run + 字节范围，删除后视情况移除空 run，
/// 返回新的 (para_index, char_offset) JSON。
///
/// 事务处理与 insert_char_in_entry 完全同构：先只读校验能否删除，校验
/// 通过后才 clone 快照 + 实际修改 + 推入 undo_stack + 清空 redo_stack。
fn delete_char_in_entry(
    entry: &mut handle::DocumentEntry,
    para_index: usize,
    char_offset: usize,
    direction: DeleteDirection,
) -> Result<String, String> {
    let RoeDocument::Writer(doc) = &entry.doc else {
        return Err("非 Writer 文档".to_string());
    };

    let para = match doc.body.get(para_index) {
        Some(BodyElement::Paragraph(p)) => p,
        _ => return Err(format!("para_index={para_index} 越界或不存在")),
    };

    // 段落 full_text 总字节长度（所有 run.text 拼接后）。
    let para_len: usize = para.runs.iter().map(|r| r.text.len()).sum();

    // 根据方向确定"待删除字符"在段落 full_text 中的字节范围
    // [del_start, del_end)。用 Run 的边界信息定位该字符跨越的字节数，
    // 因为要正确处理多字节 UTF-8 字符，不能假设删1字节。
    let (del_start, del_end) = match direction {
        DeleteDirection::Backward => {
            if char_offset == 0 {
                return Err("段落开头，退格无字符可删（本次不支持跨段合并）".to_string());
            }
            // v101 修复：locate_run 是"定位 char_offset 落在哪个 run 内部"，
            // 当 char_offset 恰好落在两个 run 的交界处时（比如上一个 run 是
            // "Hello"，下一个 run 是标点符号"("自成一个 run，char_offset=5
            // 正好是边界），locate_run 会按其自身语义把边界值归到"下一个
            // run 的开头"（offset_in_run=0），而不是"上一个 run 的末尾"。
            // 这对 Forward（向后删）是对的语义（要删的字符本来就在下一个
            // run 开头），但 Backward（向前删）需要的是边界*前面*那个
            // run，locate_run 返回的 run 在这种情况下是错的——
            // run_text[..0] 是空串，找不到"上一个字符起点"，导致原来的
            // 代码在这里报错（表现为退格卡住，且标点符号因为经常独占一个
            // run 而格外容易触发）。
            //
            // 修复：不复用 locate_run，直接在这里手写"从 full_text 尾部
            // 视角找上一个字符边界"的逻辑——按 run 顺序累加，只要某个 run
            // 的结束位置 >= char_offset 且该 run 非空，就在该 run 内找
            // char_offset 之前的最后一个字符起点；如果 char_offset 落在
            // 某个 run 的开头（offset_in_run==0），说明要删的字符在*上一个*
            // 非空 run 的末尾，改为在那个 run 里找最后一个字符起点。
            let mut consumed = 0usize;
            let mut found: Option<(usize, usize)> = None; // (run_index, prev_char_start_in_run)
            for (i, run) in para.runs.iter().enumerate() {
                let run_len = run.text.len();
                let run_start = consumed;
                let run_end = consumed + run_len;
                if char_offset > run_start && char_offset <= run_end {
                    // char_offset 落在本 run 内部（不含开头边界）：
                    // 直接在本 run 里找 char_offset 之前的最后一个字符。
                    let offset_in_run = char_offset - run_start;
                    if !run.text.is_char_boundary(offset_in_run) {
                        return Err(format!(
                            "char_offset={char_offset} 不在 run[{i}] 的字符边界上"
                        ));
                    }
                    let prev_char_start_in_run = run.text[..offset_in_run]
                        .char_indices()
                        .next_back()
                        .map(|(idx, _)| idx)
                        .ok_or_else(|| {
                            format!("run[{i}] 在 offset_in_run={offset_in_run} 之前无字符（内部不一致）")
                        })?;
                    found = Some((i, prev_char_start_in_run));
                    break;
                }
                consumed += run_len;
            }
            let (run_index, prev_char_start_in_run) = found.ok_or_else(|| {
                format!("char_offset={char_offset} 找不到可退格删除的字符（段落总字节长度={consumed}，可能落在空run边界或内部状态不一致）")
            })?;
            let consumed_before_run: usize = para.runs[..run_index].iter().map(|r| r.text.len()).sum();
            let start = consumed_before_run + prev_char_start_in_run;
            (start, char_offset)
        }
        DeleteDirection::Forward => {
            if char_offset >= para_len {
                return Err("段落末尾，Delete键无字符可删（本次不支持跨段合并）".to_string());
            }
            let (run_index, offset_in_run) = locate_run(para, char_offset)?;
            let run_text = &para.runs[run_index].text;
            if !run_text.is_char_boundary(offset_in_run) {
                return Err(format!(
                    "char_offset={char_offset} 不在 run[{run_index}] 的字符边界上"
                ));
            }
            // run_text[offset_in_run..] 的第一个字符的结束位置。
            let mut chars = run_text[offset_in_run..].char_indices();
            let (_, first_char) = chars.next().ok_or_else(|| {
                format!("run[{run_index}] 在 offset_in_run={offset_in_run} 之后无字符（内部不一致）")
            })?;
            let char_len = first_char.len_utf8();
            (char_offset, char_offset + char_len)
        }
    };

    // 校验通过，开始实际修改：克隆快照（等价 roe_begin_edit）。
    entry.snapshot = Some(entry.doc.clone());

    let RoeDocument::Writer(doc) = &mut entry.doc else {
        unreachable!("上面已确认是 Writer 文档");
    };
    let Some(BodyElement::Paragraph(para)) = doc.body.get_mut(para_index) else {
        unreachable!("上面已确认 para_index 有效");
    };

    // del_start/del_end 是段落级全局字节偏移，可能跨越单个 run（理论上
    // 一个字符不会跨 run，因为 run 边界通常在字符边界上，但为稳妥起见
    // 仍按"删除范围可能touch到的run"处理，而不是假设一定落在同一个run —
    // 实际上由于上面 locate_run 是按字符边界定位的，del_start/del_end
    // 必然落在同一个 run 内，这里用简单遍历实现，不特殊优化。
    let mut consumed = 0usize;
    let mut removed_run_index: Option<usize> = None;
    for (i, run) in para.runs.iter_mut().enumerate() {
        let run_len = run.text.len();
        let run_start = consumed;
        let run_end = consumed + run_len;
        if del_start >= run_start && del_end <= run_end {
            let local_start = del_start - run_start;
            let local_end = del_end - run_start;
            run.text.replace_range(local_start..local_end, "");
            if run.text.is_empty() {
                removed_run_index = Some(i);
            }
            break;
        }
        consumed += run_len;
    }

    // 顺手移除被删空的 run（保持数据干净，避免空 run 技术债累积）。
    if let Some(i) = removed_run_index {
        para.runs.remove(i);
    }

    let new_char_offset = match direction {
        DeleteDirection::Backward => del_start,
        DeleteDirection::Forward => char_offset,
    };

    // 等价 roe_commit_edit：快照进 undo_stack，redo_stack 清空。
    if let Some(prev) = entry.snapshot.take() {
        let ctx = handle::EditCursorContext::at(para_index, char_offset);
        entry.push_undo_snapshot(prev, ctx);
    }

    Ok(format!(
        "{{\"container_type\":0,\"container_id\":0,\"local_para_index\":{},\"char_offset\":{}}}",
        para_index, new_char_offset
    ))
}

/// 辅助：定位 char_offset（段落级全局字节偏移）落在 para.runs 的哪个
/// run 内部哪个字节位置。与 insert_char_in_entry 里的定位循环逻辑
/// 相同（重复写一份而不是提取公共函数，是因为 insert 允许 char_offset
/// 落在段落末尾"之后一位"即 consumed==char_offset 的情况（插入点可以
/// 在段落最末尾），delete 的语义不同——删除操作的 char_offset 必须
/// 落在某个实际存在的字符位置上，提取公共函数反而会让两种不同语义
/// 混在一起，不如分开写更清楚）。
fn locate_run(para: &roe_model::writer::Paragraph, char_offset: usize) -> Result<(usize, usize), String> {
    let mut consumed = 0usize;
    let run_count = para.runs.len();
    for (i, run) in para.runs.iter().enumerate() {
        let run_len = run.text.len();
        // 边界修复（v100）：char_offset 落在"段落总长度"这个边界值上时
        // （即光标停在段尾），必须能定位到最后一个 run 的末尾位置——
        // 这是 Backward（退格）删除最后一个字符的合法场景。原来用严格
        // 小于 `<` 会漏掉这个边界，导致段尾退格误报"超出段落范围"。
        // 用 `<=` 但只在"最后一个 run"时才允许 offset_in_run == run_len，
        // 中间的 run 仍然用严格小于，避免把本该属于下一个 run 开头
        // （offset_in_run=0）的位置错误地定位成上一个 run 的末尾
        // （这两种定位理论上指向 full_text 中同一个字节偏移，但归属的
        // run_index 不同，对后续按 run_index 做 replace_range 操作而言
        // 必须落在正确的 run 上，不能含糊）。
        let is_last_run = i == run_count - 1;
        if char_offset < consumed + run_len || (is_last_run && char_offset == consumed + run_len) {
            return Ok((i, char_offset - consumed));
        }
        consumed += run_len;
    }
    Err(format!(
        "char_offset={char_offset} 超出段落范围（段落总字节长度={consumed}）"
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

    fn get_para_text(h: u64, para_index: usize) -> String {
        handle::with_entry(h, |entry| {
            let RoeDocument::Writer(doc) = &entry.doc else { unreachable!() };
            let Some(BodyElement::Paragraph(p)) = doc.body.get(para_index) else { unreachable!() };
            p.runs.iter().map(|r| r.text.as_str()).collect::<String>()
        })
        .unwrap()
    }

    fn parse_char_offset(json: &str) -> usize {
        let key = "\"char_offset\":";
        let start = json.find(key).expect("json 应含 char_offset 字段") + key.len();
        let rest = &json[start..];
        let end = rest.find('}').unwrap_or(rest.len());
        rest[..end].trim().parse().expect("char_offset 应为数字")
    }

    #[test]
    fn test_insert_ascii_char_at_start() {
        let h = open_writer();
        // "Hello World" 开头插入 'X' -> "XHello World"
        let ptr = roe_insert_char(h, 0, 0, 0, 0, 'X' as u32);
        assert!(!ptr.is_null(), "插入应该成功");
        let s = unsafe { CString::from_raw(ptr) }.into_string().unwrap();
        let offset = parse_char_offset(&s);
        assert_eq!(offset, 1, "插入1字节ASCII字符后偏移应+1，实际 {offset}");
        assert_eq!(get_para_text(h, 0), "XHello World");
        handle::remove(h);
    }

    #[test]
    fn test_insert_ascii_char_in_middle() {
        let h = open_writer();
        // "Hello World" offset=5 (空格前) 插入 '!' -> "Hello! World"
        let ptr = roe_insert_char(h, 0, 0, 0, 5, '!' as u32);
        assert!(!ptr.is_null());
        let s = unsafe { CString::from_raw(ptr) }.into_string().unwrap();
        let offset = parse_char_offset(&s);
        assert_eq!(offset, 6);
        assert_eq!(get_para_text(h, 0), "Hello! World");
        handle::remove(h);
    }

    #[test]
    fn test_insert_ascii_char_at_end() {
        let h = open_writer();
        // "Hello World" 长度11，末尾插入 '!' -> "Hello World!"
        let ptr = roe_insert_char(h, 0, 0, 0, 11, '!' as u32);
        assert!(!ptr.is_null());
        let s = unsafe { CString::from_raw(ptr) }.into_string().unwrap();
        let offset = parse_char_offset(&s);
        assert_eq!(offset, 12);
        assert_eq!(get_para_text(h, 0), "Hello World!");
        handle::remove(h);
    }

    #[test]
    fn test_insert_multibyte_char_updates_offset_correctly() {
        let h = open_writer();
        // 第二段"第二段。"开头插入中文字符'中'（3字节UTF-8），
        // 验证 offset 增量是字节数而非字符数（这是本函数最容易出bug的点）。
        let ptr = roe_insert_char(h, 0, 0, 1, 0, '中' as u32);
        assert!(!ptr.is_null());
        let s = unsafe { CString::from_raw(ptr) }.into_string().unwrap();
        let offset = parse_char_offset(&s);
        assert_eq!(offset, 3, "插入3字节UTF-8字符后偏移应+3，实际 {offset}");
        assert_eq!(get_para_text(h, 1), "中第二段。");
        handle::remove(h);
    }

    #[test]
    fn test_insert_at_out_of_range_offset_rejected() {
        let h = open_writer();
        // "Hello World" 长度11，offset=999 越界
        let ptr = roe_insert_char(h, 0, 0, 0, 999, 'X' as u32);
        assert!(ptr.is_null(), "越界偏移应该返回 null");
        assert_eq!(get_para_text(h, 0), "Hello World", "越界插入不应改动文档");
        handle::remove(h);
    }

    #[test]
    fn test_insert_at_invalid_para_index_rejected() {
        let h = open_writer();
        let ptr = roe_insert_char(h, 0, 0, 999, 0, 'X' as u32);
        assert!(ptr.is_null(), "非法 para_index 应该返回 null");
        handle::remove(h);
    }

    #[test]
    fn test_non_body_container_type_rejected() {
        let h = open_writer();
        let ptr = roe_insert_char(h, 1, 0, 0, 0, 'X' as u32);
        assert!(ptr.is_null(), "非 BODY container_type 应该返回 null");
        handle::remove(h);
    }

    #[test]
    fn test_invalid_handle_returns_null() {
        let ptr = roe_insert_char(999_999, 0, 0, 0, 0, 'X' as u32);
        assert!(ptr.is_null(), "无效句柄应该返回 null");
    }

    #[test]
    fn test_insert_undo_restores_previous_text() {
        let h = open_writer();
        let ptr = roe_insert_char(h, 0, 0, 0, 0, 'X' as u32);
        assert!(!ptr.is_null());
        let _ = unsafe { CString::from_raw(ptr) };
        assert_eq!(get_para_text(h, 0), "XHello World");

        // 插入操作应该等价 commit_edit，undo_stack 里应该有一条可撤销记录。
        let undo_result = crate::snapshot::roe_undo(h);
        assert!(!undo_result.is_null(), "插入后应该可以 undo");
        let _ = unsafe { CString::from_raw(undo_result) };
        assert_eq!(get_para_text(h, 0), "Hello World", "undo 后应恢复插入前的文本");
        handle::remove(h);
    }

    #[test]
    fn test_two_inserts_are_two_separate_undo_steps() {
        let h = open_writer();
        let ptr1 = roe_insert_char(h, 0, 0, 0, 0, 'A' as u32);
        assert!(!ptr1.is_null());
        let _ = unsafe { CString::from_raw(ptr1) };
        assert_eq!(get_para_text(h, 0), "AHello World");

        let ptr2 = roe_insert_char(h, 0, 0, 0, 0, 'B' as u32);
        assert!(!ptr2.is_null());
        let _ = unsafe { CString::from_raw(ptr2) };
        assert_eq!(get_para_text(h, 0), "BAHello World");

        // 两次插入应该是两个独立的 undo 步骤，不是合并成一步。
        {
            let __undo_ptr = crate::snapshot::roe_undo(h);
            assert!(!__undo_ptr.is_null());
            let _ = unsafe { CString::from_raw(__undo_ptr) };
        }
        assert_eq!(get_para_text(h, 0), "AHello World", "第一次 undo 应该只撤销最后一次插入");

        {
            let __undo_ptr = crate::snapshot::roe_undo(h);
            assert!(!__undo_ptr.is_null());
            let _ = unsafe { CString::from_raw(__undo_ptr) };
        }
        assert_eq!(get_para_text(h, 0), "Hello World", "第二次 undo 应该撤销第一次插入");
        handle::remove(h);
    }

    // ---- roe_delete_char 测试 ----

    #[test]
    fn test_delete_backward_removes_previous_char() {
        let h = open_writer();
        // "Hello World" offset=5（空格前）退格 -> 删除 'o' -> "Hell World"
        let ptr = roe_delete_char(h, 0, 0, 0, 5, DeleteDirection::Backward as u32);
        assert!(!ptr.is_null(), "退格应该成功");
        let s = unsafe { CString::from_raw(ptr) }.into_string().unwrap();
        let offset = parse_char_offset(&s);
        assert_eq!(offset, 4, "退格后偏移应回退到被删字符起点，实际 {offset}");
        assert_eq!(get_para_text(h, 0), "Hell World");
        handle::remove(h);
    }

    #[test]
    fn test_delete_forward_removes_next_char() {
        let h = open_writer();
        // "Hello World" offset=4（'o'前）Delete键 -> 删除 'o' -> "Hell World"
        let ptr = roe_delete_char(h, 0, 0, 0, 4, DeleteDirection::Forward as u32);
        assert!(!ptr.is_null(), "Delete键应该成功");
        let s = unsafe { CString::from_raw(ptr) }.into_string().unwrap();
        let offset = parse_char_offset(&s);
        assert_eq!(offset, 4, "Delete键后偏移应保持不变，实际 {offset}");
        assert_eq!(get_para_text(h, 0), "Hell World");
        handle::remove(h);
    }

    #[test]
    fn test_delete_backward_at_paragraph_start_rejected() {
        let h = open_writer();
        let ptr = roe_delete_char(h, 0, 0, 0, 0, DeleteDirection::Backward as u32);
        assert!(ptr.is_null(), "段落开头退格应该返回 null（本次不支持跨段合并）");
        assert_eq!(get_para_text(h, 0), "Hello World", "失败的删除不应改动文档");
        handle::remove(h);
    }

    #[test]
    fn test_delete_forward_at_paragraph_end_rejected() {
        let h = open_writer();
        // "Hello World" 长度11，offset=11 已经是段尾
        let ptr = roe_delete_char(h, 0, 0, 0, 11, DeleteDirection::Forward as u32);
        assert!(ptr.is_null(), "段落末尾Delete键应该返回 null（本次不支持跨段合并）");
        assert_eq!(get_para_text(h, 0), "Hello World", "失败的删除不应改动文档");
        handle::remove(h);
    }

    #[test]
    fn test_delete_backward_multibyte_char_correct_byte_length() {
        let h = open_writer();
        // 第二段"第二段。"，先插入验证过的中文字符定位方式：
        // 直接对现有"第二段。"（每字3字节，共12字节）在offset=3（"二"前）
        // 退格，应该删除"第"（3字节），offset回退到0。
        let ptr = roe_delete_char(h, 0, 0, 1, 3, DeleteDirection::Backward as u32);
        assert!(!ptr.is_null());
        let s = unsafe { CString::from_raw(ptr) }.into_string().unwrap();
        let offset = parse_char_offset(&s);
        assert_eq!(offset, 0, "退格删除3字节UTF-8字符后应回退到offset=0，实际 {offset}");
        assert_eq!(get_para_text(h, 1), "二段。");
        handle::remove(h);
    }

    #[test]
    fn test_delete_forward_multibyte_char_correct_byte_length() {
        let h = open_writer();
        // "第二段。"，offset=0 处 Delete键，应删除"第"（3字节），offset不变。
        let ptr = roe_delete_char(h, 0, 0, 1, 0, DeleteDirection::Forward as u32);
        assert!(!ptr.is_null());
        let s = unsafe { CString::from_raw(ptr) }.into_string().unwrap();
        let offset = parse_char_offset(&s);
        assert_eq!(offset, 0);
        assert_eq!(get_para_text(h, 1), "二段。");
        handle::remove(h);
    }

    #[test]
    fn test_delete_all_chars_in_run_removes_empty_run() {
        let h = open_writer();
        // 反复退格删空第0段"Hello World"（11字节，11次Backward），
        // 验证：1) 最终段落文本为空字符串；2) runs 被清理到不留空run
        // （通过检查 get_para_text 拼接结果为空，且未panic，间接验证
        // 空run清理逻辑没有破坏后续删除操作——如果空run没被移除，
        // 下一次删除会因为offset落在一个空run里而找不到可删字符导致
        // 提前报错，11次连续退格无法全部成功）。
        for i in (1..=11).rev() {
            let ptr = roe_delete_char(h, 0, 0, 0, i, DeleteDirection::Backward as u32);
            if ptr.is_null() {
                let err_ptr = crate::error::roe_last_error_message();
                let err_msg = if err_ptr.is_null() {
                    "（无错误详情）".to_string()
                } else {
                    unsafe { CString::from_raw(err_ptr) }.into_string().unwrap_or_default()
                };
                panic!("第{}次退格应该成功（从offset={}退格），实际失败：{}", 12 - i, i, err_msg);
            }
            let _ = unsafe { CString::from_raw(ptr) };
        }
        assert_eq!(get_para_text(h, 0), "", "连续退格删空后段落应为空字符串");
        handle::remove(h);
    }

    // v115 新增：复现并验证"段首/空段落无法打字"bug的修复
    // （ROE_Progress_v114.md 三.1 遗留任务）。场景：先用退格把第0段
    // 删空（复现 test_delete_all_chars_in_run_removes_empty_run 验证过
    // 的"删空后runs为空vec"状态），再验证此时能否成功插入文本——修复前
    // insert_text_in_entry 遍历空runs的for循环不会执行，target_run恒为
    // None，必定返回Err，本测试在修复前应该失败。
    #[test]
    fn test_insert_text_into_empty_paragraph() {
        let h = open_writer();
        for i in (1..=11).rev() {
            let ptr = roe_delete_char(h, 0, 0, 0, i, DeleteDirection::Backward as u32);
            assert!(!ptr.is_null(), "退格第{}次应成功（前置条件准备）", 12 - i);
            let _ = unsafe { CString::from_raw(ptr) };
        }
        assert_eq!(get_para_text(h, 0), "", "前置条件：段落应已删空");

        let text = "New";
        let ptr = roe_insert_text(
            h, 0, 0, 0, 0,
            text.as_bytes().as_ptr(), text.as_bytes().len() as u32,
        );
        if ptr.is_null() {
            let err_ptr = crate::error::roe_last_error_message();
            let err_msg = if err_ptr.is_null() {
                "（无错误详情）".to_string()
            } else {
                unsafe { CString::from_raw(err_ptr) }.into_string().unwrap_or_default()
            };
            panic!("空段落插入文本应该成功，实际失败：{}", err_msg);
        }
        let s = unsafe { CString::from_raw(ptr) }.into_string().unwrap();
        let offset = parse_char_offset(&s);
        assert_eq!(offset, 3, "插入3字节文本后偏移应为3，实际 {offset}");
        assert_eq!(get_para_text(h, 0), "New", "空段落插入后文本应为插入内容");
        handle::remove(h);
    }

    // v115 新增：replace_range 的空段落场景（IME组合场景的实际调用路径，
    // 与insert_text不同函数，独立验证）。
    #[test]
    fn test_replace_range_into_empty_paragraph() {
        let h = open_writer();
        for i in (1..=11).rev() {
            let ptr = roe_delete_char(h, 0, 0, 0, i, DeleteDirection::Backward as u32);
            assert!(!ptr.is_null(), "退格第{}次应成功（前置条件准备）", 12 - i);
            let _ = unsafe { CString::from_raw(ptr) };
        }
        assert_eq!(get_para_text(h, 0), "", "前置条件：段落应已删空");

        let text = "你好";
        let ptr = roe_replace_range(
            h, 0, 0, 0, 0, 0,
            text.as_bytes().as_ptr(), text.as_bytes().len() as u32,
        );
        if ptr.is_null() {
            let err_ptr = crate::error::roe_last_error_message();
            let err_msg = if err_ptr.is_null() {
                "（无错误详情）".to_string()
            } else {
                unsafe { CString::from_raw(err_ptr) }.into_string().unwrap_or_default()
            };
            panic!("空段落 replace_range（0,0）应该成功，实际失败：{}", err_msg);
        }
        let s = unsafe { CString::from_raw(ptr) }.into_string().unwrap();
        let offset = parse_char_offset(&s);
        assert_eq!(offset, 6, "插入'你好'（每字3字节）后偏移应为6，实际 {offset}");
        assert_eq!(get_para_text(h, 0), "你好", "空段落replace_range后文本应为插入内容");
        handle::remove(h);
    }

    #[test]
    fn test_delete_non_body_container_type_rejected() {
        let h = open_writer();
        let ptr = roe_delete_char(h, 1, 0, 0, 5, DeleteDirection::Backward as u32);
        assert!(ptr.is_null(), "非 BODY container_type 应该返回 null");
        handle::remove(h);
    }

    #[test]
    fn test_delete_invalid_direction_rejected() {
        let h = open_writer();
        let ptr = roe_delete_char(h, 0, 0, 0, 5, 999);
        assert!(ptr.is_null(), "非法 direction 数值应该返回 null，而不是 panic");
        handle::remove(h);
    }

    #[test]
    fn test_delete_invalid_handle_returns_null() {
        let ptr = roe_delete_char(999_999, 0, 0, 0, 5, DeleteDirection::Backward as u32);
        assert!(ptr.is_null(), "无效句柄应该返回 null");
    }

    #[test]
    fn test_delete_backward_at_multi_run_boundary() {
        // v101 回归测试：模拟"Hello" + "(" 两个独立 run 拼接的段落
        // （标点符号自成一个 run 是常见的真实场景），在两个 run 的交界处
        // （char_offset=5，正好是"Hello"结束/"("开始的边界）退格，
        // 应该删除"Hello"的最后一个字符'o'，而不是报错。这是 v101 修复
        // 前的真实 bug：locate_run 把边界值定位到下一个 run（"("）的
        // 开头（offset_in_run=0），导致"在此之前找字符"这一步在空切片上
        // 操作直接报错，表现为退格在标点/run边界处卡住。
        let h = open_writer();
        handle::with_entry(h, |entry| {
            let RoeDocument::Writer(doc) = &mut entry.doc else { unreachable!() };
            let Some(BodyElement::Paragraph(para)) = doc.body.get_mut(0) else { unreachable!() };
            // 把第0段的runs强行改造成两个run："Hello" 和 "("，
            // 模拟标点符号被解析成独立run的真实场景。
            para.runs.truncate(1);
            para.runs[0].text = "Hello".to_string();
            let mut paren_run = para.runs[0].clone();
            paren_run.text = "(".to_string();
            para.runs.push(paren_run);
        });
        assert_eq!(get_para_text(h, 0), "Hello(");

        // 在 run 边界 offset=5（"Hello"结束/"("开始）退格，应删除 'o'。
        let ptr = roe_delete_char(h, 0, 0, 0, 5, DeleteDirection::Backward as u32);
        assert!(!ptr.is_null(), "run边界处退格应该成功，不应报错卡住");
        let s = unsafe { CString::from_raw(ptr) }.into_string().unwrap();
        let offset = parse_char_offset(&s);
        assert_eq!(offset, 4, "退格后偏移应回退到被删字符'o'的起点，实际 {offset}");
        assert_eq!(get_para_text(h, 0), "Hell(", "应删除'Hello'的最后一个字符，保留'('");
        handle::remove(h);
    }

    #[test]
    fn test_delete_undo_restores_deleted_char() {
        let h = open_writer();
        let ptr = roe_delete_char(h, 0, 0, 0, 5, DeleteDirection::Backward as u32);
        assert!(!ptr.is_null());
        let _ = unsafe { CString::from_raw(ptr) };
        assert_eq!(get_para_text(h, 0), "Hell World");

        {
            let __undo_ptr = crate::snapshot::roe_undo(h);
            assert!(!__undo_ptr.is_null());
            let _ = unsafe { CString::from_raw(__undo_ptr) };
        }
        assert_eq!(get_para_text(h, 0), "Hello World", "undo 后应恢复删除前的文本");
        handle::remove(h);
    }

    // ===== 以下为 roe_insert_text（v101新增，批量插入）测试 =====

    fn call_insert_text(h: u64, para_index: u32, char_offset: u32, text: &str) -> *mut c_char {
        let bytes = text.as_bytes();
        roe_insert_text(h, 0, 0, para_index, char_offset, bytes.as_ptr(), bytes.len() as u32)
    }

    #[test]
    fn test_insert_text_ascii_multichar_at_end() {
        let h = open_writer();
        // "Hello World" 末尾插入 " Foo" -> "Hello World Foo"
        let ptr = call_insert_text(h, 0, 11, " Foo");
        assert!(!ptr.is_null(), "批量插入应该成功");
        let s = unsafe { CString::from_raw(ptr) }.into_string().unwrap();
        let offset = parse_char_offset(&s);
        assert_eq!(offset, 15, "插入4字节ASCII文本后偏移应为 11+4=15，实际 {offset}");
        assert_eq!(get_para_text(h, 0), "Hello World Foo");
        handle::remove(h);
    }

    #[test]
    fn test_insert_text_ascii_multichar_in_middle() {
        let h = open_writer();
        // "Hello World" offset=5（空格前）插入 ",There" -> "Hello,There World"
        let ptr = call_insert_text(h, 0, 5, ",There");
        assert!(!ptr.is_null());
        let s = unsafe { CString::from_raw(ptr) }.into_string().unwrap();
        let offset = parse_char_offset(&s);
        assert_eq!(offset, 11, "offset=5 插入6字节文本后应为 5+6=11，实际 {offset}");
        assert_eq!(get_para_text(h, 0), "Hello,There World");
        handle::remove(h);
    }

    #[test]
    fn test_insert_text_cjk_multichar_updates_offset_by_bytes() {
        let h = open_writer();
        // 第二段初始为 "第二段。"（4个中文字符，每个3字节，共12字节）
        // 在末尾（offset=12）插入 "你好"（2个中文字符，每个3字节，共6字节）
        // -> "第二段。你好"，新offset应为 12+6=18（按字节数，不是字符数）
        let ptr = call_insert_text(h, 1, 12, "你好");
        assert!(!ptr.is_null(), "中文批量插入应该成功");
        let s = unsafe { CString::from_raw(ptr) }.into_string().unwrap();
        let offset = parse_char_offset(&s);
        assert_eq!(offset, 18, "插入6字节中文文本后偏移应为 12+6=18（按UTF-8字节数），实际 {offset}");
        assert_eq!(get_para_text(h, 1), "第二段。你好");
        handle::remove(h);
    }

    #[test]
    fn test_insert_text_cjk_multichar_in_middle() {
        let h = open_writer();
        // 第二段 "第二段。" offset=6（"第二"之后，"段。"之前，字节偏移
        // "第"=3字节+"二"=3字节=6）插入 "你好" -> "第二你好段。"
        let ptr = call_insert_text(h, 1, 6, "你好");
        assert!(!ptr.is_null());
        let s = unsafe { CString::from_raw(ptr) }.into_string().unwrap();
        let offset = parse_char_offset(&s);
        assert_eq!(offset, 12, "offset=6 插入6字节中文后应为 6+6=12，实际 {offset}");
        assert_eq!(get_para_text(h, 1), "第二你好段。");
        handle::remove(h);
    }

    #[test]
    fn test_insert_text_at_out_of_range_offset_rejected() {
        let h = open_writer();
        // "Hello World" 总长11字节，offset=999 越界
        let ptr = call_insert_text(h, 0, 999, "x");
        assert!(ptr.is_null(), "越界offset应该被拒绝");
        handle::remove(h);
    }

    #[test]
    fn test_insert_text_at_invalid_para_index_rejected() {
        let h = open_writer();
        let ptr = call_insert_text(h, 99, 0, "x");
        assert!(ptr.is_null(), "无效para_index应该被拒绝");
        handle::remove(h);
    }

    #[test]
    fn test_insert_text_non_body_container_type_rejected() {
        let h = open_writer();
        // container_type=1 (TABLE_CELL，本次未实现)
        let bytes = "x".as_bytes();
        let ptr = roe_insert_text(h, 1, 0, 0, 0, bytes.as_ptr(), bytes.len() as u32);
        assert!(ptr.is_null(), "非BODY容器类型应该被拒绝（表格cell本次未实现）");
        handle::remove(h);
    }

    #[test]
    fn test_insert_text_invalid_handle_returns_null() {
        let bytes = "x".as_bytes();
        let ptr = roe_insert_text(999999, 0, 0, 0, 0, bytes.as_ptr(), bytes.len() as u32);
        assert!(ptr.is_null(), "无效句柄应返回null");
    }

    #[test]
    fn test_insert_text_zero_len_rejected() {
        let h = open_writer();
        let bytes = "x".as_bytes(); // 指针本身有效，但传 text_len=0
        let ptr = roe_insert_text(h, 0, 0, 0, 0, bytes.as_ptr(), 0);
        assert!(ptr.is_null(), "text_len=0 应该被拒绝（空文本插入没有意义）");
        handle::remove(h);
    }

    #[test]
    fn test_insert_text_null_pointer_rejected() {
        let h = open_writer();
        let ptr = roe_insert_text(h, 0, 0, 0, 0, std::ptr::null(), 5);
        assert!(ptr.is_null(), "text_utf8为空指针应该被拒绝");
        handle::remove(h);
    }

    #[test]
    fn test_insert_text_offset_not_on_char_boundary_rejected() {
        let h = open_writer();
        // 第二段 "第二段。" 中文字符，offset=1 落在"第"这个字符内部（非边界）
        let ptr = call_insert_text(h, 1, 1, "x");
        assert!(ptr.is_null(), "非字符边界的offset应该被拒绝");
        handle::remove(h);
    }

    #[test]
    fn test_insert_text_undo_restores_entire_batch_in_one_step() {
        let h = open_writer();
        // 核心差异测试：批量插入"你好世界"（4字符）应该是"一次"undo操作
        // 就能整体撤销，而不是逐字符插入需要4次undo。
        let ptr = call_insert_text(h, 0, 11, "你好世界");
        assert!(!ptr.is_null());
        let _ = unsafe { CString::from_raw(ptr) };
        assert_eq!(get_para_text(h, 0), "Hello World你好世界");

        // 一次undo应该整体恢复到插入前，不是只撤销部分字符
        {
            let __undo_ptr = crate::snapshot::roe_undo(h);
            assert!(!__undo_ptr.is_null());
            let _ = unsafe { CString::from_raw(__undo_ptr) };
        }
        assert_eq!(
            get_para_text(h, 0),
            "Hello World",
            "一次undo应完整恢复批量插入前的文本（批量插入=一次undo步骤）"
        );
        handle::remove(h);
    }

    #[test]
    fn test_insert_text_two_batches_are_two_separate_undo_steps() {
        let h = open_writer();
        let ptr1 = call_insert_text(h, 0, 11, "AAA");
        assert!(!ptr1.is_null());
        let _ = unsafe { CString::from_raw(ptr1) };
        assert_eq!(get_para_text(h, 0), "Hello WorldAAA");

        let ptr2 = call_insert_text(h, 0, 14, "BBB");
        assert!(!ptr2.is_null());
        let _ = unsafe { CString::from_raw(ptr2) };
        assert_eq!(get_para_text(h, 0), "Hello WorldAAABBB");

        // 第一次undo只撤销第二批(BBB)
        {
            let __undo_ptr = crate::snapshot::roe_undo(h);
            assert!(!__undo_ptr.is_null());
            let _ = unsafe { CString::from_raw(__undo_ptr) };
        }
        assert_eq!(get_para_text(h, 0), "Hello WorldAAA", "第一次undo应只撤销后一批插入");

        // 第二次undo才撤销第一批(AAA)
        {
            let __undo_ptr = crate::snapshot::roe_undo(h);
            assert!(!__undo_ptr.is_null());
            let _ = unsafe { CString::from_raw(__undo_ptr) };
        }
        assert_eq!(get_para_text(h, 0), "Hello World", "第二次undo应撤销前一批插入");
        handle::remove(h);
    }
}


/// 在光标处按回车切分段落（v105新增）。语义：把 (para_index, char_offset)
/// 处的段落一分为二——[0, char_offset) 的内容留在原段落（保留原
/// para_index 及其 style/list_info/alignment 等段落级属性），
/// [char_offset, end) 的内容移动到紧跟其后的新段落（新段落 clone 原
/// 段落的所有元数据，只替换 runs，即"继承样式"）。若 char_offset 落在
/// 某个 run 内部，该 run 会被切成两半分属前后两个段落。
///
/// 参照 ROE_Progress_v104.md「三、3.3 回车的正确语义」：光标移动到新
/// 段落开头，即返回 (para_index+1, 0)。
///
/// 事务处理与 insert_char_in_entry/delete_char_in_entry 同构：先只读
/// 校验，校验通过后才 clone 快照 + 实际修改 + 推入 undo_stack + 清空
/// redo_stack。
///
/// 范围限制（本次有意收窄，同 v104 文档"3.4 未纳入本次工作量评估"）：
/// 仅支持"光标处、无选区"的最简单情况，不处理"当前有选区，先删除选区
/// 内容再分段"的组合场景（留给后续，调用方应在有选区时自行先删除选区
/// 再调用本函数，或后续由更上层的接口统一处理）。也不实现 Shift+回车
/// 的软换行语义（同段落内强制换行、不产生新段落）——那是与本函数完全
///不同的语义，需要 LineBox 体系配合行内换行符表示，本次未确认现有
/// 体系是否支持，不在本函数范围内。
///
/// 参数：
/// - handle：文档句柄
/// - container_type/container_id/local_para_index/char_offset：拆解后
///   的 RoeLocation（分段发生前的光标位置）
///
/// 返回：JSON 字符串
/// `{"container_type":0,"container_id":0,"local_para_index":N,"char_offset":0}`
/// 其中 local_para_index 是新段落（分段后光标应停留处）的下标，
/// 即原 para_index + 1，char_offset 固定为 0（新段落开头）。
///
/// 失败返回 null，调用 roe_last_error_message 获取详情。失败场景：
/// - 非 BODY container_type
/// - 无效句柄或非 Writer 文档
/// - para_index 越界
/// - char_offset 不在任何 run 的字符边界上
///
/// 调用方必须用 roe_free_string 释放返回值。
#[no_mangle]
pub extern "C" fn roe_split_paragraph(
    handle: u64,
    container_type: u32,
    container_id: u32,
    local_para_index: u32,
    char_offset: u32,
) -> *mut c_char {
    let loc = RoeLocation { container_type, container_id, local_para_index, char_offset };

    if !loc.is_body() {
        set_last_error("roe_split_paragraph: 本次仅支持 container_type=BODY（表格cell/文本框预留未实现）");
        return std::ptr::null_mut();
    }

    let (para_index, char_offset) = loc.as_legacy_body();

    let result = handle::with_entry(handle, |entry| {
        split_paragraph_in_entry(entry, para_index, char_offset)
    });

    let is_writer_ok = matches!(&result, Some(Ok(_)));

    // 必须在 with_entry 闭包返回之后调用，避免同一 handle 的 RefCell
    // 嵌套借用 panic（与 roe_insert_char/roe_delete_char 相同的坑）。
    if is_writer_ok {
        handle::invalidate_writer_layout_cache(handle);
    }

    match result {
        None => {
            set_last_error("roe_split_paragraph: 无效句柄");
            std::ptr::null_mut()
        }
        Some(Err(msg)) => {
            set_last_error(format!("roe_split_paragraph: {msg}"));
            std::ptr::null_mut()
        }
        Some(Ok(json)) => match CString::new(json) {
            Ok(cs) => cs.into_raw(),
            Err(e) => {
                set_last_error(format!("roe_split_paragraph: {e}"));
                std::ptr::null_mut()
            }
        },
    }
}

/// 核心逻辑：在 char_offset 处把段落切成两个，插入新段落到 para_index+1。
/// 事务处理与 insert_char_in_entry 完全同构：先只读校验，校验通过后才
/// clone 快照 + 实际修改 + 推入 undo_stack + 清空 redo_stack。
fn split_paragraph_in_entry(
    entry: &mut handle::DocumentEntry,
    para_index: usize,
    char_offset: usize,
) -> Result<String, String> {
    let RoeDocument::Writer(doc) = &entry.doc else {
        return Err("非 Writer 文档".to_string());
    };

    let para = match doc.body.get(para_index) {
        Some(BodyElement::Paragraph(p)) => p,
        _ => return Err(format!("para_index={para_index} 越界或不存在")),
    };

    // 只读校验：定位 char_offset 落在哪个 run 的哪个字节位置。语义与
    // insert_char_in_entry 的定位循环一致——允许 char_offset 落在段落
    // 末尾"之后一位"（即 consumed==char_offset，落在最后一个 run 结束
    // 处），这种情况下新段落是空段落（没有 run 需要移动过去）。
    let mut consumed = 0usize;
    // target: Some((run_index, offset_in_run)) 表示落在某个 run 内部；
    // None 且 consumed==char_offset 表示落在段落末尾（新段落为空）。
    let mut target: Option<(usize, usize)> = None;
    let mut found = false;
    for (i, run) in para.runs.iter().enumerate() {
        let run_len = run.text.len();
        if char_offset <= consumed + run_len {
            let offset_in_run = char_offset - consumed;
            if !run.text.is_char_boundary(offset_in_run) {
                return Err(format!(
                    "char_offset={char_offset} 不在 run[{i}] 的字符边界上（内部字节偏移={offset_in_run}）"
                ));
            }
            target = Some((i, offset_in_run));
            found = true;
            break;
        }
        consumed += run_len;
    }
    if !found {
        if consumed != char_offset {
            return Err(format!(
                "char_offset={char_offset} 超出段落范围（段落总字节长度={consumed}）"
            ));
        }
        // char_offset 恰好等于段落总长度：落在段落末尾，target 保持 None。
    }

    // 校验通过，开始实际修改：克隆快照（等价 roe_begin_edit）。
    entry.snapshot = Some(entry.doc.clone());

    let RoeDocument::Writer(doc) = &mut entry.doc else {
        unreachable!("上面已确认是 Writer 文档");
    };
    let Some(BodyElement::Paragraph(para)) = doc.body.get_mut(para_index) else {
        unreachable!("上面已确认 para_index 有效");
    };

    // 新段落继承原段落的所有元数据（style/list_info/alignment等），
    // 只替换 runs——用 clone() 而不是手写字段列表，避免 Paragraph 未来
    // 新增字段时这里漏拷贝。
    let mut new_para = para.clone();

    let new_runs = match target {
        Some((run_index, offset_in_run)) => {
            // 落在某个 run 内部：该 run 可能需要被切成两半。
            let mut tail_runs: Vec<roe_model::writer::Run> = para.runs.split_off(run_index + 1);
            let split_run = para.runs.pop().expect("run_index 已在上面校验存在");
            if offset_in_run == 0 {
                // 边界正好在 run 开头：整个 run 归后半段，前半段不含它。
                let mut front = vec![split_run];
                front.extend(tail_runs.drain(..));
                front
            } else if offset_in_run == split_run.text.len() {
                // 边界正好在 run 结尾：整个 run 归前半段，后半段从下一个
                // run 开始。
                para.runs.push(split_run);
                tail_runs
            } else {
                // 边界落在 run 中间：真正切开该 run，前半部分留在原段落，
                // 后半部分放进新段落。用 clone() 保留除 text 外的所有
                // run 级属性（bold/italic/hyperlink/color等），前后两半
                // 共享同一套格式，这是标准编辑器回车分段的预期行为。
                let mut front_run = split_run.clone();
                let mut back_run = split_run;
                front_run.text.truncate(offset_in_run);
                back_run.text = back_run.text.split_off(offset_in_run);
                para.runs.push(front_run);
                let mut new_runs = vec![back_run];
                new_runs.extend(tail_runs.drain(..));
                new_runs
            }
        }
        None => {
            // 落在段落末尾：前半段保留全部现有 runs，新段落没有内容。
            Vec::new()
        }
    };

    new_para.runs = new_runs;
    doc.body.insert(para_index + 1, BodyElement::Paragraph(new_para));

    // 等价 roe_commit_edit：快照进 undo_stack，redo_stack 清空。
    if let Some(prev) = entry.snapshot.take() {
        let ctx = handle::EditCursorContext::at(para_index, char_offset);
        entry.push_undo_snapshot(prev, ctx);
    }

    Ok(format!(
        "{{\"container_type\":0,\"container_id\":0,\"local_para_index\":{},\"char_offset\":0}}",
        para_index + 1
    ))
}


/// 跨段落合并（v105新增，回车分段 roe_split_paragraph 的逆操作）。
/// 语义：段首退格 或 段尾Delete 时，把光标所在段与相邻段合并成一个段落。
///
/// - direction=Backward（退格）：要求 char_offset==0 且 para_index>0，
///   合并 (para_index-1) 和 para_index 两段——结果段落保留前一段
///   （para_index-1）的所有元数据（style/list_info/alignment等），
///   runs 依次拼接"前一段的runs + 当前段的runs"，然后从 body 中移除
///   原 para_index 位置的段落。光标停在合并点，即原 para_index-1 段落
///   的原有文字末尾（不含新拼入的内容）。
/// - direction=Forward（Delete）：要求 char_offset==段落末尾字节长度
///   （即光标已在本段末尾）且本段不是文档最后一段，合并 para_index 和
///   (para_index+1) 两段——结果段落保留 para_index 的元数据，runs 依次
///   拼接"当前段的runs + 下一段的runs"，移除原 (para_index+1) 位置的
///   段落。光标停在合并点，即原 para_index 段落的文字末尾（与传入的
///   char_offset 数值相同，因为该段自身内容没有变化，只是后面接上了
///   新内容）。
///
/// 两种方向合并后光标都停在"两段的交界处"，这是本次拍板的统一行为
/// （用户确认：不需要区分退格/Delete给出不同的光标落点）。
///
/// 参数：
/// - handle：文档句柄
/// - container_type/container_id/local_para_index/char_offset：拆解后
///   的 RoeLocation（合并发生前的光标位置）
/// - direction：0=Backward退格 1=Forward Delete，与 roe_delete_char 的
///   DeleteDirection 编码一致
///
/// 返回：JSON 字符串
/// `{"container_type":0,"container_id":0,"local_para_index":N,"char_offset":N}`
/// 即合并完成后光标应移动到的新位置。
///
/// 失败返回 null，调用 roe_last_error_message 获取详情。失败场景：
/// - 非 BODY container_type
/// - 无效句柄或非 Writer 文档
/// - para_index 越界
/// - Backward 但 char_offset != 0，或 para_index == 0（已是首段，无上
///   一段可合并）
/// - Forward 但 char_offset 不在段落末尾，或 para_index 已是文档最后
///   一段（无下一段可合并）
///
/// 调用方必须用 roe_free_string 释放返回值。
#[no_mangle]
pub extern "C" fn roe_merge_paragraph(
    handle: u64,
    container_type: u32,
    container_id: u32,
    local_para_index: u32,
    char_offset: u32,
    direction: u32,
) -> *mut c_char {
    let loc = RoeLocation { container_type, container_id, local_para_index, char_offset };

    if !loc.is_body() {
        set_last_error("roe_merge_paragraph: 本次仅支持 container_type=BODY（表格cell/文本框预留未实现）");
        return std::ptr::null_mut();
    }

    let Some(dir) = DeleteDirection::from_u32(direction) else {
        set_last_error(format!("roe_merge_paragraph: 非法 direction 值 {direction}"));
        return std::ptr::null_mut();
    };

    let (para_index, char_offset) = loc.as_legacy_body();

    let result = handle::with_entry(handle, |entry| {
        merge_paragraph_in_entry(entry, para_index, char_offset, dir)
    });

    let is_writer_ok = matches!(&result, Some(Ok(_)));

    // 必须在 with_entry 闭包返回之后调用，避免同一 handle 的 RefCell
    // 嵌套借用 panic（与其他 text_edit 函数相同的坑）。
    if is_writer_ok {
        handle::invalidate_writer_layout_cache(handle);
    }

    match result {
        None => {
            set_last_error("roe_merge_paragraph: 无效句柄");
            std::ptr::null_mut()
        }
        Some(Err(msg)) => {
            set_last_error(format!("roe_merge_paragraph: {msg}"));
            std::ptr::null_mut()
        }
        Some(Ok(json)) => match CString::new(json) {
            Ok(cs) => cs.into_raw(),
            Err(e) => {
                set_last_error(format!("roe_merge_paragraph: {e}"));
                std::ptr::null_mut()
            }
        },
    }
}

/// 核心逻辑：合并相邻两个段落。事务处理与其他 text_edit 函数同构：
/// 先只读校验，校验通过后才 clone 快照 + 实际修改 + 推入 undo_stack +
/// 清空 redo_stack。
fn merge_paragraph_in_entry(
    entry: &mut handle::DocumentEntry,
    para_index: usize,
    char_offset: usize,
    direction: DeleteDirection,
) -> Result<String, String> {
    let RoeDocument::Writer(doc) = &entry.doc else {
        return Err("非 Writer 文档".to_string());
    };

    let para = match doc.body.get(para_index) {
        Some(BodyElement::Paragraph(p)) => p,
        _ => return Err(format!("para_index={para_index} 越界或不存在")),
    };
    let para_len: usize = para.runs.iter().map(|r| r.text.len()).sum();

    // 只读校验：确定要合并的是哪两个段落下标 (keep_index, remove_index)，
    // 以及合并完成后光标应停在哪个段落的哪个字节偏移。
    let (keep_index, remove_index, result_offset) = match direction {
        DeleteDirection::Backward => {
            if char_offset != 0 {
                return Err(format!(
                    "退格合并要求 char_offset==0（当前={char_offset}），非段首场景不应调用本函数"
                ));
            }
            if para_index == 0 {
                return Err("已是文档第一段，无上一段可合并".to_string());
            }
            let prev_index = para_index - 1;
            let prev_para = match doc.body.get(prev_index) {
                Some(BodyElement::Paragraph(p)) => p,
                _ => return Err(format!("para_index={prev_index}（上一段）不是段落，无法合并")),
            };
            let prev_len: usize = prev_para.runs.iter().map(|r| r.text.len()).sum();
            (prev_index, para_index, prev_len)
        }
        DeleteDirection::Forward => {
            if char_offset != para_len {
                return Err(format!(
                    "Delete合并要求 char_offset 落在段落末尾（当前={char_offset}，段落长度={para_len}）"
                ));
            }
            let next_index = para_index + 1;
            match doc.body.get(next_index) {
                Some(BodyElement::Paragraph(_)) => {}
                Some(_) => return Err(format!("para_index={next_index}（下一段）不是段落，无法合并")),
                None => return Err("已是文档最后一段，无下一段可合并".to_string()),
            }
            (para_index, next_index, char_offset)
        }
    };

    // 校验通过，开始实际修改：克隆快照（等价 roe_begin_edit）。
    entry.snapshot = Some(entry.doc.clone());

    let RoeDocument::Writer(doc) = &mut entry.doc else {
        unreachable!("上面已确认是 Writer 文档");
    };

    // remove_index 段落的 runs 取出，接到 keep_index 段落末尾。remove_index
    // 恒大于 keep_index（无论 Backward 还是 Forward 都是如此），先移除
    // 下标较大的 remove_index 段落，keep_index 的下标不受影响。
    debug_assert!(remove_index > keep_index, "remove_index 应恒大于 keep_index");
    let removed = doc.body.remove(remove_index);
    let BodyElement::Paragraph(removed_para) = removed else {
        unreachable!("上面已确认 remove_index 处是 Paragraph");
    };
    let Some(BodyElement::Paragraph(keep_para)) = doc.body.get_mut(keep_index) else {
        unreachable!("上面已确认 keep_index 有效");
    };
    keep_para.runs.extend(removed_para.runs);

    // 等价 roe_commit_edit：快照进 undo_stack，redo_stack 清空。
    if let Some(prev) = entry.snapshot.take() {
        let ctx = handle::EditCursorContext::at(para_index, char_offset);
        entry.push_undo_snapshot(prev, ctx);
    }

    Ok(format!(
        "{{\"container_type\":0,\"container_id\":0,\"local_para_index\":{},\"char_offset\":{}}}",
        keep_index, result_offset
    ))
}

/// 在指定段落的字节区间 [start_char_offset, end_char_offset) 上，做一次
/// "原子替换"：删除该区间原有内容，插入 new_text，整个操作只产生
/// *一条* undo 记录（不是先删一条、再插一条）。
///
/// 这是 v112 方案的核心新接口，用来替代旧的"IME删旧插新"实现——旧实现
/// 在 office-rs2 侧用"记住上次插入了几个字符，下次结算时先删这么多字符
/// 再整体插入"这种相对状态模拟区间替换，一旦光标发生移动/跨段落，相对
/// 状态就会和文档实际内容脱节（ROE_Progress_v112.md 二.2 记录的两个
/// 严重bug）。本接口改为接受绝对区间，一次性原子替换，不依赖调用方
/// 自己维护"删多少"这种滚动状态。
///
/// 参数（container_type/container_id/local_para_index 与其他新接口一致，
/// 拆解自 RoeLocation；区间用 start/end 两个 char_offset 表示，语义与
/// insert_text/delete_char 一致：段落 full_text 上的全局字节偏移）：
/// - handle：文档句柄
/// - container_type/container_id/local_para_index：定位段落（本次仅
///   支持 container_type=BODY）
/// - start_char_offset/end_char_offset：待替换的字节区间
///   [start, end)，要求 start <= end，且两者都落在同一段落的合法字节
///   边界上。start == end 时退化为纯插入（等价 insert_text）。
/// - new_text_utf8/new_text_len：替换成的新文本（可以是空文本，退化为
///   纯删除，等价 delete 区间版）。
///
/// 返回：JSON 字符串
/// `{"container_type":0,"container_id":0,"local_para_index":N,"char_offset":N}`
/// 即替换完成后光标应移动到的新位置：start_char_offset + new_text 字节长度
/// （替换后光标落在新插入内容的末尾，这是 IME 组合场景需要的语义——
/// 光标跟着组合中的文字走）。
///
/// 本次范围限制（与其他新接口一致的"不跨段"原则）：
/// - 不支持跨段落的区间替换（IME 组合场景不会产生跨段落的组合文本，
///   v112 方案三.4 已确认此限制，start/end 必须落在同一段落内）。
/// - start_char_offset > end_char_offset 视为参数错误，返回失败
///   （调用方应保证传入前 start <= end，不在这里做"自动交换"的容错，
///   避免掩盖调用方逻辑错误）。
///
/// 失败返回 null，调用 roe_last_error_message 获取详情。
/// 调用方必须用 roe_free_string 释放返回值。
#[no_mangle]
pub extern "C" fn roe_replace_range(
    handle: u64,
    container_type: u32,
    container_id: u32,
    local_para_index: u32,
    start_char_offset: u32,
    end_char_offset: u32,
    new_text_utf8: *const u8,
    new_text_len: u32,
) -> *mut c_char {
    let loc = RoeLocation {
        container_type,
        container_id,
        local_para_index,
        char_offset: start_char_offset,
    };

    if !loc.is_body() {
        set_last_error("roe_replace_range: 本次仅支持 container_type=BODY（表格cell/文本框预留未实现）");
        return std::ptr::null_mut();
    }

    if start_char_offset > end_char_offset {
        set_last_error(format!(
            "roe_replace_range: start_char_offset({start_char_offset}) > end_char_offset({end_char_offset})，非法区间"
        ));
        return std::ptr::null_mut();
    }

    // new_text_len == 0 是合法的（纯删除场景），不像 roe_insert_text 那样
    // 报错；但如果 new_text_len > 0，new_text_utf8 就不能是空指针。
    let new_text = if new_text_len == 0 {
        String::new()
    } else {
        if new_text_utf8.is_null() {
            set_last_error("roe_replace_range: new_text_len > 0 但 new_text_utf8 为空指针");
            return std::ptr::null_mut();
        }
        let bytes = unsafe { std::slice::from_raw_parts(new_text_utf8, new_text_len as usize) };
        match std::str::from_utf8(bytes) {
            Ok(s) => s.to_string(),
            Err(e) => {
                set_last_error(format!("roe_replace_range: new_text_utf8 不是合法UTF-8: {e}"));
                return std::ptr::null_mut();
            }
        }
    };

    let (para_index, _) = loc.as_legacy_body();
    let start = start_char_offset as usize;
    let end = end_char_offset as usize;

    let result = handle::with_entry(handle, |entry| {
        replace_range_in_entry(entry, para_index, start, end, &new_text)
    });

    let is_writer_ok = matches!(&result, Some(Ok(_)));

    // 必须在 with_entry 闭包返回之后调用，避免同一 handle 的 RefCell
    // 嵌套借用 panic（与 roe_insert_text / roe_delete_char 相同的坑）。
    if is_writer_ok {
        handle::invalidate_writer_layout_cache(handle);
    }

    match result {
        None => {
            set_last_error("roe_replace_range: 无效句柄");
            std::ptr::null_mut()
        }
        Some(Err(msg)) => {
            set_last_error(format!("roe_replace_range: {msg}"));
            std::ptr::null_mut()
        }
        Some(Ok(json)) => match CString::new(json) {
            Ok(cs) => cs.into_raw(),
            Err(e) => {
                set_last_error(format!("roe_replace_range: {e}"));
                std::ptr::null_mut()
            }
        },
    }
}

/// 核心逻辑：在段落 full_text 的字节区间 [range_start, range_end) 上做
/// 原子替换。与 insert_text_in_entry / delete_char_in_entry 同构：
/// 先只读校验能否替换（区间必须落在同一个 run 内、且落在字符边界上），
/// 校验通过后才 clone 快照 + 实际修改 + 推入 undo_stack + 清空
/// redo_stack。事务处理手写而不是调用 roe_begin_edit/roe_commit_edit，
/// 原因与本文件其它 xxx_in_entry 函数一致：那两个导出函数各自会独立
/// 调用 handle::with_entry，嵌套调用会导致同一 handle 的 RefCell
/// 重复借用而 panic（snapshot.rs 注释里反复强调的坑）。
///
/// UTF-8 边界处理参考 delete_char_in_entry 的写法（本函数复用同样的
/// "先按 run 遍历累加字节长度，定位区间落在哪个 run、run 内偏移是多少，
/// 全程用 is_char_boundary 校验"思路），不重新发明一套不同的边界处理
/// 逻辑（ROE_Progress_v112.md 三.5 Gemini 提醒事项）。
///
/// 简化假设（本次范围限制，同文件顶部注释一致）：range_start/range_end
/// 必须落在同一个 run 内部（不支持替换区间跨越多个 run 的情况）。
/// 这个假设在当前调用场景（IME 组合替换）下成立，因为：
/// - 组合开始时 start_offset 是光标当前位置，此时该位置左右的文本
///   仍是编辑前的原始状态，跨 run 的可能性存在（比如光标恰好停在两个
///   run 的交界处），但 end_offset（组合过程中不断增长的终点）通常
///   仍落在同一个 run 内，因为组合文本本身是作为新插入内容处理的。
/// 若未来发现该假设不成立（比如需要支持"选中一段跨越多个 run 的文本
/// 后整体替换"这种场景），需要重新设计本函数为多 run 版本，参考
/// selection_rects.rs 里"遍历区间覆盖的多个对象"的思路。
fn replace_range_in_entry(
    entry: &mut handle::DocumentEntry,
    para_index: usize,
    range_start: usize,
    range_end: usize,
    new_text: &str,
) -> Result<String, String> {
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

    // v115 修复：空段落特判，原因和修复方式同 insert_text_in_entry（见该
    // 函数同一处注释）。空段落时 para_len==0，range_start/range_end 只能
    // 都是 0（上面的 range_end > para_len 校验已经保证了这点，range_end
    // 不可能大于0），等价于一次纯插入。
    if para.runs.is_empty() {
        entry.snapshot = Some(entry.doc.clone());

        let RoeDocument::Writer(doc) = &mut entry.doc else {
            unreachable!("上面已确认是 Writer 文档");
        };
        let Some(BodyElement::Paragraph(para)) = doc.body.get_mut(para_index) else {
            unreachable!("上面已确认 para_index 有效");
        };
        if !new_text.is_empty() {
            para.runs.push(roe_model::writer::Run {
                text: new_text.to_string(),
                ..Default::default()
            });
        }

        let new_char_offset = new_text.len();

        if let Some(prev) = entry.snapshot.take() {
            let ctx = handle::EditCursorContext::range(para_index, range_start, range_end);
            entry.push_undo_snapshot(prev, ctx);
        }

        return Ok(format!(
            "{{\"container_type\":0,\"container_id\":0,\"local_para_index\":{},\"char_offset\":{}}}",
            para_index, new_char_offset
        ));
    }

    // 定位 range_start/range_end 落在哪个 run、run 内偏移分别是多少，
    // 并校验两者落在同一个 run 内（本函数不支持跨 run 替换，见函数
    // 文档注释）、且都落在字符边界上。
    let mut consumed = 0usize;
    let mut found_run_index: Option<usize> = None;
    let mut local_start = 0usize;
    let mut local_end = 0usize;

    for (i, run) in para.runs.iter().enumerate() {
        let run_len = run.text.len();
        let run_start = consumed;
        let run_end = consumed + run_len;

        // range_start 落在本 run 内（含首尾边界）。
        let start_in_this_run = range_start >= run_start && range_start <= run_end;
        // range_end 落在本 run 内（含首尾边界）。
        let end_in_this_run = range_end >= run_start && range_end <= run_end;

        if start_in_this_run && end_in_this_run {
            local_start = range_start - run_start;
            local_end = range_end - run_start;

            if !run.text.is_char_boundary(local_start) {
                return Err(format!(
                    "range_start={range_start} 不在 run[{i}] 的字符边界上（内部字节偏移={local_start}）"
                ));
            }
            if !run.text.is_char_boundary(local_end) {
                return Err(format!(
                    "range_end={range_end} 不在 run[{i}] 的字符边界上（内部字节偏移={local_end}）"
                ));
            }

            found_run_index = Some(i);
            break;
        }
        consumed += run_len;
    }

    let Some(run_index) = found_run_index else {
        return Err(format!(
            "range=[{range_start},{range_end}) 未落在同一个 run 内（本次不支持跨 run 替换），段落总字节长度={para_len}"
        ));
    };

    // 校验通过，开始实际修改：克隆快照（等价 roe_begin_edit）。
    entry.snapshot = Some(entry.doc.clone());

    let RoeDocument::Writer(doc) = &mut entry.doc else {
        unreachable!("上面已确认是 Writer 文档");
    };
    let Some(BodyElement::Paragraph(para)) = doc.body.get_mut(para_index) else {
        unreachable!("上面已确认 para_index 有效");
    };

    let run = &mut para.runs[run_index];
    // 一次性区间替换：删除 [local_start, local_end)，插入 new_text。
    // String::replace_range 本身就是"删除区间+插入"的原子操作，比
    // 分两步调用 delete + insert 更直接，且不会在中间态留下不一致的
    // run.text（避免中途 panic 导致数据半损坏）。
    run.text.replace_range(local_start..local_end, new_text);

    // 如果替换后 run 变成空字符串（比如 new_text 是空文本、且这个 run
    // 原本只有 [local_start, local_end) 这段内容），顺手移除，保持
    // 数据干净（与 delete_char_in_entry 的空 run 清理逻辑一致）。
    if run.text.is_empty() {
        para.runs.remove(run_index);
    }

    // 新光标位置：替换区间起点 + 新文本的字节长度（光标落在新插入
    // 内容末尾，IME 组合场景需要这个语义——光标跟着组合中的文字走）。
    let new_char_offset = range_start + new_text.len();

    // 等价 roe_commit_edit：快照进 undo_stack，redo_stack 清空。
    // 整个删除+插入是一次 replace_range 调用完成的，只产生这一条
    // undo 记录，不会像"循环调用 delete_char"那样拆成多条
    // （ROE_Progress_v112.md 三.5 否决方案A的核心原因）。
    if let Some(prev) = entry.snapshot.take() {
        let ctx = handle::EditCursorContext::range(para_index, range_start, range_end);
        entry.push_undo_snapshot(prev, ctx);
    }

    Ok(format!(
        "{{\"container_type\":0,\"container_id\":0,\"local_para_index\":{},\"char_offset\":{}}}",
        para_index, new_char_offset
    ))
}
