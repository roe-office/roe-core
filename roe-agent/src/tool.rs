//! roe-agent 的 Tool:ExecuteScriptTool / QueryScriptTool 只是 LLM 与
//! `ScriptExecutor` 之间的薄适配层。
//!
//! Tool 运行在 tokio 线程,不直接碰文档 handle(roe_api 的 HANDLES 是
//! thread_local);脚本由 `ScriptExecutor` 的实现在持有文档的线程上执行
//! (见 executor.rs)。不再有 shadow/确认流程:execute_script_impl 失败时
//! 整体回滚,成功时进撤销栈,用户可 Ctrl+Z。

use std::sync::Arc;

use rig_agent::tool::{Tool, ToolContext};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::executor::{ExecuteOutcome, ScriptExecError, ScriptExecutor};

/// execute_script_preview 的参数。目标文档由 executor 决定,LLM 不填 handle。
#[derive(Debug, Deserialize)]
pub struct ExecuteScriptArgs {
    /// 要执行的 Rhai 脚本源码。
    pub script: String,
}

/// 返回给 LLM 的执行结果:脏区域信息 + 脚本返回值。
pub type ExecuteScriptOutput = ExecuteOutcome;

/// Tool 执行失败的原因(错误文本是特意写给模型看的,帮助它修正脚本)。
#[derive(Debug, thiserror::Error)]
pub enum ExecuteScriptError {
    #[error("invalid document handle: {0}")]
    InvalidHandle(u64),
    #[error("no active document is currently open")]
    NoActiveDocument,
    #[error("rhai script execution failed: {0}")]
    ScriptError(String),
}

impl From<ScriptExecError> for ExecuteScriptError {
    fn from(e: ScriptExecError) -> Self {
        match e {
            ScriptExecError::NoActiveDocument => Self::NoActiveDocument,
            ScriptExecError::InvalidHandle(h) => Self::InvalidHandle(h),
            ScriptExecError::Script(s) => Self::ScriptError(s),
        }
    }
}

/// 在当前文档上执行写脚本。
pub struct ExecuteScriptTool {
    pub executor: Arc<dyn ScriptExecutor>,
}

impl ExecuteScriptTool {
    pub fn new(executor: Arc<dyn ScriptExecutor>) -> Self {
        Self { executor }
    }
}

impl Tool for ExecuteScriptTool {
    const NAME: &'static str = "execute_script_preview";

    type Args = ExecuteScriptArgs;
    type Output = ExecuteScriptOutput;
    type Error = ExecuteScriptError;

    fn description(&self) -> String {
        "在当前打开的文档上执行一段 Rhai 脚本。执行成功后立即生效(用户可以\
         用撤销回退);脚本报错时文档会自动回滚、不产生任何修改。返回受影响\
         的区域和脚本的返回值,供你判断脚本是否达到了预期效果。"
            .to_string()
    }

    /// 把真实错误文本带给模型(rig 默认会把普通错误替换成固定文案)。
    fn map_error(&self, error: Self::Error) -> rig_core::tool::ToolExecutionError {
        let feedback = error.to_string();
        rig_core::tool::ToolExecutionError::from_error(error).with_model_feedback(feedback)
    }

    fn parameters(&self) -> JsonValue {
        serde_json::json!({
            "type": "object",
            "properties": {
                "script": {
                    "type": "string",
                    "description": "要执行的 Rhai 脚本源码"
                }
            },
            "required": ["script"]
        })
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        Ok(self.executor.execute(args.script).await?)
    }
}

/// 只读查询工具:不修改文档,不进撤销栈。
pub struct QueryScriptTool {
    pub executor: Arc<dyn ScriptExecutor>,
}

impl QueryScriptTool {
    pub fn new(executor: Arc<dyn ScriptExecutor>) -> Self {
        Self { executor }
    }
}

#[derive(Debug, Deserialize)]
pub struct QueryScriptArgs {
    /// 只读 Rhai 脚本;只能调用只读函数,调用写函数会报错。
    pub script: String,
}

