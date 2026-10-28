//! 句柄表：thread_local HashMap<u64, DocumentEntry>
//!
//! 选择 thread_local（而非 Mutex 全局表）是因为 QML 主线程单线程调用，
//! 且 FontSystem 等渲染器状态本身大概率不是 Send。
//!
//! 13a 阶段只搭结构，13c 再加渲染器缓存字段。

#![allow(dead_code)]

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};

use roe_core::suspension::KernelEvent;
use roe_impress::animation::renderer::SlideRuntimeState;
use roe_impress::animation::scheduler::AnimationScheduler;
use roe_calc::CalcEngine;
use roe_parser::RoeDocument;
use roe_writer::layout::{LayoutResult, ParagraphLayoutEngine};
use roe_writer::FontRegistry;

/// 单张幻灯片的动画播放状态：scheduler 驱动 RuntimeTree，
/// runtime 是渲染覆盖层（ShapeRuntimeState 列表）。
/// 首次 roe_tick/roe_on_click 时惰性创建（build_from(animation_tree) + start()）。
pub struct SlideAnimationState {
    pub scheduler: AnimationScheduler,
    pub runtime: SlideRuntimeState,
}

/// 单个打开文档对应的条目。
/// Writer 文档显示选项（通过 roe_set_writer_options 设置）
#[derive(Debug, Clone)]
pub struct WriterOptions {
    /// 是否显示水平标尺
    pub ruler_enabled: bool,
    /// 标尺高度（pt），ruler_enabled=false 时忽略
    pub ruler_height_pt: f32,
    /// v265: 显示段落标记/手动换行标记（只影响渲染）
    pub show_marks: bool,
}

impl Default for WriterOptions {
    fn default() -> Self {
        Self {
            ruler_enabled: false,
            ruler_height_pt: 20.0,
            show_marks: false,
        }
    }
}

/// undo/redo 精确光标/焦点定位（v115 方案，阶段C v170 改为 enum）：
/// 每次编辑操作 push 快照进 undo_stack 时，同时记录"编辑发生前光标/
/// 选区/焦点在哪"，roe_undo/roe_redo 弹栈时把这个位置一并返回给前端。
///
/// 不暴露给 C-ABI 层的具体字段结构，只在 roe-api 内部使用（对外通过
/// to_json() 输出统一 JSON 形状）。改为 enum 的原因：原先的单一
/// struct 只覆盖 Writer 的段落/偏移量概念，Calc（单元格坐标）和
/// Impress（幻灯片/形状）场景被迫用 (0,0) 占位，语义不通。现在按
/// 文档类型分别建模，Rhai 脚本路径（阶段C接入 roe_execute_script）
/// 等没有明确光标概念的场景统一用 None，不再滥用数字占位。
///
/// Writer 字段命名与 RoeLocation（cursor.rs）的锚点/焦点概念对齐：
/// anchor_offset 是选区起点，focus_offset 是选区终点/光标位置；
/// 单点操作（如 insert_text）时两者相等。
///
/// 全部 variant 字段均为 Copy 类型（Impress.shape_id 用 Shape.id 的
/// 原生类型 u32，不用 String），使整个 enum 保留 Copy，undo_stack/
/// redo_stack 里 (RoeDocument, EditCursorContext) 的 ctx 部分继续
/// 廉价复制，不引入额外 clone 开销。
#[derive(Debug, Clone, Copy)]
pub enum EditCursorContext {
    Writer {
        para_index: usize,
        anchor_offset: usize,
        focus_offset: usize,
    },
    Calc {
        sheet_index: usize,
        row: usize,
        col: usize,
    },
    Impress {
        slide_index: usize,
        /// 对应 roe_model::impress::Shape.id（u32）。None 表示焦点
        /// 不在具体某个形状上（如刚选中幻灯片、还没选形状）。
        shape_id: Option<u32>,
    },
    /// 没有明确光标/焦点概念的场景：Rhai 脚本执行路径（阶段C起
    /// roe_execute_script 接入 undo_stack 时默认使用）、非 Writer
    /// 文档的 roe_commit_edit 手写事务对等。
    None,
}

