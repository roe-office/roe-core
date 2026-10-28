//! roe_tick / roe_on_click —— Impress 动画播放
//!
//! 设计要点（13g）：
//! - DocumentEntry::animation 按 slide_idx 惰性持有 SlideAnimationState
//!   （AnimationScheduler + SlideRuntimeState）。首次对某个 slide_idx
//!   调用 roe_tick/roe_on_click 时，build_from(slide.animation_tree) +
//!   scheduler.start() + SlideRuntimeState::from_shapes(&slide.shapes)。
//! - 【对原 13d/13e/13f 占位签名的修正】原 todo!() 签名是
//!   `roe_tick(handle, delta_ms)` / `roe_on_click(handle, shape_id)`，
//!   没有 slide_idx。但 Impress 演示文稿每张幻灯片有独立的
//!   animation_tree/运行时状态，必须按 slide 区分，因此两个函数都
//!   加了 `slide_idx: u64` 参数。
//! - roe_on_click 内部实现为 `scheduler.on_click(shape_id)` 后立即
//!   `scheduler.tick(0.0)`：AnimationScheduler::on_click 本身不直接
//!   返回 Vec<AnimationFrame>，需要紧跟一次 0ms tick 来收集点击触发
//!   的首帧（v1 近似：假定点击效果在 0ms 产生首帧）。
//! - roe_render_page 与这里的 SlideRuntimeState 集成（13c-4 当时传
//!   `None`）留给后续：可以在 roe_tick/roe_on_click 之后，前端重新
//!   调用 roe_render_page 时由 roe-api 内部查 entry.animation 传入
//!   `Some(&state.runtime)`，本次先把状态管线打通。

use std::ffi::CString;
use std::os::raw::c_char;

use roe_impress::animation::renderer::SlideRuntimeState;
use roe_impress::animation::runtime_node::{AnimationFrame, RuntimeTree};
use roe_impress::animation::scheduler::AnimationScheduler;
use roe_parser::RoeDocument;

use crate::error::set_last_error;
use crate::handle::{self, DocumentEntry, SlideAnimationState};

/// 确保 entry.animation[slide_idx] 存在；首次调用时构建并 start()。
fn ensure_slide_state(entry: &mut DocumentEntry, slide_idx: usize) -> Result<(), String> {
    let pres = match &entry.doc {
        RoeDocument::Impress(pres) => pres,
        _ => return Err("not an Impress document".to_string()),
    };
    if slide_idx >= pres.slides.len() {
        return Err(format!(
            "slide_idx {slide_idx} out of range (slide_count={})",
            pres.slides.len()
        ));
    }
    entry.animation.entry(slide_idx).or_insert_with(|| {
        let slide = &pres.slides[slide_idx];
        let tree = RuntimeTree::build_from(&slide.animation_tree);
        let mut scheduler = AnimationScheduler::new(tree);
        scheduler.start();
        let runtime = SlideRuntimeState::from_shapes(&slide.shapes);
        SlideAnimationState { scheduler, runtime }
    });
    Ok(())
}

fn frames_to_json(frames: &[AnimationFrame]) -> *mut c_char {
    let json = serde_json::to_string(frames).unwrap_or_else(|_| "[]".to_string());
    CString::new(json)
        .map(CString::into_raw)
        .unwrap_or_else(|_| std::ptr::null_mut())
}

/// 推进动画时间轴 delta_ms，返回本次产生的 AnimationFrame 列表 JSON
/// （数组，可能为空数组 `[]`）。失败（句柄/slide_idx/文档类型不对）
/// 返回 null，详情见 roe_last_error_message。
/// 返回值需要 [`crate::roe_free_string`] 释放。
#[no_mangle]
pub extern "C" fn roe_tick(handle: u64, slide_idx: u64, delta_ms: f64) -> *mut c_char {
    let slide_idx = slide_idx as usize;
    let result = handle::with_entry(handle, |entry| {
        ensure_slide_state(entry, slide_idx)?;
        let state = entry.animation.get_mut(&slide_idx).unwrap();
        let frames = state.scheduler.tick(delta_ms);
        state.runtime.apply_frames(&frames);
        Ok::<Vec<AnimationFrame>, String>(frames)
    });

    match result {
        Some(Ok(frames)) => frames_to_json(&frames),
        Some(Err(e)) => {
            set_last_error(format!("roe_tick: {e}"));
            std::ptr::null_mut()
        }
        None => {
            set_last_error(format!("roe_tick: invalid handle {handle}"));
            std::ptr::null_mut()
        }
    }
}

