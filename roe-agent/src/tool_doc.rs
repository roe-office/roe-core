//! 无头模式的文档工具:new / open / save / close / list_workspace。
//!
//! 工具只面向模型,不碰文档句柄:文档操作经 DocWorker(见 worker.rs),
//! 文件落盘经 WorkspaceContext。模型只看到虚拟路径(如 /exports/a.xlsx),
//! 物理路径由前端按需用 WorkspaceContext::physical_path 换算。
//! 逻辑写在各工具的 run() 里,call() 只做转发,便于测试。

use std::collections::BTreeMap;
use std::sync::Arc;

use rig_agent::tool::{Tool, ToolContext};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::worker::{DocKind, DocSummary, DocWorker};
use crate::workspace::{WorkspaceContext, WorkspaceEntry, EXPORTS_DIR, INPUTS_DIR, MEDIA_DIR};

/// 文档工具的统一错误:文本是写给模型看的。
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct DocToolError(pub String);

fn m<E: ToString>(e: E) -> DocToolError {
    DocToolError(e.to_string())
}

/// 与其他 Tool 相同:把真实错误文本带给模型。
macro_rules! feedback_map_error {
    () => {
        fn map_error(&self, error: Self::Error) -> rig_core::tool::ToolExecutionError {
            let feedback = error.to_string();
            rig_core::tool::ToolExecutionError::from_error(error).with_model_feedback(feedback)
        }
    };
}

/// 已知文档扩展名(不分大小写),用于从文件名主干里去掉。
const KNOWN_EXTS: [&str; 4] = ["docx", "xlsx", "pptx", "csv"];

/// 把模型给的名字变成安全的文件名主干:去掉已知扩展名,替换路径分隔符与控制字符,限长,空则 "document"。
fn file_stem(name: &str) -> String {
    let name = name.trim();
    let base = match name.rfind('.') {
        Some(i) if i > 0 && KNOWN_EXTS.contains(&name[i + 1..].to_ascii_lowercase().as_str()) => &name[..i],
        _ => name,
    };
    let cleaned: String = base
        .chars()
        .map(|c| if c == '/' || c == '\\' || c.is_control() { '_' } else { c })
        .take(100)
        .collect();
    let cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        "document".to_string()
    } else {
        cleaned
    }
}

// ───────────────────────── new_document ─────────────────────────

#[derive(Debug, Deserialize)]
pub struct NewDocumentArgs {
    /// "writer" / "calc" / "impress"
    pub kind: String,
    /// 文档显示名,同时是保存时默认的文件名主干。可省略。
    pub name: Option<String>,
}

pub struct NewDocumentTool {
    pub worker: Arc<DocWorker>,
}

impl NewDocumentTool {
    pub fn new(worker: Arc<DocWorker>) -> Self {
        Self { worker }
    }

    pub async fn run(&self, args: NewDocumentArgs) -> Result<DocSummary, DocToolError> {
        let kind = match args.kind.to_ascii_lowercase().as_str() {
            "writer" => DocKind::Writer,
            "calc" => DocKind::Calc,
            "impress" => DocKind::Impress,
            other => {
                return Err(DocToolError(format!(
                    "未知文档类型 {other:?}:只能是 writer(文字文档)、calc(表格)、impress(演示文稿)"
                )))
            }
        };
        let name = args.name.as_deref().map(file_stem).unwrap_or_else(|| "未命名".to_string());
        self.worker.new_document(kind, name).await.map_err(m)
    }
}

impl Tool for NewDocumentTool {
    const NAME: &'static str = "new_document";
    type Args = NewDocumentArgs;
    type Output = DocSummary;
    type Error = DocToolError;

    fn description(&self) -> String {
        "新建一个空白文档,新文档成为当前文档,之后的 execute_script_preview / \
         query_script_preview 都作用在它上面。kind 可选 writer(文字文档)、calc(表格)、\
         impress(演示文稿)。新建后用 list_functions(\"\") 的第一行确认文档类型和可用函数。"
            .to_string()
    }