#[derive(Debug, Serialize)]
pub struct QueryScriptOutput {
    pub script_result: JsonValue,
}

#[derive(Debug, thiserror::Error)]
pub enum QueryScriptError {
    #[error("invalid document handle: {0}")]
    InvalidHandle(u64),
    #[error("no active document is currently open")]
    NoActiveDocument,
    /// 语法错误、调用了只读白名单之外的函数、或文档类型暂不支持。
    #[error("readonly script execution failed: {0}")]
    ScriptError(String),
}

impl From<ScriptExecError> for QueryScriptError {
    fn from(e: ScriptExecError) -> Self {
        match e {
            ScriptExecError::NoActiveDocument => Self::NoActiveDocument,
            ScriptExecError::InvalidHandle(h) => Self::InvalidHandle(h),
            ScriptExecError::Script(s) => Self::ScriptError(s),
        }
    }
}

impl Tool for QueryScriptTool {
    const NAME: &'static str = "query_script_preview";

    type Args = QueryScriptArgs;
    type Output = QueryScriptOutput;
    type Error = QueryScriptError;

    fn description(&self) -> String {
        "在当前打开的文档上执行一段只读 Rhai 脚本,用于查询文档内容(例如读取\
         单元格的值、公式、格式等),不会修改文档,结果立即返回。支持表格(Calc)和文字文档(Writer),演示文稿暂不支持查询。脚本只能调用只读函数(get_cell、get_cell_formula、\
         get_sheet_count 等),调用写函数会报错——任何会修改文档的操作请用 \
         execute_script_preview。不确定有哪些函数时调用 list_functions(\"\") \
         查看清单,调用 inspect_workbook() 了解表格结构。"
            .to_string()
    }

    fn map_error(&self, error: Self::Error) -> rig_core::tool::ToolExecutionError {
        let feedback = error.to_string();
        rig_core::tool::ToolExecutionError::from_error(error).with_model_feedback(feedback)
    }