impl EditCursorContext {
    /// 单点位置（光标不构成选区时用这个），anchor == focus。
    /// 保留原有签名不变——text_edit.rs 现有 8 处调用点无需改动。
    pub fn at(para_index: usize, offset: usize) -> Self {
        Self::Writer {
            para_index,
            anchor_offset: offset,
            focus_offset: offset,
        }
    }

    /// 区间位置（比如 replace_range 的 [range_start, range_end)）。
    /// 保留原有签名不变。
    pub fn range(para_index: usize, anchor_offset: usize, focus_offset: usize) -> Self {
        Self::Writer {
            para_index,
            anchor_offset,
            focus_offset,
        }
    }

    /// Calc 单元格坐标位置。sheet_index/row/col 均 0-based，与
    /// roe-calc 内部约定一致（Excel 1-based 转换由调用方负责）。
    pub fn calc_cell(sheet_index: usize, row: usize, col: usize) -> Self {
        Self::Calc { sheet_index, row, col }
    }

    /// Impress 幻灯片/形状焦点位置。
    pub fn impress_shape(slide_index: usize, shape_id: Option<u32>) -> Self {
        Self::Impress { slide_index, shape_id }
    }

    /// 序列化为 JSON 字符串，字段命名对齐 RoeLocation 系列接口现有的
    /// JSON 返回风格（见 text_edit.rs 各 xxx_in_entry 函数的返回值）。
    /// container_type 约定：0=Writer, 1=Calc, 2=Impress, 3=None
    /// （无明确焦点，前端/Agent 不需要恢复任何光标位置）。
    pub fn to_json(&self) -> String {
        match self {
            Self::Writer { para_index, anchor_offset, focus_offset } => format!(
                "{{\"container_type\":0,\"container_id\":0,\"local_para_index\":{},\"anchor_offset\":{},\"focus_offset\":{}}}",
                para_index, anchor_offset, focus_offset
            ),
            Self::Calc { sheet_index, row, col } => format!(
                "{{\"container_type\":1,\"container_id\":{},\"row\":{},\"col\":{}}}",
                sheet_index, row, col
            ),
            Self::Impress { slide_index, shape_id } => format!(
                "{{\"container_type\":2,\"container_id\":{},\"shape_id\":{}}}",
                slide_index,
                match shape_id {
                    Some(id) => id.to_string(),
                    None => "null".to_string(),
                }
            ),
            Self::None => "{\"container_type\":3}".to_string(),
        }
    }
}