    feedback_map_error!();

    fn parameters(&self) -> JsonValue {
        serde_json::json!({
            "type": "object",
            "properties": {
                "kind": { "type": "string", "enum": ["writer", "calc", "impress"], "description": "文档类型" },
                "name": { "type": "string", "description": "文档名(可省略),保存时默认用它做文件名" }
            },
            "required": ["kind"]
        })
    }

    async fn call(&self, _context: &mut ToolContext, args: Self::Args) -> Result<Self::Output, Self::Error> {
        self.run(args).await
    }
}

// ───────────────────────── open_document ─────────────────────────

#[derive(Debug, Deserialize)]
pub struct OpenDocumentArgs {
    /// 工作区虚拟路径,形如 /inputs/data.xlsx 或 /exports/report.docx。
    pub path: String,
}

pub struct OpenDocumentTool {
    pub worker: Arc<DocWorker>,
    pub workspace: Arc<WorkspaceContext>,
}

impl OpenDocumentTool {
    pub fn new(worker: Arc<DocWorker>, workspace: Arc<WorkspaceContext>) -> Self {
        Self { worker, workspace }
    }

    pub async fn run(&self, args: OpenDocumentArgs) -> Result<DocSummary, DocToolError> {
        let path = args.path.trim();
        let allowed = [format!("/{INPUTS_DIR}/"), format!("/{EXPORTS_DIR}/")];
        if !allowed.iter().any(|p| path.starts_with(p.as_str())) {
            return Err(DocToolError(format!(
                "路径 {path:?} 不可打开:只能打开 /inputs/ 或 /exports/ 下的文件(先用 list_workspace 查看)"
            )));
        }
        let file_name = path.rsplit('/').next().unwrap_or("").to_string();
        let ext = file_name.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
        let (kind, hint) = DocKind::from_extension(ext).ok_or_else(|| {
            DocToolError(format!("不支持的文件类型 {file_name:?}:目前只能打开 docx、xlsx、csv、pptx"))
        })?;
        let bytes = self.workspace.read(path).map_err(m)?;
        self.worker.open_document(bytes, kind, hint, file_name).await.map_err(m)
    }
}

impl Tool for OpenDocumentTool {
    const NAME: &'static str = "open_document";
    type Args = OpenDocumentArgs;
    type Output = DocSummary;
    type Error = DocToolError;

    fn description(&self) -> String {
        "打开工作区里的一份文档(docx / xlsx / csv / pptx),打开后成为当前文档。\
         path 是工作区虚拟路径,只能在 /inputs/(用户上传)或 /exports/(之前的产物)下,\
         不确定有哪些文件时先调用 list_workspace。csv 会作为表格打开,保存时得到 xlsx。"
            .to_string()
    }

    feedback_map_error!();

    fn parameters(&self) -> JsonValue {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "工作区虚拟路径,如 /inputs/data.xlsx" }
            },
            "required": ["path"]
        })
    }

    async fn call(&self, _context: &mut ToolContext, args: Self::Args) -> Result<Self::Output, Self::Error> {
        self.run(args).await
    }
}

// ───────────────────────── save_document ─────────────────────────

#[derive(Debug, Deserialize)]
pub struct SaveDocumentArgs {
    /// 文件名(可省略,默认用文档名)。扩展名由文档类型决定。
    pub name: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SaveDocumentOutput {
    /// 产物的虚拟路径,如 /exports/报表.xlsx。重名时自动加序号,不会覆盖旧文件。
    pub virtual_path: String,
    pub kind: DocKind,
    pub size_bytes: u64,
}

pub struct SaveDocumentTool {
    pub worker: Arc<DocWorker>,
    pub workspace: Arc<WorkspaceContext>,
}

impl SaveDocumentTool {
    pub fn new(worker: Arc<DocWorker>, workspace: Arc<WorkspaceContext>) -> Self {
        Self { worker, workspace }
    }

