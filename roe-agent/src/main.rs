//! roe-agent CLI —— 多轮对话,脚本直接对文档生效(无确认流程)。
//!
//! 无头模式:启动时没有任何文档,由模型自己 new_document / open_document /
//! save_document。文档由专属的 DocWorker 线程持有,脚本在该线程上执行。
//! 失败时文档自动回滚;成功的修改进撤销栈。
//!
//! 参数:--workspace <目录>(默认 ./roe_workspace);--input <文件>(可重复,
//! 启动时复制进工作区 /inputs/,保留原名)。

use std::io::{self, Write};
use std::sync::Arc;

use futures::StreamExt;
use rig_agent::agent::MultiTurnStreamItem;
use rig_agent::prelude::*;
use rig_agent::streaming::StreamedAssistantContent;
use rig_core::providers::deepseek;

use roe_agent::tool::{ExecuteScriptTool, IngestMediaAssetTool, QueryScriptTool};
use roe_agent::tool_doc::{
    CloseDocumentTool, ListWorkspaceTool, NewDocumentTool, OpenDocumentTool, SaveDocumentTool,
};
use roe_agent::log_hook::LogHook;
use roe_agent::worker::DocWorker;
use roe_agent::workspace::WorkspaceContext;

/// 解析 --workspace <path> 参数,默认 "./roe_workspace"。
fn parse_workspace_root() -> std::path::PathBuf {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == "--workspace")
        .and_then(|i| args.get(i + 1))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("./roe_workspace"))
}

/// 解析所有 --input <文件>(可重复)。
fn parse_inputs() -> Vec<std::path::PathBuf> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .enumerate()
        .filter(|(_, a)| a.as_str() == "--input")
        .filter_map(|(i, _)| args.get(i + 1))
        .map(std::path::PathBuf::from)
        .collect()
}

fn prompt(label: &str) -> io::Result<String> {
    print!("{label}");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input.trim().to_string())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();

    let client = deepseek::Client::from_env()?;

    let preamble = roe_agent::prompt::headless_preamble();

    let workspace_root = parse_workspace_root();
    let workspace_ctx = WorkspaceContext::open_or_create(&workspace_root)
        .map_err(|e| format!("failed to open workspace at {}: {e}", workspace_root.display()))?;
    println!("[Agent workspace] {}", workspace_root.display());
    let workspace = Arc::new(workspace_ctx);

    for src in parse_inputs() {
        let bytes = std::fs::read(&src)
            .map_err(|e| format!("读取 --input {} 失败: {e}", src.display()))?;
        let name = src
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| format!("--input {} 没有有效文件名", src.display()))?;
        let vp = workspace.write_file("inputs", name, &bytes)?;
        println!("[已上传] {} -> {vp}", src.display());
    }

    let worker = Arc::new(DocWorker::spawn(workspace.clone())?);
    let tool = ExecuteScriptTool::new(worker.clone());
    let query_tool = QueryScriptTool::new(worker.clone());
    let ingest_tool = IngestMediaAssetTool::new(workspace.clone());
    let new_tool = NewDocumentTool::new(worker.clone());
    let open_tool = OpenDocumentTool::new(worker.clone(), workspace.clone());
    let save_tool = SaveDocumentTool::new(worker.clone(), workspace.clone());
    let close_tool = CloseDocumentTool::new(worker.clone());
    let list_tool = ListWorkspaceTool::new(worker.clone(), workspace.clone());

    let agent = client
        .agent(deepseek::DEEPSEEK_V4_FLASH)
        .preamble(&preamble)
        .tool(tool)
        .tool(query_tool)
        .tool(ingest_tool)
        .tool(new_tool)
        .tool(open_tool)
        .tool(save_tool)
        .tool(close_tool)
        .tool(list_tool)
        .build();

    let verbose = std::env::args().any(|a| a == "--verbose");
    println!("[多轮对话已就绪,输入 exit 退出]{}", if verbose { "(详细日志已开启,输出到 stderr)" } else { "" });

    let mut history: Vec<rig_core::message::Message> = Vec::new();

    loop {
        let input = prompt("> ")?;
        if input == "exit" {
            break;
        }
        if input.is_empty() {
            continue;
        }

        println!();
        let mut response_stream = agent
            .stream_prompt(input.clone())
            .history(history.clone())
            .max_turns(20)
            .add_hook(LogHook { enabled: verbose })
            .await;

        let mut acc = String::new();
        let mut new_messages = None;

        loop {
            let Some(chunk) = response_stream.next().await else {
                println!();
                break;
            };
            match chunk {
                Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(
                    text_item,
                ))) => {
                    print!("{}", text_item.text);
                    acc.push_str(&text_item.text);
                }
                Ok(MultiTurnStreamItem::FinalResponse(final_response)) => {
                    new_messages = final_response.messages().map(|h| h.to_vec());
                }
                Err(e) => {
                    println!("[错误] {e}");
                    break;
                }
                _ => continue,
            }
        }

        if let Some(messages) = new_messages {
            history.extend(messages);
        } else if !acc.is_empty() {
            history.push(rig_core::message::Message::user(input));
            history.push(rig_core::message::Message::assistant(acc));
        }
    }

    println!("\n[exports/ 里的产物]");
    if let Ok(files) = workspace.list("exports") {
        for f in files {
            match workspace.physical_path(&f.virtual_path) {
                Ok(pp) => println!("  {}  ({} 字节)  {}", f.virtual_path, f.size_bytes, pp.display()),
                Err(_) => println!("  {}  ({} 字节)", f.virtual_path, f.size_bytes),
            }
        }
    }
    Ok(())
}
