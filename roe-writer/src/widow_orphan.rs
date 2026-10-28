//! WidowOrphanResolver：在 PageFitChecker 给出 break point 后做后调整
//! 孤行（orphan）：段落第一行单独留在页底
//! 寡行（widow） ：段落最后一行单独出现在下一页页顶

use crate::page_fit::SplitDecision;

/// 孤行/寡行调整结果
#[derive(Debug, Clone)]
pub enum ResolveResult {
    /// 不需要调整，使用原 break_at
    Keep(usize),
    /// 把 break_at 提前（当前页少放几行，避免孤行）
    ShiftUp(usize),
    /// 整段移到下一页
    MoveToNextPage,
}

pub struct WidowOrphanResolver {
    /// 段落在页底最少保留的行数（防孤行）
    pub min_orphan_lines: usize,
    /// 段落在下一页顶部最少保留的行数（防寡行）
    pub min_widow_lines: usize,
}

impl WidowOrphanResolver {
    pub fn new() -> Self {
        Self {
            min_orphan_lines: 2,
            min_widow_lines:  2,
        }
    }

    /// decision: PageFitChecker 的输出
    /// total_lines: 该段落的总行数
    pub fn resolve(&self, decision: SplitDecision, total_lines: usize) -> ResolveResult {
        match decision {
            SplitDecision::Fits => ResolveResult::Keep(total_lines),
            SplitDecision::MoveToNextPage => ResolveResult::MoveToNextPage,
            SplitDecision::BreakBefore { break_at } => {
                let lines_on_current = break_at;
                let lines_on_next    = total_lines - break_at;

                // 孤行检测：当前页的行数不足 min_orphan_lines
                if lines_on_current < self.min_orphan_lines {
                    return ResolveResult::MoveToNextPage;
                }

                // 寡行检测：下一页的行数不足 min_widow_lines
                if lines_on_next < self.min_widow_lines {
                    // 尝试把 break_at 提前，让更多行移到下一页
                    let new_break = total_lines.saturating_sub(self.min_widow_lines);
                    if new_break < self.min_orphan_lines {
                        // 提前后孤行问题又出现了，整段移走
                        return ResolveResult::MoveToNextPage;
                    }
                    return ResolveResult::ShiftUp(new_break);
                }

                ResolveResult::Keep(break_at)
            }
        }
    }
}

impl Default for WidowOrphanResolver {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_split_needed() {
        let r = WidowOrphanResolver::new();
        let res = r.resolve(SplitDecision::Fits, 5);
        assert!(matches!(res, ResolveResult::Keep(5)));
    }

    #[test]
    fn test_orphan_triggers_move() {
        let r = WidowOrphanResolver::new();
        // break_at=1，当前页只有1行 < min_orphan=2，整段移走
        let res = r.resolve(SplitDecision::BreakBefore { break_at: 1 }, 5);
        assert!(matches!(res, ResolveResult::MoveToNextPage));
    }

    #[test]
    fn test_widow_triggers_shift() {
        let r = WidowOrphanResolver::new();
        // total=5, break_at=4 → 下一页只有1行 < min_widow=2，ShiftUp to 3
        let res = r.resolve(SplitDecision::BreakBefore { break_at: 4 }, 5);
        assert!(matches!(res, ResolveResult::ShiftUp(3)));
    }

    #[test]
    fn test_clean_break() {
        let r = WidowOrphanResolver::new();
        // total=6, break_at=3 → 两边各3行，都OK
        let res = r.resolve(SplitDecision::BreakBefore { break_at: 3 }, 6);
        assert!(matches!(res, ResolveResult::Keep(3)));
    }
}