    pub async fn run(&self, args: SaveDocumentArgs) -> Result<SaveDocumentOutput, DocToolError> {
        let saved = self.worker.save_document().await.map_err(m)?;
        let stem = file_stem(args.name.as_deref().unwrap_or(&saved.name));
        let file_name = format!("{stem}.{}", saved.kind.ext());
        let virtual_path = self.workspace.write_file(EXPORTS_DIR, &file_name, &saved.bytes).map_err(m)?;
        Ok(SaveDocumentOutput { virtual_path, kind: saved.kind, size_bytes: saved.bytes.len() as u64 })
    }
}

impl Tool for SaveDocumentTool {
    const NAME: &'static str = "save_document";
    type Args = SaveDocumentArgs;
    type Output = SaveDocumentOutput;
    type Error = DocToolError;

    fn description(&self) -> String {
        "把当前文档保存到工作区的 /exports/,返回产物的虚拟路径。扩展名按文档类型自动确定\
         (表格 xlsx、文字文档 docx、演示文稿 pptx),name 里写的扩展名会被忽略。\
         重名自动加序号,不会覆盖已有文件。任务完成后必须保存,并把返回的路径告诉用户。"
            .to_string()
    }

    feedback_map_error!();

    fn parameters(&self) -> JsonValue {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "文件名(可省略),不必带扩展名" }
            }
        })
    }

    async fn call(&self, _context: &mut ToolContext, args: Self::Args) -> Result<Self::Output, Self::Error> {
        self.run(args).await
    }
}

// ───────────────────────── close_document ─────────────────────────

#[derive(Debug, Deserialize)]
pub struct CloseDocumentArgs {}

pub struct CloseDocumentTool {
    pub worker: Arc<DocWorker>,
}

impl CloseDocumentTool {
    pub fn new(worker: Arc<DocWorker>) -> Self {
        Self { worker }
    }

    pub async fn run(&self, _args: CloseDocumentArgs) -> Result<DocSummary, DocToolError> {
        self.worker.close_current().await.map_err(m)
    }
}

impl Tool for CloseDocumentTool {
    const NAME: &'static str = "close_document";
    type Args = CloseDocumentArgs;
    type Output = DocSummary;
    type Error = DocToolError;

    fn description(&self) -> String {
        "关闭当前文档(未保存的修改会丢失,所以先 save_document),并回到上一个打开的文档。\
         批量处理多份文档时,每份处理完并保存后都要关闭。"
            .to_string()
    }

    feedback_map_error!();

    fn parameters(&self) -> JsonValue {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    async fn call(&self, _context: &mut ToolContext, args: Self::Args) -> Result<Self::Output, Self::Error> {
        self.run(args).await
    }
}

// ───────────────────────── list_workspace ─────────────────────────

#[derive(Debug, Deserialize)]
pub struct ListWorkspaceArgs {
    /// "inputs" / "exports" / "media";省略则三个都列。
    pub dir: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ListWorkspaceOutput {
    pub files: BTreeMap<String, Vec<WorkspaceEntry>>,
    /// 当前打开的文档(最后一个是当前文档)。
    pub open_documents: Vec<DocSummary>,
}

pub struct ListWorkspaceTool {
    pub worker: Arc<DocWorker>,
    pub workspace: Arc<WorkspaceContext>,
}

impl ListWorkspaceTool {
    pub fn new(worker: Arc<DocWorker>, workspace: Arc<WorkspaceContext>) -> Self {
        Self { worker, workspace }
    }

