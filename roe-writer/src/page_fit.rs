//! PageFitChecker：物理溢出检测，找 break point
//! 输入：一组 LineBox + 页面剩余高度
//! 输出：SplitDecision

use crate::frame::LineBox;

/// 分页决策
#[derive(Debug, Clone)]
pub enum SplitDecision {
    /// 所有行都放得下，不需要分页
    Fits,
    /// 需要在第 break_at 行之前断开
    /// lines[0..break_at] 留在当前页，lines[break_at..] 移到下一页
    BreakBefore {
        break_at: usize,
    },
    /// 一行都放不下（页面剩余空间极小），整段移到下一页
    MoveToNextPage,
}

/// PageFitChecker：检查一段 lines 能否放入剩余高度
pub struct PageFitChecker {
    /// 最少保留在当前页的行数（防止 widow/orphan 在这里做粗剪）
    /// 精细调整由 WidowOrphanResolver 负责
    pub min_lines_on_page: usize,
}

impl PageFitChecker {
    pub fn new() -> Self {
        Self { min_lines_on_page: 1 }
    }

    /// 检查 lines 能否放入 remaining_height
    /// space_after：段落尾部间距（只在最后一行之后加）
    /// [WROOT-047] is_empty_container：调用方所在的页/栏在插入这段内容之前
    /// 是否为空（即 used_height_before == 0）。
    ///
    /// 根因：当一整行内容本身的高度就超过整页可用高度（常见于巨大字号的
    /// 页码/艺术字场景，例如明信片小页面配合96pt页码），此前 None 分支
    /// 无条件 MoveToNextPage——但换到下一页页面尺寸不变，同样放不下，
    /// 只会产生一个空页且内容仍然溢出，不解决任何问题，纯属误判。
    ///
    /// 依据：LOK官方测试 sw/qa/core/header_footer/HeaderFooterTest.cxx
    /// testEvenPageOddPageFooter_Import/_Roundtrip 明确断言样本
    /// tdf135216_evenOddFooter.docx 的段落2（96pt页码"2"）必须留在
    /// 第1页（getParagraph(2, u"2"_ustr)），即使其行高远超该页可用
    /// 高度——LOK允许它视觉溢出，不会因为放不下就换页。
    ///
    /// 规则：只有当容器已有内容（is_empty_container=false）时，"一行都
    /// 放不下"才应该真正换页（换页确实能腾出全新空间）；容器为空时换页
    /// 无意义（新页同样放不下），应改判 Fits，让内容留在原地、允许溢出。
    pub fn check(
        &self,
        lines: &[LineBox],
        remaining_height: f32,
        space_after: f32,
        is_empty_container: bool,
    ) -> SplitDecision {
        if lines.is_empty() {
            return SplitDecision::Fits;
        }

        // 计算每行的累积高度
        let mut cum_height = 0.0f32;
        let mut last_fitting = None; // 最后一个放得下的行 index

        for (i, line) in lines.iter().enumerate() {
            cum_height += line.line_height;
            // 最后一行额外加 space_after
            let total = if i == lines.len() - 1 {
                cum_height + space_after
            } else {
                cum_height
            };

            if total <= remaining_height + 0.5 { // 0.5pt 浮点容差
                last_fitting = Some(i);
            } else {
                break;
            }
        }

        match last_fitting {
            None => {
                // [WROOT-047] 一行都放不下。若容器本来就是空的，换页无法
                // 腾出更多空间（新页尺寸相同，同样放不下），换页只会产生
                // 一个空页且问题依旧存在——此时应就地放置、允许视觉溢出，
                // 与LOK对巨字号单行内容的处理一致（见上方函数注释）。
                // 只有容器已有内容时，换页才是有意义的（新页从头开始，
                // 至少这一整行能独占一页，比硬塞在已有内容后面更合理）。
                if is_empty_container {
                    SplitDecision::Fits
                } else {
                    SplitDecision::MoveToNextPage
                }
            }
            Some(idx) if idx == lines.len() - 1 => {
                // 全部放得下
                SplitDecision::Fits
            }
            Some(idx) => {
                // 前 idx+1 行放得下，从 idx+1 开始断
                let break_at = idx + 1;
                // 保证当前页至少留 min_lines_on_page 行
                if break_at < self.min_lines_on_page {
                    SplitDecision::MoveToNextPage
                } else {
                    SplitDecision::BreakBefore { break_at }
                }
            }
        }
    }
}

impl Default for PageFitChecker {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_lines(count: usize, line_height: f32) -> Vec<LineBox> {
        (0..count).map(|i| LineBox {
            y_offset: i as f32 * line_height,
            ascent: line_height * 0.8,
            descent: line_height * 0.2,
            line_height,
            text: format!("line {i}"),
            runs: Vec::new(),
            ..Default::default()
        }).collect()
    }

    #[test]
    fn test_all_fit() {
        let checker = PageFitChecker::new();
        let lines = make_lines(3, 20.0);
        // 3 * 20 + 4 space_after = 64, remaining = 100
        assert!(matches!(checker.check(&lines, 100.0, 4.0, false), SplitDecision::Fits));
    }

    #[test]
    fn test_break_in_middle() {
        let checker = PageFitChecker::new();
        let lines = make_lines(5, 20.0);
        // remaining = 65: 3 行 (60pt) 放得下，第 4 行 (80pt) 放不下
        let decision = checker.check(&lines, 65.0, 4.0, false);
        assert!(matches!(decision, SplitDecision::BreakBefore { break_at: 3 }));
    }

    #[test]
    fn test_move_to_next() {
        let checker = PageFitChecker::new();
        let lines = make_lines(3, 20.0);
        // remaining = 5: 一行都放不下
        assert!(matches!(checker.check(&lines, 5.0, 4.0, false), SplitDecision::MoveToNextPage));
    }
}