pub struct DocumentEntry {
    pub doc: RoeDocument,
    /// roe_begin_edit 时克隆出的快照，commit 时清空，discard 时回滚。
    pub snapshot: Option<RoeDocument>,
    /// 是否为 Agent shadow 副本（roe-agent 在其上执行 Rhai 脚本预览、
    /// 用户确认后 commit 回原 handle）。普通用户打开的文档恒为 false；
    /// 只有 shadow.rs::create_shadow() 创建的 handle 才为 true。
    /// UI 层遍历"已打开文档列表"时应过滤 is_shadow == true 的条目，
    /// 避免影子副本被误认成用户正常打开的文档。
    pub is_shadow: bool,
    /// roe_poll_event 消费的挂起事件队列。
    pub events: VecDeque<KernelEvent>,
    /// 按 slide_idx 惰性持有的动画播放状态（仅 Impress 文档使用）。
    pub animation: HashMap<usize, SlideAnimationState>,
    /// Writer 布局缓存（roe_hit_test / roe_query_layout_rect 依赖这个，
    /// 避免每次查询都重新跑一遍 cosmic-text shaping + 分页，支持拖拽选区
    /// 这种高频调用场景）。
    /// None 表示缓存未建立或已失效（文档被编辑后应调用
    /// invalidate_writer_layout_cache 使其失效），下次查询时重新 layout()。
    /// 仅 Writer 文档使用；Calc/Impress 文档此字段始终为 None。
    pub writer_layout_cache: Option<LayoutResult>,
    /// 配套 writer_layout_cache 的 FontRegistry 缓存。重新 shape 一行文字
    /// （roe_hit_test / roe_query_layout_rect 的精确 x 坐标计算）需要
    /// FontSystem，与布局缓存同生命周期：建立 writer_layout_cache 时一起
    /// 建立，invalidate 时也应一起清空（当前 invalidate_writer_layout_cache
    /// 已同步清空，见该函数实现）。
    /// 仅 Writer 文档使用；Calc/Impress 文档此字段始终为 None。
    pub writer_font_registry: Option<FontRegistry>,
    /// 撤销栈：commit_edit 时把提交前的旧文档 push 进来，最旧的在栈底。
    pub undo_stack: Vec<(RoeDocument, EditCursorContext)>,
    /// 重做栈：roe_undo 弹出 undo_stack 时，把当前文档 push 进来；
    /// 任何非 undo/redo 触发的新提交会清空这个栈（标准线性历史行为，不保留分支）。
    pub redo_stack: Vec<(RoeDocument, EditCursorContext)>,
    /// Writer 显示选项（标尺等），默认值见 WriterOptions::default()
    pub writer_options: WriterOptions,
    /// Impress viewport 渲染专用字体资源缓存，用途同上（v86任务A）。
    pub impress_font_registry: Option<parley::FontContext>,
    /// Writer viewport 渲染专用 Parley FontContext，跨帧复用，避免每次
    /// FontContext::new() 重新扫描系统字体。配合 WriterTileRenderer
    /// （已迁移到 Parley）使用。
    pub writer_parley_font_context: Option<parley::FontContext>,
    /// Calc 公式重算引擎（v104 新增）：持有 FormulaDAG 依赖图，跨多次
    /// roe_execute_script 调用复用，避免每次脚本执行都重建整个依赖图。
    /// 仅 Calc 文档使用；Writer/Impress 文档此字段始终为 None。
    /// 首次执行 Calc 脚本时惰性创建（roe_execute_script 里 get_or_insert）。
    pub calc_engine: Option<CalcEngine>,
    /// 批注气泡功能第一阶段（Slint C-ABI接口，v157第三节后续实现）：
    /// 每次 roe_render_writer_viewport 渲染完成后，把 WriterTileRenderer::
    /// take_comment_hits() 的结果存到这里，供 roe_take_comment_hits 导出
    /// 函数读取。语义与来源数据完全一致：只覆盖"最近一次渲染的viewport"
    /// 范围内可见的批注命中，不是全文档批注坐标（全文档扫描是独立的、
    /// 本次未实现的后续功能，见 ROE_Progress_v158 讨论）。
    /// 仅 Writer 文档使用；Calc/Impress 文档此字段始终为空 Vec。
    pub pending_comment_hits: Vec<(u32, f32, f32, u8)>,
}

/// 撤销栈深度上限：超过后从栈底（最旧的一条）丢弃，防止长时间编辑
/// 会话内存无限增长。50 步是初版经验值，后续如有需要可调整或做成
/// 可配置项。
///
/// 阶段C v170 重构：此前 snapshot.rs 和 text_edit.rs 各自独立定义了
/// 一份同名同值常量（技术债，text_edit.rs 那份的注释也承认是"与
/// snapshot.rs 保持一致"的手动同步）。现在统一到这里，作为唯一定义，
/// 两个调用方改为引用 handle::UNDO_STACK_LIMIT。
pub const UNDO_STACK_LIMIT: usize = 50;

