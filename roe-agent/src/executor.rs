//! ScriptExecutor:Tool 与"持有文档的线程"之间的抽象。
//!
//! roe_api / Rhai 对文档的访问依赖 thread_local 的 HANDLES,只能在持有
//! handle 的线程上进行。Tool 运行在 tokio 线程,因此不直接碰 handle,
//! 而是持有 `Arc<dyn ScriptExecutor>`,由实现方决定在哪个线程执行:
//!
//! - LocalExecutor:在调用线程内联执行(CLI、单元测试)。
//! - GUI 侧(office-rs2):把请求投递到 UI 线程,经 oneshot 回传。
//! - 将来的文档工作线程(方案 B)与 MCP server 也实现同一个 trait。
//!
//! 不使用 shadow:execute_script_impl 失败时整体回滚,成功时 push
//! undo_stack,用户可 Ctrl+Z 撤销。

use std::sync::Arc;

use futures::future::BoxFuture;
use serde::Serialize;
use serde_json::Value as JsonValue;

use roe_api::script::{execute_script_impl, readonly_query_script_impl, DirtyRegion};

use crate::workspace::WorkspaceContext;

/// 写脚本的执行结果(纯数据,可跨线程)。
#[derive(Debug, Serialize)]
pub struct ExecuteOutcome {
    pub dirty_pages: Vec<usize>,
    pub dirty_regions: Vec<DirtyRegion>,
    pub script_result: JsonValue,
}

#[derive(Debug, thiserror::Error)]
pub enum ScriptExecError {
    #[error("no active document is currently open")]
    NoActiveDocument,
    #[error("invalid document handle: {0}")]
    InvalidHandle(u64),
    /// 脚本本身失败(语法错误、API 报错、只读环境调用了写函数等)。
    #[error("{0}")]
    Script(String),
}

pub trait ScriptExecutor: Send + Sync {
    /// 执行写脚本。成功即生效(已进撤销栈),失败则文档已回滚。
    fn execute(&self, script: String) -> BoxFuture<'static, Result<ExecuteOutcome, ScriptExecError>>;
    /// 执行只读查询脚本。
    fn query(&self, script: String) -> BoxFuture<'static, Result<JsonValue, ScriptExecError>>;
}

/// 在调用线程内联执行。调用方必须保证:当前线程就是持有 `handle` 的线程。
pub struct LocalExecutor {
    handle: u64,
    workspace: Arc<WorkspaceContext>,
}

impl LocalExecutor {
    pub fn new(handle: u64, workspace: Arc<WorkspaceContext>) -> Self {
        Self { handle, workspace }
    }
}

impl ScriptExecutor for LocalExecutor {
    fn execute(&self, script: String) -> BoxFuture<'static, Result<ExecuteOutcome, ScriptExecError>> {
        let r = if self.handle == 0 {
            Err(ScriptExecError::NoActiveDocument)
        } else {
            let media: &dyn roe_script::media_context::MediaAssetSource = &*self.workspace;
            match execute_script_impl(self.handle, &script, Some(media)) {
                None => Err(ScriptExecError::InvalidHandle(self.handle)),
                Some(Err(e)) => Err(ScriptExecError::Script(e)),
                Some(Ok((dirty_pages, dirty_regions, script_result))) => Ok(ExecuteOutcome {
                    dirty_pages,
                    dirty_regions,
                    script_result,
                }),
            }
        };
        Box::pin(std::future::ready(r))
    }

    fn query(&self, script: String) -> BoxFuture<'static, Result<JsonValue, ScriptExecError>> {
        let r = if self.handle == 0 {
            Err(ScriptExecError::NoActiveDocument)
        } else {
            match readonly_query_script_impl(self.handle, &script) {
                None => Err(ScriptExecError::InvalidHandle(self.handle)),
                Some(Err(e)) => Err(ScriptExecError::Script(e)),
                Some(Ok(v)) => Ok(v),
            }
        };
        match &r {
            Ok(v) => eprintln!("[ai] query {} -> {}", log_clip(&script, 200), log_clip(&v.to_string(), 300)),
            Err(e) => eprintln!("[ai] query {} -> 错误: {}", log_clip(&script, 200), log_clip(&format!("{e:?}"), 300)),
        }
        Box::pin(std::future::ready(r))
    }
}

/// 日志截断:按字符数而非字节,避免切在 UTF-8 中间;换行压成空格。
fn log_clip(s: &str, max: usize) -> String {
    let flat: String = s.chars().map(|c| if c == '\n' || c == '\r' { ' ' } else { c }).collect();
    if flat.chars().count() <= max {
        return flat;
    }
    let head: String = flat.chars().take(max).collect();
    format!("{head}…(共 {} 字符)", flat.chars().count())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_executor(handle: u64) -> LocalExecutor {
        let ws = WorkspaceContext::open_or_create(
            &tempfile::tempdir().expect("temp dir").keep(),
        )
        .expect("open workspace");
        LocalExecutor::new(handle, Arc::new(ws))
    }

    #[tokio::test]
    async fn zero_handle_is_no_active_document() {
        let ex = make_executor(0);
        assert!(matches!(
            ex.execute("1".into()).await,
            Err(ScriptExecError::NoActiveDocument)
        ));
        assert!(matches!(
            ex.query("1".into()).await,
            Err(ScriptExecError::NoActiveDocument)
        ));
    }

    #[tokio::test]
    async fn invalid_handle_is_reported() {
        let ex = make_executor(999_999);
        assert!(matches!(
            ex.execute("1".into()).await,
            Err(ScriptExecError::InvalidHandle(999_999))
        ));
    }

    #[tokio::test]
    async fn execute_then_query_roundtrip_on_calc() {
        let h = roe_api::document::new_document_impl(2); // 2 = Calc
        assert_ne!(h, 0);
        let ex = make_executor(h);
        ex.execute(r#"set_cell(0, 0, 0, "42")"#.into())
            .await
            .expect("execute should succeed");
        let v = ex
            .query("get_cell(0, 0, 0)".into())
            .await
            .expect("query should succeed");
        assert!(v.to_string().contains("42"), "got {v}");
    }

    #[tokio::test]
    async fn bad_script_returns_script_error() {
        let h = roe_api::document::new_document_impl(2);
        let ex = make_executor(h);
        assert!(matches!(
            ex.execute("this is not rhai (".into()).await,
            Err(ScriptExecError::Script(_))
        ));
    }
}
