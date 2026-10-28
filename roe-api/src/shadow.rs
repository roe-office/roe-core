//! Agent 影子副本（Shadow Handle）机制。
//!
//! 背景：Agent（roe-agent，通过 roe_execute_script 驱动 Rhai 脚本）
//! 修改文档前，需要先在一份完全独立的副本上试跑，原文档必须在用户
//! 确认前保持不变——这与 entry.snapshot（roe_begin_edit/commit_edit/
//! discard_edit，用于用户直接编辑原文档时的可回滚事务）语义不同，
//! 不能复用同一套：snapshot 方案下 entry.doc 本身在编辑期间已经被
//! 改动，无法满足"原文档不变、副本先跑、确认后才生效"的要求。
//!
//! 设计（v189之后的 roe-agent 骨架首个决策，经方案讨论定案）：
//! shadow 副本是一个完整独立的 DocumentEntry，通过 handle::insert()
//! 分配全新 handle 承载，is_shadow: bool 字段标记，不对现有 u64
//! 句柄做位掩码（避免几十个 C-ABI 入口都要拆解句柄）。
//!
//! 生命周期由 ShadowGuard（RAII）管理：无论 Agent 脚本执行成功、
//! 失败还是 panic，Drop 都保证 shadow handle 被清理，不会泄漏。
//!
//! commit 使用 mem::swap 而非 clone-replace：零拷贝，且把 swap 前
//! 原 handle 的旧文档 push 进原 handle 的 undo_stack，让"接受 Agent
//! 的修改"本身成为一条可撤销的记录（Ctrl+Z 能整体撤销 Agent 的操作）。

use std::mem;

use roe_parser::RoeDocument;

use crate::handle::{self, DocumentEntry, EditCursorContext};

/// 影子副本句柄的 RAII 守卫。
///
/// Drop 时自动调用 handle::remove(shadow_handle) 清理影子文档，
/// 无论调用方是显式 discard、忘记处理、还是中途因错误提前返回——
/// 只要 guard 离开作用域，shadow handle 就不会泄漏在 thread_local
/// 句柄表里。
///
/// commit_shadow(guard) 会消费 guard（拿走所有权），内部完成 swap
/// 后 guard 被 drop，同样触发清理。
pub struct ShadowGuard {
    original_handle: u64,
    shadow_handle: u64,
    /// 标记 shadow handle 是否已被清理（commit_shadow 内部在 swap
    /// 之后会先手动 remove 一次，避免 Drop 再重复 remove 一次不存在
    /// 的 handle——重复 remove 本身无害（返回 None），但用这个标记
    /// 让意图更明确，也便于未来加调试断言）。
    cleaned: bool,
}

impl ShadowGuard {
    /// 影子副本的 handle，供调用方对其执行 Rhai 脚本
    /// （roe_execute_script）或走现有 C-ABI 渲染/查询接口做预览。
    pub fn shadow_handle(&self) -> u64 {
        self.shadow_handle
    }

    /// 影子副本对应的原始文档 handle。
    pub fn original_handle(&self) -> u64 {
        self.original_handle
    }
}

impl Drop for ShadowGuard {
    fn drop(&mut self) {
        if !self.cleaned {
            handle::remove(self.shadow_handle);
            self.cleaned = true;
        }
    }
}