/// 撤销栈所有快照的单元格总预算(按实测约 550 字节/格,300 万格约 1.6GB)。
pub const UNDO_CELL_BUDGET: usize = 3_000_000;

/// 一份快照占用的单元格数;Writer/Impress 不按格计,返回 0。
pub fn snapshot_cells(doc: &RoeDocument) -> usize {
    match doc {
        RoeDocument::Calc(wb) => wb.sheets.iter().map(|s| s.cells.cell_count()).sum(),
        _ => 0,
    }
}

impl DocumentEntry {
    /// push 一份"编辑前"的文档快照进 undo_stack，并按标准线性历史
    /// 语义清空 redo_stack（新的提交发生后，之前 undo 出来的"未来"
    /// 分支不再有效，不保留分支）。
    ///
    /// 阶段C v170 重构：此前这四步操作（push -> 检查长度 -> 超限则
    /// 移除最旧一条 -> 清空redo_stack）在 snapshot.rs::roe_commit_edit
    /// 和 text_edit.rs 的 8 个 xxx_in_entry 函数里各自重复实现了一遍
    /// （共9处，逻辑完全一致），收进这个方法统一维护。后续 roe_execute_
    /// script 接入 undo/redo（阶段C Step 2b）时复用同一方法，不再引入
    /// 第10份重复实现。
    pub fn push_undo_snapshot(&mut self, prev: RoeDocument, ctx: EditCursorContext) {
        if undo_group_should_skip() { return; }
        self.undo_stack.push((prev, ctx));
        if self.undo_stack.len() > UNDO_STACK_LIMIT {
            self.undo_stack.remove(0);
        }
        // 按总格数预算丢最旧的快照,至少保留最近 1 条。
        let mut total: usize = self.undo_stack.iter().map(|(d, _)| snapshot_cells(d)).sum();
        while total > UNDO_CELL_BUDGET && self.undo_stack.len() > 1 {
            let (old, _) = self.undo_stack.remove(0);
            total -= snapshot_cells(&old);
        }
        self.redo_stack.clear();
    }
}

struct HandleTable {
    next_id: u64,
    entries: HashMap<u64, DocumentEntry>,
}

impl HandleTable {
    fn new() -> Self {
        // id 从 1 开始自增，0 表示无效/失败句柄。
        Self { next_id: 1, entries: HashMap::new() }
    }
}

thread_local! {
    static HANDLES: RefCell<HandleTable> = RefCell::new(HandleTable::new());
}

/// 插入新文档条目，返回分配的句柄（>= 1）。
pub fn insert(entry: DocumentEntry) -> u64 {
    HANDLES.with(|h| {
        let mut h = h.borrow_mut();
        let id = h.next_id;
        h.next_id += 1;
        h.entries.insert(id, entry);
        id
    })
}

/// 移除并返回文档条目；句柄不存在返回 None。
pub fn remove(handle: u64) -> Option<DocumentEntry> {
    // 数据集随句柄一起释放(关闭文档 / shadow 销毁都走这里)。
    DATASET_STORES.with(|s| { s.borrow_mut().remove(&handle); });
    HANDLES.with(|h| h.borrow_mut().entries.remove(&handle))
}

thread_local! {
    /// 会话级 DataFrame 数据集的句柄侧表(按文档归属)。
    /// 不放进 DocumentEntry 是为避免 40+ 处构造点连带修改。
    static DATASET_STORES: std::cell::RefCell<
        std::collections::HashMap<u64, roe_script::calc_df_api::DatasetStore>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
}

/// 取出句柄的数据集容器(没有则返回空容器)。用法:take → run_calc_ds → put_datasets。
pub fn take_datasets(handle: u64) -> roe_script::calc_df_api::DatasetStore {
    DATASET_STORES.with(|s| s.borrow_mut().remove(&handle).unwrap_or_default())
}

