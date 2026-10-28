//! 媒体资产读取能力的桥接层。
//!
//! 背景：roe-script 是最底层 crate，不能依赖 roe-agent（依赖方向
//! 只能是 roe-agent → roe-api → roe-script，反过来会让 roe-script
//! 被迫拉入 rig-core/tokio 等一整套 agent 专用依赖，参见
//! ROE_Agent_Workspace_设计文档_v1.md 的架构约束）。
//!
//! 但 set_slide_background 的 picture/picture_tiled 分支（定义在
//! impress_api.rs）需要"给一个虚拟路径字符串，读出文件字节"这个
//! 能力，而这个能力的真正实现（WorkspaceContext，基于 cap-std 的
//! 沙盒）活在 roe-agent 里。
//!
//! 解法：本文件只定义一个极小的 trait（MediaAssetSource），不知道
//! WorkspaceContext 的存在。roe-agent 那边让 WorkspaceContext 实现
//! 这个 trait，在调用 execute_script_impl 时把 &dyn MediaAssetSource
//! 传下来。传递机制沿用本 crate 里 impress_api.rs::with_presentation
//! /calc_api.rs::WORKBOOK 等已有的 thread_local 裸指针模式，保持
//! 项目内一致的工程习惯，不引入新的传递机制。

use std::cell::RefCell;

/// 读取虚拟路径对应的媒体文件内容。virtual_path 形如 "/media/xxx.png"
/// （见设计文档 三、3.1）。实现方（WorkspaceContext）负责解析虚拟
/// 路径、做沙盒安全校验；本 trait 不关心具体怎么存、怎么校验。
pub trait MediaAssetSource {
    fn read_media(&self, virtual_path: &str) -> Result<Vec<u8>, String>;
}

thread_local! {
    static MEDIA_SOURCE: RefCell<Option<*const dyn MediaAssetSource>> = const { RefCell::new(None) };
}

/// 在 f 执行期间，把 source 绑定进 thread_local，供 impress_api.rs
/// 里的 with_media 取用。f 执行完毕后自动清空（无论 f 是否 panic
/// 都不清空——与本 crate 现有 with_presentation 的既有行为一致，
/// 该行为在这里同样不是本次改动引入的新风险）。
///
/// source 为 None 时（比如 FFI 边界调用、或 run_calc/run_writer
/// 这类根本不需要媒体能力的路径），不绑定任何东西，with_media
/// 会返回 None，picture 分支据此返回"当前会话未提供 workspace"
/// 类的错误，而不是 panic。
pub fn with_media_source<F: FnOnce()>(source: Option<&dyn MediaAssetSource>, f: F) {
    if let Some(s) = source {
        // Safety: 生命周期擦除。s 的真实生命周期至少覆盖 f() 的整个
        // 执行期间（source 参数本身就是调用方传进来、活过本函数调用
        // 的引用），本函数保证在 f() 返回后立刻清空 thread_local，
        // 不会有任何代码在 f() 之外经由 thread_local 访问这个指针。
        // 与本 crate 其它 thread_local 裸指针模式（PRESENTATION 等）
        // 同样的安全前提，只是 dyn Trait 引用需要显式 transmute 才能
        // 满足 'static 类型标注（具体类型的 *mut T 不需要这一步）。
        let erased: *const (dyn MediaAssetSource + 'static) =
            unsafe { std::mem::transmute::<*const dyn MediaAssetSource, *const (dyn MediaAssetSource + 'static)>(s as *const dyn MediaAssetSource) };
        MEDIA_SOURCE.with(|cell| {
            *cell.borrow_mut() = Some(erased);
        });
    }
    f();
    if source.is_some() {
        MEDIA_SOURCE.with(|cell| {
            *cell.borrow_mut() = None;
        });
    }
}

/// 供 impress_api.rs 内部调用：取出当前绑定的 MediaAssetSource 并
/// 执行 f。没有绑定（None）时整体返回 None。
pub fn with_media<R>(f: impl FnOnce(&dyn MediaAssetSource) -> R) -> Option<R> {
    MEDIA_SOURCE.with(|cell| {
        let ptr = (*cell.borrow())?;
        // Safety: 指针只在 with_media_source 包裹的 f() 执行期间有效，
        // 且 f() 是同步调用（thread_local，非跨线程持有），与本 crate
        // 其它 thread_local 裸指针模式（PRESENTATION/WORKBOOK 等）
        // 遵循同样的安全前提。
        Some(f(unsafe { &*ptr }))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeSource;
    impl MediaAssetSource for FakeSource {
        fn read_media(&self, virtual_path: &str) -> Result<Vec<u8>, String> {
            if virtual_path == "/media/ok.png" {
                Ok(b"fake bytes".to_vec())
            } else {
                Err(format!("not found: {virtual_path}"))
            }
        }
    }

    #[test]
    fn with_media_returns_none_when_no_source_bound() {
        let result = with_media(|s| s.read_media("/media/ok.png"));
        assert!(result.is_none());
    }

    #[test]
    fn with_media_source_binds_and_unbinds() {
        let source = FakeSource;
        let mut observed = None;
        with_media_source(Some(&source), || {
            observed = with_media(|s| s.read_media("/media/ok.png"));
        });
        assert_eq!(observed, Some(Ok(b"fake bytes".to_vec())));

        // 离开 with_media_source 之后应该已解绑。
        let after = with_media(|s| s.read_media("/media/ok.png"));
        assert!(after.is_none());
    }

    #[test]
    fn with_media_source_none_means_no_binding() {
        let mut observed_before: Option<Option<Result<Vec<u8>, String>>> = None;
        with_media_source(None, || {
            observed_before = Some(with_media(|s| s.read_media("/media/ok.png")));
        });
        assert_eq!(observed_before, Some(None));
    }
}
