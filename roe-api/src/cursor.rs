//! ROE 光标输入/选区功能 —— RoeLocation 与相关基础设施。
//!
//! 设计依据：roe_cursor_selection_final_v1.md（v92会话，三方拍板方案）。
//! 本次（v92第一步）只实现 RoeLocation 结构体本身 + 与现有
//! para_index/char_offset 语义的对齐转换，不包含任何新 C-ABI 导出函数
//! （roe_cursor_move_inline / roe_cursor_move_line /
//! roe_query_selection_rects 留给后续步骤，按 final 文档"七"的实现顺序
//! 分步进行，避免一次改动过大难以排查）。
//!
//! FFI 边界表现形式（与 hit_test.rs / layout_query.rs 现有风格对齐，
//! 三方讨论后确认，不采用 final 文档 3.1 原始草案中的 #[repr(C)] 结构体
//! 传参方式）：
//! - 输入：C-ABI 函数签名里把 RoeLocation 拆解成离散裸参数
//!   （container_type/container_id/local_para_index/char_offset 四个
//!   独立的 u32 参数），不接收 JSON 字符串输入。
//! - 输出：返回值仍是手写拼接的 JSON 字符串（`*mut c_char`），与现有两个
//!   接口一致。
//! - RoeLocation 本身只是 roe 内部（纯 Rust 侧）用来传递/计算的结构体，
//!   不带 #[repr(C)]，因为它从不直接跨越 FFI 边界，边界上永远是"裸参数
//!   in / JSON out"。

/// 单行内移动命令（路径1，见 roe_cursor_selection_final_v1.md 3.2）。
/// 本次（v94）只实现 CharRight，其余打 todo!() 占位，链路打通后再补全
/// （v93/final 文档"七"建议的实现顺序：先用最简单的一个命令跑通全链路）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum CursorInlineCommand {
    CharLeft = 0,
    CharRight = 1,
    WordLeft = 2,
    WordRight = 3,
}

impl CursorInlineCommand {
    /// 从 C-ABI 裸 u32 参数解析。非法值返回 None，调用方应报错而非 panic。
    pub fn from_u32(v: u32) -> Option<Self> {
        match v {
            0 => Some(Self::CharLeft),
            1 => Some(Self::CharRight),
            2 => Some(Self::WordLeft),
            3 => Some(Self::WordRight),
            _ => None,
        }
    }
}

/// 容器类型：本次（v92）只支持 BODY（Writer 正文流）。TABLE_CELL /
/// TEXT_FRAME 是为将来扩展预留的值位，本次不实现其内部行为——如果
/// container_type 传入非 0 值，各接口应返回错误而非静默按 BODY 处理。
pub const CONTAINER_TYPE_BODY: u32 = 0;
#[allow(dead_code)]
pub const CONTAINER_TYPE_TABLE_CELL: u32 = 1;
#[allow(dead_code)]
pub const CONTAINER_TYPE_TEXT_FRAME: u32 = 2;

/// 光标/选区位置标识。
///
/// 现有 roe_hit_test / roe_query_layout_rect 用裸的 para_index
/// （WriterDocument::body 全局下标）+ char_offset，明确仅支持 Writer
/// 正文流，不支持表格 cell。本次不要求实现表格 cell 场景，但结构体设计
/// 预留了扩展空间（container_type/container_id 字段），避免将来支持
/// 表格时做破坏性重构。
///
/// 本次所有新增接口的位置参数一律用 RoeLocation 表示（FFI 层拆解为裸
/// 参数），即使 container_type 目前只会是 CONTAINER_TYPE_BODY。内部
/// 实现把 RoeLocation{container_type: BODY, local_para_index, char_offset}
/// 当成等价于现有的裸 para_index/char_offset 处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoeLocation {
    /// 0 = BODY（正文流，本次唯一支持的值）；1 = TABLE_CELL（预留，本次
    /// 不实现）；2 = TEXT_FRAME（预留，本次不实现）。
    pub container_type: u32,
    /// container_type = BODY 时无意义，固定填 0。
    pub container_id: u32,
    /// container 内部的段落索引。container_type = BODY 时，与现有
    /// roe_hit_test / roe_query_layout_rect 的 para_index 语义完全一致
    /// （WriterDocument::body 中的下标）。
    pub local_para_index: u32,
    /// 字节偏移，语义与现有 char_offset 一致：该段落所有 Run 拼接后
    /// full_text 上的字节偏移。
    pub char_offset: u32,
}

impl RoeLocation {
    /// 从现有的裸 para_index/char_offset（Writer 正文流场景）构造一个
    /// RoeLocation，container_type 固定为 BODY，container_id 固定为 0。
    ///
    /// 用于把新接口的输入参数适配到内部统一的 RoeLocation 表示，以及在
    /// 测试里验证新旧语义对齐（是否与 roe_hit_test / roe_query_layout_rect
    /// 的裸参数输出一致）。
    pub fn from_legacy(para_index: usize, char_offset: usize) -> Self {
        RoeLocation {
            container_type: CONTAINER_TYPE_BODY,
            container_id: 0,
            local_para_index: para_index as u32,
            char_offset: char_offset as u32,
        }
    }

    /// 是否是本次唯一支持的 BODY 容器类型。路径1/路径2/选区几何接口的
    /// 胶水函数应在入口处调用本方法校验，container_type 非 BODY 时应
    /// 返回错误（而不是静默当成 BODY 处理，也不是 panic）。
    pub fn is_body(&self) -> bool {
        self.container_type == CONTAINER_TYPE_BODY
    }

    /// 反向：BODY 场景下取回 (para_index, char_offset) 二元组，供内部
    /// 复用现有 hit_test.rs / layout_query.rs 里已经验证过的、按裸
    /// para_index/char_offset 工作的核心逻辑（query_rect_in_entry 等）。
    /// 调用前应先用 is_body() 确认，非 BODY 场景不应调用本方法
    /// （本次不实现 TABLE_CELL/TEXT_FRAME，调用方需要自行分流）。
    pub fn as_legacy_body(&self) -> (usize, usize) {
        debug_assert!(self.is_body(), "as_legacy_body 只应在 container_type=BODY 时调用");
        (self.local_para_index as usize, self.char_offset as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_legacy_sets_body_container() {
        let loc = RoeLocation::from_legacy(2, 15);
        assert_eq!(loc.container_type, CONTAINER_TYPE_BODY);
        assert_eq!(loc.container_id, 0);
        assert_eq!(loc.local_para_index, 2);
        assert_eq!(loc.char_offset, 15);
        assert!(loc.is_body());
    }

    #[test]
    fn test_roundtrip_legacy_to_location_and_back() {
        // 与现有 para_index/char_offset 语义对齐验证（final 文档"七.1"
        // 要求的"写一个内部转换函数...确认与现有接口输出对得上"）。
        for (para_index, char_offset) in [(0usize, 0usize), (3, 42), (999, 0)] {
            let loc = RoeLocation::from_legacy(para_index, char_offset);
            let (back_para, back_offset) = loc.as_legacy_body();
            assert_eq!(back_para, para_index, "para_index 往返应保持一致");
            assert_eq!(back_offset, char_offset, "char_offset 往返应保持一致");
        }
    }

    #[test]
    fn test_non_body_container_type_not_is_body() {
        let loc = RoeLocation {
            container_type: CONTAINER_TYPE_TABLE_CELL,
            container_id: 0,
            local_para_index: 0,
            char_offset: 0,
        };
        assert!(!loc.is_body(), "TABLE_CELL 容器类型不应被 is_body() 判定为 true");
    }
}