/// 放回句柄的数据集容器(空容器不占表项)。
pub fn put_datasets(handle: u64, ds: roe_script::calc_df_api::DatasetStore) {
    DATASET_STORES.with(|s| {
        let mut s = s.borrow_mut();
        if ds.is_empty() { s.remove(&handle); } else { s.insert(handle, ds); }
    });
}

/// 把 from 句柄的数据集复制给 to 句柄(DataFrame clone 为 Arc 级,很便宜)。
pub fn copy_datasets(from: u64, to: u64) {
    DATASET_STORES.with(|s| {
        let mut s = s.borrow_mut();
        match s.get(&from).cloned() {
            Some(ds) => { s.insert(to, ds); }
            None => { s.remove(&to); }
        }
    });
}

/// 对指定句柄的条目执行操作；句柄不存在返回 None。
pub fn with_entry<R>(handle: u64, f: impl FnOnce(&mut DocumentEntry) -> R) -> Option<R> {
    HANDLES.with(|h| h.borrow_mut().entries.get_mut(&handle).map(f))
}

/// 使指定句柄的 Writer 布局缓存失效。
/// 文档内存模型被编辑后（roe_commit_edit / executeScript 等改动 doc.body
/// 的操作完成后）应调用此函数，否则后续 hit_test / query_layout_rect
/// 会基于过时的布局数据返回错误结果。
/// 句柄不存在时静默忽略（不是错误，调用方不需要检查返回值）。
///
/// 现状说明（A块范围）：本函数已实现且可独立调用/测试，但尚未在
/// roe-api 的编辑类入口（script.rs::roe_execute_script、
/// snapshot.rs::roe_commit_edit 等）中接入调用。这是有意为之——A块只
/// 负责搭好缓存基础设施本身，接入编辑流程触发失效是后续任务，接入时
/// 只需在对应位置加一行 invalidate_writer_layout_cache(handle) 调用。
pub fn invalidate_writer_layout_cache(handle: u64) {
    with_entry(handle, |entry| {
        entry.writer_layout_cache = None;
        // FontRegistry 缓存与布局缓存同生命周期，一起失效，避免出现
        // "布局缓存已清空但字体缓存还留着"这种不一致的中间状态（虽然
        // 字体本身不会因为文档编辑而改变，但保持两者同步失效更简单、
        // 不容易引入难以调试的边界 bug）。
        entry.writer_font_registry = None;
    });
}

/// 获取或构建指定句柄的 Writer 布局缓存。
/// 仅适用于 RoeDocument::Writer 文档；句柄不存在或文档类型不是 Writer
/// 时返回 false。成功时保证调用后 entry.writer_layout_cache 是 Some。
/// 已有有效缓存时直接返回 true，不重新排版（单次 with_entry 内完成
/// 判断+构建，避免对同一句柄连续多次借用）。
pub fn ensure_writer_layout(handle: u64) -> bool {
    with_entry(handle, |entry| {
        if entry.writer_layout_cache.is_some() {
            return true; // 已有缓存，直接复用
        }
        match &entry.doc {
            RoeDocument::Writer(doc) => {
                let registry = FontRegistry::new_with_system_fonts();
                let mut engine = ParagraphLayoutEngine::new(registry);
                let result = engine.layout(doc);
                entry.writer_layout_cache = Some(result);
                // FontRegistry 随布局缓存一起持有：B 块升级为"方案B：重新
                // shape 拿精确 glyph x 坐标"时需要同一份 FontSystem 重新
                // shape 一行文字，避免每次查询都重新扫描系统字体（开销大，
                // 在拖拽选区高频查询场景下不可接受）。
                entry.writer_font_registry = Some(engine.registry);
                true
            }
            _ => false, // 不是 Writer 文档，无法建立布局缓存
        }
    }).unwrap_or(false) // 句柄不存在
}

#[cfg(test)]
mod tests_writer_layout_cache {
    use super::*;
    use roe_parser::odf::fodt::FodtParser;

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
      <text:p text:style-name="Default">缓存测试段落。</text:p>
    </office:text>
  </office:body>
</office:document>"#;

