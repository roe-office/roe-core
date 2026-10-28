//! CLI 工具调用日志:`--verbose` 时把模型的每次工具调用与结果打到 stderr。
//! 日志一行一条;长内容按字符(不是字节)截断,避免中文被切坏、脚本全文刷屏。

/// 日志里单段内容(参数/结果)最多保留的字符数。
pub const LOG_MAX_CHARS: usize = 300;

/// 把换行压成空格(`\r\n` 算一个),再按字符截断;超长时末尾加 `…`。
pub fn truncate_chars(s: &str, n: usize) -> String {
    // 先压平换行:\r\n 算一个空格,再处理单独的 \n、\r
    let flat = s.replace("\r\n", " ").replace(['\n', '\r'], " ");
    if flat.chars().count() <= n {
        return flat;
    }
    let mut out: String = flat.chars().take(n).collect();
    out.push('…');
    out
}

use rig_agent::agent::hook::ToolCall;
use rig_agent::agent::{AgentHook, HookContext, ToolCallAction, ToolResultAction, ToolResultEvent};

/// 只观察、不改变行为的日志 hook。`enabled=false` 时什么都不打印。
/// 始终挂载(add_hook 是泛型按值传入,不能按条件挂),靠 enabled 开关。
pub struct LogHook {
    pub enabled: bool,
}

impl AgentHook for LogHook {
    async fn on_tool_call(&self, _ctx: &HookContext, event: ToolCall<'_>) -> ToolCallAction {
        if self.enabled {
            eprintln!(
                "[工具调用] {} {}",
                event.tool_name,
                truncate_chars(event.args, LOG_MAX_CHARS)
            );
        }
        ToolCallAction::Run
    }

    async fn on_tool_result(
        &self,
        _ctx: &HookContext,
        event: ToolResultEvent<'_>,
    ) -> ToolResultAction {
        if self.enabled {
            // Debug 含 status(成功/失败/拒绝/跳过),四态都能分辨
            eprintln!(
                "[工具结果] {} {:?} {}",
                event.tool_name,
                event.raw_result,
                truncate_chars(&event.presentation.render(), LOG_MAX_CHARS)
            );
        }
        ToolResultAction::Keep
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_chinese_by_chars_not_bytes() {
        assert_eq!(truncate_chars("东区西区南区", 2), "东区…");
    }

    #[test]
    fn exactly_n_chars_is_unchanged() {
        assert_eq!(truncate_chars("东区", 2), "东区");
    }

    #[test]
    fn shorter_than_n_is_unchanged() {
        assert_eq!(truncate_chars("东", 5), "东");
    }

    #[test]
    fn zero_limit_gives_ellipsis_for_nonempty_and_empty_for_empty() {
        assert_eq!(truncate_chars("abc", 0), "…");
        assert_eq!(truncate_chars("", 0), "");
    }

    #[test]
    fn newlines_are_flattened_to_single_spaces() {
        assert_eq!(truncate_chars("a\r\nb\nc\rd", 100), "a b c d");
    }

    #[test]
    fn flatten_happens_before_counting() {
        // 压平后是 "a b c"(5 个字符),n=5 时原样返回
        assert_eq!(truncate_chars("a\nb\nc", 5), "a b c");
    }
}