    pub async fn run(&self, args: ListWorkspaceArgs) -> Result<ListWorkspaceOutput, DocToolError> {
        let dirs: Vec<&str> = match args.dir.as_deref() {
            None => vec![INPUTS_DIR, EXPORTS_DIR, MEDIA_DIR],
            Some(d) => vec![d],
        };
        let mut files = BTreeMap::new();
        for d in dirs {
            files.insert(d.to_string(), self.workspace.list(d).map_err(m)?);
        }
        let open_documents = self.worker.list_documents().await.map_err(m)?;
        Ok(ListWorkspaceOutput { files, open_documents })
    }
}

impl Tool for ListWorkspaceTool {
    const NAME: &'static str = "list_workspace";
    type Args = ListWorkspaceArgs;
    type Output = ListWorkspaceOutput;
    type Error = DocToolError;

    fn description(&self) -> String {
        "列出工作区里的文件(inputs=用户上传、exports=已保存的产物、media=图片素材),\
         以及当前打开的文档。返回的是虚拟路径,可直接传给 open_document。"
            .to_string()
    }

    feedback_map_error!();

    fn parameters(&self) -> JsonValue {
        serde_json::json!({
            "type": "object",
            "properties": {
                "dir": { "type": "string", "enum": ["inputs", "exports", "media"], "description": "只列某个目录(可省略)" }
            }
        })
    }