/// 为 `original_handle` 创建一个影子副本。
///
/// 返回 None：原始句柄无效。
///
/// 影子副本是原文档的完整 clone，装进一个全新的、is_shadow: true
/// 的 DocumentEntry。其余字段（undo_stack、animation 等）均从空白
/// 状态开始——影子副本不继承原文档的撤销历史，它本身在 Agent 多步
/// 操作期间可以独立累积自己的 undo_stack（如果 roe_execute_script
/// 之类的路径对着 shadow_handle 调用）。
pub fn create_shadow(original_handle: u64) -> Option<ShadowGuard> {
    let doc_clone = handle::with_entry(original_handle, |entry| entry.doc.clone())?;

    let shadow_entry = DocumentEntry {
        doc: doc_clone,
        snapshot: None,
        is_shadow: true,
        events: Default::default(),
        animation: Default::default(),
        writer_layout_cache: None,
        writer_font_registry: None,
        undo_stack: Vec::new(),
        redo_stack: Vec::new(),
        writer_options: Default::default(),
        impress_font_registry: None,
        writer_parley_font_context: None,
        calc_engine: None,
        pending_comment_hits: Vec::new(),
    };

    let shadow_handle = handle::insert(shadow_entry);
    // 影子继承原文档的会话级数据集(DataFrame clone 为 Arc 级,开销很小)。
    handle::copy_datasets(original_handle, shadow_handle);

    Some(ShadowGuard {
        original_handle,
        shadow_handle,
        cleaned: false,
    })
}