    fn open_writer() -> u64 {
        let doc = FodtParser::parse(SAMPLE_FODT).expect("parse failed");
        insert(DocumentEntry {
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

    #[test]
    fn test_cache_starts_empty() {
        let h = open_writer();
        let has_cache = with_entry(h, |e| e.writer_layout_cache.is_some()).unwrap();
        assert!(!has_cache, "新打开的文档不应该有缓存");
        remove(h);
    }

    #[test]
    fn test_ensure_builds_cache() {
        let h = open_writer();
        let ok = ensure_writer_layout(h);
        assert!(ok, "ensure_writer_layout 应该成功");
        let has_cache = with_entry(h, |e| e.writer_layout_cache.is_some()).unwrap();
        assert!(has_cache, "调用后应该有缓存");
        remove(h);
    }

    #[test]
    fn test_ensure_reuses_existing_cache() {
        let h = open_writer();
        assert!(ensure_writer_layout(h));
        let first_frame_count = with_entry(h, |e| {
            e.writer_layout_cache.as_ref().unwrap().arena.frames.len()
        }).unwrap();

        assert!(ensure_writer_layout(h));
        let second_frame_count = with_entry(h, |e| {
            e.writer_layout_cache.as_ref().unwrap().arena.frames.len()
        }).unwrap();
        assert_eq!(first_frame_count, second_frame_count);
        remove(h);
    }

    #[test]
    fn test_invalidate_clears_cache() {
        let h = open_writer();
        assert!(ensure_writer_layout(h));
        assert!(with_entry(h, |e| e.writer_layout_cache.is_some()).unwrap());

        invalidate_writer_layout_cache(h);
        let has_cache = with_entry(h, |e| e.writer_layout_cache.is_some()).unwrap();
        assert!(!has_cache, "invalidate 后缓存应该被清空");
        remove(h);
    }

    #[test]
    fn test_ensure_fails_for_non_writer_doc() {
        use roe_model::calc::{SparseGrid, Sheet, Workbook};
        let wb = Workbook {
            sheets: vec![Sheet {
                col_style: vec![],
                name: "Sheet1".into(),
                cells: SparseGrid::default(),
                col_widths: vec![],
                row_heights: vec![],
                frozen_rows: 0,
                frozen_cols: 0,
                cell_formats: std::collections::HashMap::new(),
                cond_formats: vec![],
                data_validations: vec![],
                auto_filter: None,
                merged_cells: vec![],
                pivot:        None,
                ..Default::default()
            }],
            named_ranges: HashMap::new(),
            named_formulas: HashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        };
        let h = insert(DocumentEntry {
            doc: RoeDocument::Calc(wb),
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
        });
        let ok = ensure_writer_layout(h);
        assert!(!ok, "Calc 文档不应该能建立 writer 布局缓存");
        remove(h);
    }

    #[test]
    fn test_invalidate_invalid_handle_does_not_panic() {
        invalidate_writer_layout_cache(999_999);
    }

    #[test]
    fn test_ensure_invalid_handle_returns_false() {
        assert!(!ensure_writer_layout(999_999));
    }
}

// ───────── v240: 撤销分组 ─────────
// 0=未分组 1=分组中且尚未存快照 2=分组中且已存过快照（后续快照跳过）
thread_local! {
    static UNDO_GROUP: std::cell::Cell<u8> = std::cell::Cell::new(0);
}

pub fn undo_group_begin() { UNDO_GROUP.with(|g| g.set(1)); }
pub fn undo_group_end() { UNDO_GROUP.with(|g| g.set(0)); }

/// 分组中第一次返回 false（正常存快照），之后返回 true（跳过）
fn undo_group_should_skip() -> bool {
    UNDO_GROUP.with(|g| match g.get() {
        0 => false,
        1 => { g.set(2); false }
        _ => true,
    })
}