    fn parameters(&self) -> JsonValue {
        serde_json::json!({
            "type": "object",
            "properties": {
                "script": {
                    "type": "string",
                    "description": "要执行的只读 Rhai 查询脚本源码"
                }
            },
            "required": ["script"]
        })
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        let script_result = self.executor.query(args.script).await?;
        Ok(QueryScriptOutput { script_result })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::LocalExecutor;
    use roe_api::handle::{self, DocumentEntry};
    use roe_parser::RoeDocument;
    use std::collections::VecDeque;

    fn temp_workspace() -> Arc<WorkspaceContext> {
        Arc::new(
            WorkspaceContext::open_or_create(
                &tempfile::tempdir().expect("create temp dir for test workspace").keep(),
            )
            .expect("open_or_create should succeed in test"),
        )
    }

    fn local(handle: u64, ws: Arc<WorkspaceContext>) -> Arc<dyn ScriptExecutor> {
        Arc::new(LocalExecutor::new(handle, ws))
    }

    /// 打开一个最简空白 Impress 文档（一张空 slide，无 shape），
    /// 供端到端测试使用。
    fn open_test_impress_doc() -> u64 {
        use roe_model::impress::{AnimationNode, Presentation, Slide};
        let pres = Presentation {
            slide_size: (960.0, 540.0),
            slides: vec![Slide { layout_idx: None,
                shapes: vec![],
                master_idx: None,
                animation_tree: AnimationNode::TimingRoot { children: vec![] },
                transition: None,
                notes: None,
                background: None,
                layout_shapes: vec![],
            }],
            master_slides: vec![],
            styles: Default::default(),
            theme_colors: Default::default(),
            default_text_style: Default::default(),
        };
        handle::insert(DocumentEntry {
            doc: RoeDocument::Impress(pres),
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

    #[tokio::test]
    async fn execute_tool_no_active_document() {
        let tool = ExecuteScriptTool::new(local(0, temp_workspace()));
        let r = tool
            .call(&mut ToolContext::new(), ExecuteScriptArgs { script: "()".into() })
            .await;
        assert!(matches!(r, Err(ExecuteScriptError::NoActiveDocument)));
    }

    #[tokio::test]
    async fn execute_tool_invalid_handle() {
        let tool = ExecuteScriptTool::new(local(999_999, temp_workspace()));
        let r = tool
            .call(&mut ToolContext::new(), ExecuteScriptArgs { script: "()".into() })
            .await;
        assert!(matches!(r, Err(ExecuteScriptError::InvalidHandle(999_999))));
    }

    #[tokio::test]
    async fn execute_then_query_on_calc() {
        let h = roe_api::document::new_document_impl(2);
        let ws = temp_workspace();
        let exec_tool = ExecuteScriptTool::new(local(h, ws.clone()));
        let query_tool = QueryScriptTool::new(local(h, ws));

        exec_tool
            .call(
                &mut ToolContext::new(),
                ExecuteScriptArgs { script: r#"set_cell(0, 0, 0, "42")"#.into() },
            )
            .await
            .expect("execute should succeed");

        let out = query_tool
            .call(
                &mut ToolContext::new(),
                QueryScriptArgs { script: "get_cell(0, 0, 0)".into() },
            )
            .await
            .expect("query should succeed");
        assert!(out.script_result.to_string().contains("42"), "got {}", out.script_result);
    }

    #[tokio::test]
    async fn execute_tool_bad_script_maps_to_script_error() {
        let h = roe_api::document::new_document_impl(2);
        let tool = ExecuteScriptTool::new(local(h, temp_workspace()));
        let r = tool
            .call(
                &mut ToolContext::new(),
                ExecuteScriptArgs { script: "this is not rhai (".into() },
            )
            .await;
        assert!(matches!(r, Err(ExecuteScriptError::ScriptError(_))));
    }

    #[tokio::test]
    async fn query_tool_rejects_write_function() {
        let h = roe_api::document::new_document_impl(2);
        let tool = QueryScriptTool::new(local(h, temp_workspace()));
        let r = tool
            .call(
                &mut ToolContext::new(),
                QueryScriptArgs { script: r#"set_cell(0, 0, 0, "1")"#.into() },
            )
            .await;
        assert!(matches!(r, Err(QueryScriptError::ScriptError(_))), "got {r:?}");
    }

    #[tokio::test]
    async fn query_tool_no_active_document() {
        let tool = QueryScriptTool::new(local(0, temp_workspace()));
        let r = tool
            .call(&mut ToolContext::new(), QueryScriptArgs { script: "1".into() })
            .await;
        assert!(matches!(r, Err(QueryScriptError::NoActiveDocument)));
    }

    /// 端到端:真实文件 → ingest_media_asset → 虚拟路径 → set_slide_background
    /// (picture 分支经 executor 的 workspace 读取)→ 原文档 background 变为 Picture。
    #[tokio::test]
    async fn end_to_end_ingest_then_set_picture_background() {
        let ws = temp_workspace();

        let src_dir = tempfile::tempdir().expect("create source temp dir");
        let src_path = src_dir.path().join("e2e_test.png");
        let fake_png_bytes = b"fake png bytes for end-to-end test";
        std::fs::write(&src_path, fake_png_bytes).expect("write source file");

        let ingest_tool = IngestMediaAssetTool::new(ws.clone());
        let ingest_output = ingest_tool
            .call(
                &mut ToolContext::new(),
                IngestMediaAssetArgs {
                    source_type: "local_path".to_string(),
                    source: src_path.to_string_lossy().to_string(),
                },
            )
            .await
            .expect("ingest should succeed");
        assert!(ingest_output.virtual_path.starts_with("/media/"));
        assert_eq!(ingest_output.mime_type, "image/png");

        let h = open_test_impress_doc();
        let script_tool = ExecuteScriptTool::new(local(h, ws));
        let bg_json = format!(
            r#"{{"type":"picture","path":"{}","mime":"{}"}}"#,
            ingest_output.virtual_path, ingest_output.mime_type
        );
        let script = format!(r#"set_slide_background(0, `{bg_json}`)"#);
        let out = script_tool
            .call(&mut ToolContext::new(), ExecuteScriptArgs { script })
            .await
            .expect("set_slide_background script should run");
        assert_eq!(
            out.script_result,
            serde_json::Value::String(String::new()),
            "got: {:?}",
            out.script_result
        );

        use roe_model::common::FillStyle;
        let verified = handle::with_entry(h, |entry| match &entry.doc {
            RoeDocument::Impress(pres) => match &pres.slides[0].background {
                Some(FillStyle::Picture(image)) => {
                    assert_eq!(image.data, fake_png_bytes);
                    assert_eq!(image.mime, "image/png");
                    true
                }
                other => panic!("期望 Some(FillStyle::Picture(..)),实际: {other:?}"),
            },
            other => panic!("期望 RoeDocument::Impress,实际: {other:?}"),
        });
        assert_eq!(verified, Some(true));

        handle::remove(h);
    }
}

// =============================================================================
// IngestMediaAssetTool —— 第三个 Agent Tool，把媒体文件落地进 workspace。
//
// 设计依据：ROE_Agent_Workspace_设计文档_v1.md，四、新增 Tool：ingest_media_asset。
//
// 与 ExecuteScriptTool/QueryScriptTool 的关键区别：
// - 操作对象是磁盘上的文件（workspace），不是内存中的文档 handle。
// - 不经过 shadow/confirm 机制：写入 workspace 是"准备素材"这个动作
//   本身，不是"修改文档"，不应该被 Confirm 模式的 y/n 确认流程卡住
//   （设计文档 四、4.5）。真正需要用户确认的是后续"把这张图设为
//   背景"这个动作，那一步走 execute_script_preview，天然会经过既
//   有的 Confirm 流程。
// =============================================================================

use sha2::{Digest, Sha256};

use crate::workspace::WorkspaceContext;

/// ingest_media_asset 的参数。
///
/// MVP 阶段只实现 source_type = "local_path" 一个分支（钉死方案，
/// 见设计文档四、4.4 及本次会话讨论）。source_type = "url" 分支
/// 走占位错误，见 IngestMediaAssetTool::call 内部说明。
#[derive(Debug, Deserialize)]
pub struct IngestMediaAssetArgs {
    /// 来源类型。当前只支持 "local_path"；"url" 已保留字段位置，
    /// 但本次返回占位错误（未实现）。
    pub source_type: String,
    /// source_type = "local_path" 时：宿主机上的本地文件路径。
    /// 这个路径本身不受 workspace 沙盒限制（它描述的是 workspace
    /// **之外**的文件，是外部输入源，不是 workspace 内部路径）——
    /// 但读取时用标准 std::fs，因为这不是 cap-std Dir 的管辖范围。
    pub source: String,
}

/// 返回给 LLM 的结果。
#[derive(Debug, Serialize)]
pub struct IngestMediaAssetOutput {
    /// 虚拟路径，形如 "/media/xxx.png"，可直接作为
    /// set_slide_background 的 path 字段传入（该字段的图片分支本次
    /// 仍是占位实现，见 v198 交接文档与设计文档五、3）。
    pub virtual_path: String,
    /// MIME 类型，与 roe_model::common::ImageData.mime 字段同类型
    /// （纯字符串），供 AI 判断格式是否符合预期。
    ///
    /// 本次 MVP 阶段判断方式：仅看文件扩展名，非真正的魔术字节
    /// 探测（设计文档三、3.6 提到的 infer/infer2 校验本次未引入，
    /// 见 Cargo.toml 注释）。这是已知的粗糙之处：伪造扩展名的文件
    /// 会被误判，等将来引入 infer2 后应该替换成真正的内容探测。
    pub mime_type: String,
    /// 文件字节数。
    pub size_bytes: u64,
    /// 本次写入是否命中了 CAS 去重（内容已存在，未重复写盘）。
    pub deduplicated: bool,
}

/// IngestMediaAssetTool 执行失败的原因。
#[derive(Debug, thiserror::Error)]
pub enum IngestMediaAssetError {
    /// source_type 不是已知值，或是已知但本次未实现（如 "url"）。
    #[error("unsupported source_type {0:?}: only \"local_path\" is implemented in this version (\"url\" is reserved for future reqwest-based download support)")]
    UnsupportedSourceType(String),
    #[error("local path {0:?} does not exist or is not a regular file")]
    LocalPathNotFound(String),
    #[error("failed to read local file {path}: {source}")]
    LocalReadFailed {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("workspace error: {0}")]
    Workspace(#[from] crate::workspace::WorkspaceError),
}

/// 包装 WorkspaceContext 的 Tool：把外部文件（本次仅本地路径）落地
/// 进 workspace 的 CAS 存储，返回虚拟路径。
pub struct IngestMediaAssetTool {
    pub workspace: Arc<WorkspaceContext>,
}

impl IngestMediaAssetTool {
    pub fn new(workspace: Arc<WorkspaceContext>) -> Self {
        Self { workspace }
    }

    /// 从文件扩展名粗略猜测 MIME 类型和文件名后缀。
    ///
    /// 临时方案（见 IngestMediaAssetOutput.mime_type 文档），只覆盖
    /// 幻灯片背景场景最可能遇到的几种图片格式。未知扩展名时返回
    /// "application/octet-stream" 兜底，不 panic、不拒绝。
    fn guess_mime_and_ext(path: &str) -> (&'static str, &'static str) {
        let ext = std::path::Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        match ext.as_str() {
            "png" => ("image/png", "png"),
            "jpg" | "jpeg" => ("image/jpeg", "jpg"),
            "gif" => ("image/gif", "gif"),
            "webp" => ("image/webp", "webp"),
            "bmp" => ("image/bmp", "bmp"),
            _ => ("application/octet-stream", "bin"),
        }
    }
}

impl Tool for IngestMediaAssetTool {
    const NAME: &'static str = "ingest_media_asset";

    type Args = IngestMediaAssetArgs;
    type Output = IngestMediaAssetOutput;
    type Error = IngestMediaAssetError;

    fn description(&self) -> String {
        "把一份媒体文件（目前仅支持本地文件路径）的内容落地进 Agent 的 \
         专属工作区（workspace），返回一个虚拟路径字符串（形如 \
         \"/media/xxx.png\"）。这个虚拟路径可以在后续需要引用图片的 \
         脚本操作中使用（例如设置幻灯片背景图片）。source_type 目前只 \
         支持 \"local_path\"；\"url\"（从网络地址下载）尚未实现。"
            .to_string()
    }

    fn map_error(&self, error: Self::Error) -> rig_core::tool::ToolExecutionError {
        let feedback = error.to_string();
        rig_core::tool::ToolExecutionError::from_error(error).with_model_feedback(feedback)
    }

    fn parameters(&self) -> JsonValue {
        serde_json::json!({
            "type": "object",
            "properties": {
                "source_type": {
                    "type": "string",
                    "enum": ["local_path"],
                    "description": "来源类型，目前只支持 \"local_path\""
                },
                "source": {
                    "type": "string",
                    "description": "source_type=\"local_path\" 时：宿主机本地文件的绝对或相对路径"
                }
            },
            "required": ["source_type", "source"]
        })
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        if args.source_type != "local_path" {
            return Err(IngestMediaAssetError::UnsupportedSourceType(
                args.source_type,
            ));
        }

        let source_path = std::path::Path::new(&args.source);
        if !source_path.is_file() {
            return Err(IngestMediaAssetError::LocalPathNotFound(
                args.source.clone(),
            ));
        }

        let bytes = std::fs::read(source_path).map_err(|source| {
            IngestMediaAssetError::LocalReadFailed {
                path: args.source.clone(),
                source,
            }
        })?;

        let (mime_type, ext) = Self::guess_mime_and_ext(&args.source);

        let hash = Sha256::digest(&bytes);
        let file_name = format!("{hash:x}.{ext}");

        // write_media 内部已经做了 CAS 去重判断；这里为了准确回填
        // deduplicated 字段，在写入前后各查一次是否存在（简单起见，
        // 不改 WorkspaceContext 的接口去返回这个信息——那是内部实现
        // 细节，本次先用最直接的方式在 Tool 层判断）。
        let already_existed = self.workspace.read(&format!("/media/{file_name}")).is_ok();

        let virtual_path = self.workspace.write_media(&file_name, &bytes)?;

        Ok(IngestMediaAssetOutput {
            virtual_path,
            mime_type: mime_type.to_string(),
            size_bytes: bytes.len() as u64,
            deduplicated: already_existed,
        })
    }
}

#[cfg(test)]
mod ingest_media_asset_tests {
    use super::*;

    fn make_ingest_tool() -> (tempfile::TempDir, IngestMediaAssetTool) {
        let ws_dir = tempfile::tempdir().expect("create workspace temp dir");
        let ctx = WorkspaceContext::open_or_create(ws_dir.path())
            .expect("workspace open_or_create should succeed");
        (ws_dir, IngestMediaAssetTool::new(Arc::new(ctx)))
    }

    #[tokio::test]
    async fn rejects_url_source_type_as_not_yet_implemented() {
        let (_ws_dir, tool) = make_ingest_tool();
        let mut ctx = ToolContext::new();
        let args = IngestMediaAssetArgs {
            source_type: "url".to_string(),
            source: "https://example.com/pic.png".to_string(),
        };
        let result = tool.call(&mut ctx, args).await;
        assert!(matches!(
            result,
            Err(IngestMediaAssetError::UnsupportedSourceType(_))
        ));
    }

    #[tokio::test]
    async fn rejects_nonexistent_local_path() {
        let (_ws_dir, tool) = make_ingest_tool();
        let mut ctx = ToolContext::new();
        let args = IngestMediaAssetArgs {
            source_type: "local_path".to_string(),
            source: "/this/path/definitely/does/not/exist.png".to_string(),
        };
        let result = tool.call(&mut ctx, args).await;
        assert!(matches!(
            result,
            Err(IngestMediaAssetError::LocalPathNotFound(_))
        ));
    }

    #[tokio::test]
    async fn ingests_local_file_and_returns_virtual_path() {
        let (_ws_dir, tool) = make_ingest_tool();
        let mut ctx = ToolContext::new();

        // 造一个假的本地"图片"文件（内容不重要，MVP 阶段的 MIME
        // 判断只看扩展名，不校验真实内容，见 guess_mime_and_ext 文档）。
        let src_dir = tempfile::tempdir().expect("create source temp dir");
        let src_path = src_dir.path().join("test_pic.png");
        std::fs::write(&src_path, b"fake png bytes for testing").expect("write source file");

        let args = IngestMediaAssetArgs {
            source_type: "local_path".to_string(),
            source: src_path.to_string_lossy().to_string(),
        };
        let output = tool.call(&mut ctx, args).await.expect("ingest should succeed");

        assert!(output.virtual_path.starts_with("/media/"));
        assert!(output.virtual_path.ends_with(".png"));
        assert_eq!(output.mime_type, "image/png");
        assert_eq!(output.size_bytes, 26); // "fake png bytes for testing".len()
        assert!(!output.deduplicated, "first ingest should not be a dedup hit");
    }

    #[tokio::test]
    async fn ingesting_same_content_twice_deduplicates() {
        let (_ws_dir, tool) = make_ingest_tool();
        let mut ctx = ToolContext::new();

        let src_dir = tempfile::tempdir().expect("create source temp dir");
        let src_path = src_dir.path().join("dup.png");
        std::fs::write(&src_path, b"identical content").expect("write source file");

        let args = || IngestMediaAssetArgs {
            source_type: "local_path".to_string(),
            source: src_path.to_string_lossy().to_string(),
        };

        let first = tool.call(&mut ctx, args()).await.expect("first ingest should succeed");
        assert!(!first.deduplicated);

        let second = tool.call(&mut ctx, args()).await.expect("second ingest should succeed");
        assert!(second.deduplicated, "second ingest of identical content should be a dedup hit");
        assert_eq!(first.virtual_path, second.virtual_path);
    }
}