/// 提交影子副本：将影子文档写回原文档，零拷贝（mem::swap），并把
/// 原文档（提交前的旧内容）push 进原 handle 的 undo_stack，使这次
/// 提交整体可被一次 undo 撤销。
///
/// 返回 true 表示提交成功；false 表示 original_handle 或
/// shadow_handle 已失效（理论上不应发生——shadow_handle 由 guard
/// 独占持有且 guard 尚未 drop；original_handle 失效则可能是原文档
/// 在 Agent 执行期间被外部关闭，属异常情况）。
///
/// 消费 guard：commit 完成后 shadow handle 必须被清理，不能再被
/// 误用，用所有权转移在编译期保证这一点。
pub fn commit_shadow(mut guard: ShadowGuard) -> bool {
    let swapped = (|| {
        // 先取出 shadow 的文档内容，避免同时对两个 handle 做嵌套
        // borrow（HANDLES 是 thread_local RefCell<HashMap<...>>，
        // 两次 with_entry 嵌套调用会 panic）。
        let shadow_doc: RoeDocument =
            handle::with_entry(guard.shadow_handle, |entry| entry.doc.clone())?;

        handle::with_entry(guard.original_handle, |entry| {
            let mut incoming = shadow_doc;
            mem::swap(&mut entry.doc, &mut incoming);
            // swap 后 incoming 持有的是原文档提交前的旧内容，push
            // 进 undo_stack；ctx 用 None——Agent 提交没有明确的
            // 光标/选区语境，语义上等同于 roe_execute_script 现有
            // 用法。
            entry.push_undo_snapshot(incoming, EditCursorContext::None);
            // P0-c/P0-d 修复:doc 被整体替换后,旧引擎的 DAG 与缓存已失效,
            // 丢弃并全量重算(复用打开文档时的同一入口)。
            let fresh = crate::document::init_calc_engine_for_doc(&mut entry.doc);
            entry.calc_engine = fresh;
        })
    })();

    // swap 成功时,把影子里的数据集回写原 handle(必须在 remove 之前,
    // 否则影子的数据集随句柄一起释放)。
    if swapped.is_some() {
        handle::copy_datasets(guard.shadow_handle, guard.original_handle);
    }

    // 无论 swap 是否成功，shadow handle 都应立即清理（不必等
    // guard drop）：commit 已经是终态操作，不应再保留 shadow。
    handle::remove(guard.shadow_handle);
    guard.cleaned = true;

    swapped.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use roe_model::writer::WriterDocument;
    use std::collections::VecDeque;

    fn open_test_writer_doc() -> u64 {
        handle::insert(DocumentEntry {
            doc: RoeDocument::Writer(WriterDocument::default()),
            snapshot: None,
            is_shadow: false,
            events: VecDeque::new(),
            animation: Default::default(),
            writer_layout_cache: None,
            writer_font_registry: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            writer_options: Default::default(),
            impress_font_registry: None,
            writer_parley_font_context: None,
            calc_engine: None,
            pending_comment_hits: Vec::new(),
        })
    }

    #[test]
    fn create_shadow_returns_none_for_invalid_handle() {
        assert!(create_shadow(999_999).is_none());
    }

    #[test]
    fn create_shadow_clones_doc_into_new_handle() {
        let h = open_test_writer_doc();
        let guard = create_shadow(h).expect("shadow should be created for valid handle");

        assert_ne!(guard.shadow_handle(), h, "shadow handle must differ from original");
        assert_eq!(guard.original_handle(), h);

        // shadow handle 应该能被正常 with_entry 访问到，且 is_shadow 为 true
        let is_shadow = handle::with_entry(guard.shadow_handle(), |e| e.is_shadow).unwrap();
        assert!(is_shadow);

        handle::remove(h);
    }

    #[test]
    fn drop_guard_without_commit_cleans_up_shadow_handle() {
        let h = open_test_writer_doc();
        let shadow_handle = {
            let guard = create_shadow(h).unwrap();
            let sh = guard.shadow_handle();
            assert!(handle::with_entry(sh, |_| ()).is_some(), "shadow should exist while guard alive");
            sh
            // guard 在此处离开作用域，触发 Drop
        };

        assert!(
            handle::with_entry(shadow_handle, |_| ()).is_none(),
            "shadow handle should be removed after guard drop (discard semantics)"
        );

        // 原文档不受影响
        assert!(handle::with_entry(h, |_| ()).is_some());
        handle::remove(h);
    }

    #[test]
    fn commit_shadow_modifies_original_and_cleans_shadow() {
        let h = open_test_writer_doc();
        let guard = create_shadow(h).unwrap();
        let shadow_handle = guard.shadow_handle();

        // 模拟 Agent 在 shadow 上做了修改：往 body 插入一个段落
        handle::with_entry(shadow_handle, |entry| {
            if let RoeDocument::Writer(doc) = &mut entry.doc {
                doc.body.push(roe_model::writer::BodyElement::Paragraph(Default::default()));
            }
        });

        let before_len = handle::with_entry(h, |entry| match &entry.doc {
            RoeDocument::Writer(doc) => doc.body.len(),
            _ => unreachable!(),
        })
        .unwrap();

        let ok = commit_shadow(guard);
        assert!(ok, "commit should succeed for valid handles");

        // shadow handle 已被清理
        assert!(handle::with_entry(shadow_handle, |_| ()).is_none());

        // 原文档已经变成 shadow 修改后的内容
        let (after_len, undo_len) = handle::with_entry(h, |entry| {
            let len = match &entry.doc {
                RoeDocument::Writer(doc) => doc.body.len(),
                _ => unreachable!(),
            };
            (len, entry.undo_stack.len())
        })
        .unwrap();

        assert_eq!(after_len, before_len + 1, "original doc should now have the shadow's extra paragraph");
        assert_eq!(undo_len, 1, "commit should push exactly one undo snapshot");

        // undo_stack 里那份应该是提交前的旧内容（段落数 = before_len）
        let undo_doc_len = handle::with_entry(h, |entry| match &entry.undo_stack[0].0 {
            RoeDocument::Writer(doc) => doc.body.len(),
            _ => unreachable!(),
        })
        .unwrap();
        assert_eq!(undo_doc_len, before_len, "undo snapshot should hold pre-commit content");

        handle::remove(h);
    }

    #[test]
    fn commit_shadow_fails_gracefully_if_original_removed_mid_flight() {
        let h = open_test_writer_doc();
        let guard = create_shadow(h).unwrap();
        let shadow_handle = guard.shadow_handle();

        // 模拟原文档在 Agent 执行期间被外部关闭（异常场景）
        handle::remove(h);

        let ok = commit_shadow(guard);
        assert!(!ok, "commit should fail if original handle is gone");

        // shadow handle 无论如何都应被清理，不能因为 commit 失败而泄漏
        assert!(handle::with_entry(shadow_handle, |_| ()).is_none());
    }
}