/// 用户点击事件。shape_id < 0 表示未点击具体形状（推进 MainSequence）。
/// 返回值/错误处理同 [`roe_tick`]。
#[no_mangle]
pub extern "C" fn roe_on_click(handle: u64, slide_idx: u64, shape_id: i64) -> *mut c_char {
    let slide_idx = slide_idx as usize;
    let shape = if shape_id < 0 { None } else { Some(shape_id as u32) };

    let result = handle::with_entry(handle, |entry| {
        ensure_slide_state(entry, slide_idx)?;
        let state = entry.animation.get_mut(&slide_idx).unwrap();
        state.scheduler.on_click(shape);
        let frames = state.scheduler.tick(0.0);
        state.runtime.apply_frames(&frames);
        Ok::<Vec<AnimationFrame>, String>(frames)
    });

    match result {
        Some(Ok(frames)) => frames_to_json(&frames),
        Some(Err(e)) => {
            set_last_error(format!("roe_on_click: {e}"));
            std::ptr::null_mut()
        }
        None => {
            set_last_error(format!("roe_on_click: invalid handle {handle}"));
            std::ptr::null_mut()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handle::WriterOptions;
    use crate::roe_free_string;
    use roe_model::impress::{Presentation, Slide};
    use std::ffi::CStr;

    /// 构造一个最小 Impress 文档：1 张空白 slide（无 shape、无动画树子节点）。
    fn make_impress_handle(slide: Slide) -> u64 {
        let pres = Presentation {
            styles: roe_model::style::StyleRegistry::default(),
            slides: vec![slide],
            master_slides: Default::default(),
            slide_size: (960.0, 540.0), theme_colors: Default::default(), default_text_style: Default::default() };
        handle::insert(DocumentEntry {
            doc: RoeDocument::Impress(pres),
            snapshot: None,
            is_shadow: false,
            events: Default::default(),
            animation: Default::default(),
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

    fn json_from(ptr: *mut c_char) -> String {
        assert!(!ptr.is_null());
        let s = unsafe { CStr::from_ptr(ptr) }.to_str().unwrap().to_string();
        roe_free_string(ptr);
        s
    }

    #[test]
    fn test_tick_invalid_handle() {
        assert!(roe_tick(999_999, 0, 16.0).is_null());
    }

    #[test]
    fn test_on_click_invalid_handle() {
        assert!(roe_on_click(999_999, 0, -1).is_null());
    }

    #[test]
    fn test_tick_slide_idx_out_of_range() {
        let slide = Slide { layout_idx: None,
            shapes: vec![],
            master_idx: None,
            animation_tree: roe_model::impress::AnimationNode::TimingRoot { children: vec![] },
            notes: None,
            transition: None,
            background: None,
            layout_shapes: vec![],
        };
        let h = make_impress_handle(slide);
        assert!(roe_tick(h, 5, 16.0).is_null());
    }

    #[test]
    fn test_tick_empty_timeline_returns_empty_array() {
        let slide = Slide { layout_idx: None,
            shapes: vec![],
            master_idx: None,
            animation_tree: roe_model::impress::AnimationNode::TimingRoot { children: vec![] },
            notes: None,
            transition: None,
            background: None,
            layout_shapes: vec![],
        };
        let h = make_impress_handle(slide);
        let s = json_from(roe_tick(h, 0, 16.0));
        assert_eq!(s, "[]");

        // 第二次 tick：状态已惰性创建，应继续正常返回
        let s2 = json_from(roe_tick(h, 0, 16.0));
        assert_eq!(s2, "[]");
    }

    #[test]
    fn test_on_click_empty_timeline_returns_empty_array() {
        let slide = Slide { layout_idx: None,
            shapes: vec![],
            master_idx: None,
            animation_tree: roe_model::impress::AnimationNode::TimingRoot { children: vec![] },
            notes: None,
            transition: None,
            background: None,
            layout_shapes: vec![],
        };
        let h = make_impress_handle(slide);
        let s = json_from(roe_on_click(h, 0, -1));
        assert_eq!(s, "[]");

        let s2 = json_from(roe_on_click(h, 0, 3));
        assert_eq!(s2, "[]");
    }
}