    async fn call(&self, _context: &mut ToolContext, args: Self::Args) -> Result<Self::Output, Self::Error> {
        self.run(args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::ScriptExecutor;

    fn setup() -> (Arc<DocWorker>, Arc<WorkspaceContext>) {
        let ws = Arc::new(
            WorkspaceContext::open_or_create(&tempfile::tempdir().expect("temp dir").keep()).expect("ws"),
        );
        let worker = Arc::new(DocWorker::spawn(ws.clone()).expect("spawn"));
        (worker, ws)
    }

    #[test]
    fn file_stem_cases() {
        assert_eq!(file_stem("报表.xlsx"), "报表");
        assert_eq!(file_stem("Data.CSV"), "Data");
        assert_eq!(file_stem("a.b"), "a.b"); // 未知扩展名保留
        assert_eq!(file_stem("a/b\\c"), "a_b_c");
        assert_eq!(file_stem(""), "document");
        assert_eq!(file_stem(".."), "document");
        assert_eq!(file_stem(".xlsx"), ".xlsx"); // 点在开头不是扩展名
        assert_eq!(file_stem(&"长".repeat(300)).chars().count(), 100);
    }

    #[tokio::test]
    async fn new_rejects_unknown_kind() {
        let (w, _ws) = setup();
        let t = NewDocumentTool::new(w);
        let e = t.run(NewDocumentArgs { kind: "excel".into(), name: None }).await.unwrap_err();
        assert!(e.to_string().contains("writer"), "{e}");
    }

    /// 新建表格 -> 写数据 -> 保存 -> 关闭 -> 从 exports 重新打开 -> 数据还在。
    #[tokio::test]
    async fn new_edit_save_close_open_roundtrip() {
        let (w, ws) = setup();
        let new_t = NewDocumentTool::new(w.clone());
        let save_t = SaveDocumentTool::new(w.clone(), ws.clone());
        let close_t = CloseDocumentTool::new(w.clone());
        let open_t = OpenDocumentTool::new(w.clone(), ws.clone());

        let s = new_t.run(NewDocumentArgs { kind: "CALC".into(), name: Some("销售.xlsx".into()) }).await.unwrap();
        assert_eq!(s.name, "销售");
        w.execute(r#"set_cell(0, 0, 0, "42")"#.into()).await.unwrap();

        let out = save_t.run(SaveDocumentArgs { name: None }).await.unwrap();
        assert_eq!(out.virtual_path, "/exports/销售.xlsx");
        assert_eq!(out.kind, DocKind::Calc);
        assert!(out.size_bytes > 100);
        // 再存一次:不覆盖,加序号
        let out2 = save_t.run(SaveDocumentArgs { name: Some("销售".into()) }).await.unwrap();
        assert_eq!(out2.virtual_path, "/exports/销售 (1).xlsx");
        // 模型给了错的扩展名:被替换
        let out3 = save_t.run(SaveDocumentArgs { name: Some("x.docx".into()) }).await.unwrap();
        assert_eq!(out3.virtual_path, "/exports/x.xlsx");

        close_t.run(CloseDocumentArgs {}).await.unwrap();
        assert!(w.list_documents().await.unwrap().is_empty());

        let back = open_t.run(OpenDocumentArgs { path: out.virtual_path.clone() }).await.unwrap();
        assert_eq!(back.kind, DocKind::Calc);
        assert!(w.query("get_cell(0, 0, 0)".into()).await.unwrap().to_string().contains("42"));
    }

    #[tokio::test]
    async fn open_rejects_bad_paths_and_types() {
        let (w, ws) = setup();
        let t = OpenDocumentTool::new(w, ws.clone());
        for p in ["/media/a.xlsx", "/etc/passwd", "inputs/a.xlsx", "/inputs", "/exports/../media/a.xlsx"] {
            let e = t.run(OpenDocumentArgs { path: p.into() }).await;
            assert!(e.is_err(), "{p}");
        }
        ws.write_file("inputs", "a.odt", b"x").unwrap();
        let e = t.run(OpenDocumentArgs { path: "/inputs/a.odt".into() }).await.unwrap_err();
        assert!(e.to_string().contains("不支持"), "{e}");
        let e = t.run(OpenDocumentArgs { path: "/inputs/none.xlsx".into() }).await.unwrap_err();
        assert!(!e.to_string().is_empty());
        ws.write_file("inputs", "bad.xlsx", b"not a zip").unwrap();
        assert!(t.run(OpenDocumentArgs { path: "/inputs/bad.xlsx".into() }).await.is_err());
    }

    #[tokio::test]
    async fn open_csv_as_calc_and_save_as_xlsx() {
        let (w, ws) = setup();
        ws.write_file("inputs", "d.csv", "a,b\n1,2\n".as_bytes()).unwrap();
        let open_t = OpenDocumentTool::new(w.clone(), ws.clone());
        let save_t = SaveDocumentTool::new(w.clone(), ws.clone());
        let s = open_t.run(OpenDocumentArgs { path: "/inputs/d.csv".into() }).await.unwrap();
        assert_eq!(s.kind, DocKind::Calc);
        let out = save_t.run(SaveDocumentArgs { name: None }).await.unwrap();
        assert_eq!(out.virtual_path, "/exports/d.xlsx");
    }

    #[tokio::test]
    async fn save_and_close_without_document_error() {
        let (w, ws) = setup();
        assert!(SaveDocumentTool::new(w.clone(), ws).run(SaveDocumentArgs { name: None }).await.is_err());
        assert!(CloseDocumentTool::new(w).run(CloseDocumentArgs {}).await.is_err());
    }

    #[tokio::test]
    async fn list_workspace_shows_files_and_open_documents() {
        let (w, ws) = setup();
        ws.write_file("inputs", "a.xlsx", b"x").unwrap();
        NewDocumentTool::new(w.clone()).run(NewDocumentArgs { kind: "writer".into(), name: None }).await.unwrap();
        let t = ListWorkspaceTool::new(w, ws);
        let all = t.run(ListWorkspaceArgs { dir: None }).await.unwrap();
        assert_eq!(all.files.len(), 3);
        assert_eq!(all.files["inputs"][0].virtual_path, "/inputs/a.xlsx");
        assert_eq!(all.open_documents.len(), 1);
        assert!(all.open_documents[0].current);
        let one = t.run(ListWorkspaceArgs { dir: Some("exports".into()) }).await.unwrap();
        assert_eq!(one.files.len(), 1);
        assert!(t.run(ListWorkspaceArgs { dir: Some("etc".into()) }).await.is_err());
    }
}
