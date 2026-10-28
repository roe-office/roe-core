//! AnimationEvent + EventQueue
//! 统一单队列：时间触发用 BinaryHeap，用户交互用 Vec

use std::collections::BinaryHeap;
use std::cmp::Ordering;

/// 运行时节点 ID（调度层内部使用）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RuntimeNodeId(pub u32);

/// 动画事件
#[derive(Debug, Clone)]
pub enum AnimationEvent {
    /// 定时触发：在 activate_at 毫秒时激活 node
    TimeDelay {
        activate_at: f64,
        node_id: RuntimeNodeId,
    },
    /// 用户点击触发（shape_id=None 表示点击任意位置）
    OnClick {
        shape_id: Option<u32>,
        node_id: RuntimeNodeId,
    },
    /// MainSequence 推进（用户点击下一步）
    OnNext {
        node_id: RuntimeNodeId,
    },
    /// 监听另一节点开始时触发
    OnNodeBegin {
        watch:  RuntimeNodeId,
        notify: RuntimeNodeId,
    },
    /// 监听另一节点结束时触发
    OnNodeEnd {
        watch:  RuntimeNodeId,
        notify: RuntimeNodeId,
    },
}

/// 带时间戳的定时事件（用于 BinaryHeap，最小堆）
#[derive(Debug, Clone)]
pub struct TimedEvent {
    pub activate_at: f64,   // 毫秒
    pub node_id: RuntimeNodeId,
}

impl PartialEq for TimedEvent {
    fn eq(&self, other: &Self) -> bool {
        self.activate_at == other.activate_at
    }
}
impl Eq for TimedEvent {}

impl PartialOrd for TimedEvent {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

// 最小堆：activate_at 越小优先级越高
impl Ord for TimedEvent {
    fn cmp(&self, other: &Self) -> Ordering {
        other.activate_at.partial_cmp(&self.activate_at)
            .unwrap_or(Ordering::Equal)
    }
}

/// 统一事件队列
pub struct EventQueue {
    /// 定时事件（最小堆，按 activate_at 排序）
    pub timed: BinaryHeap<TimedEvent>,
    /// 用户交互事件（按插入顺序处理）
    pub user:  Vec<AnimationEvent>,
}

impl EventQueue {
    pub fn new() -> Self {
        Self {
            timed: BinaryHeap::new(),
            user:  Vec::new(),
        }
    }

    /// 注册一个定时事件
    pub fn push_timed(&mut self, activate_at: f64, node_id: RuntimeNodeId) {
        self.timed.push(TimedEvent { activate_at, node_id });
    }

    /// 注册一个用户交互事件
    pub fn push_user(&mut self, event: AnimationEvent) {
        self.user.push(event);
    }

    /// 取出所有在 now 之前（含）应触发的定时事件
    pub fn drain_due(&mut self, now: f64) -> Vec<RuntimeNodeId> {
        let mut due = Vec::new();
        while let Some(top) = self.timed.peek() {
            if top.activate_at <= now + 0.5 { // 0.5ms 容差
                due.push(self.timed.pop().unwrap().node_id);
            } else {
                break;
            }
        }
        due
    }

    /// 取出所有用户事件
    pub fn drain_user(&mut self) -> Vec<AnimationEvent> {
        std::mem::take(&mut self.user)
    }

    pub fn is_empty(&self) -> bool {
        self.timed.is_empty() && self.user.is_empty()
    }
}

impl Default for EventQueue {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_timed_order() {
        let mut q = EventQueue::new();
        q.push_timed(300.0, RuntimeNodeId(3));
        q.push_timed(100.0, RuntimeNodeId(1));
        q.push_timed(200.0, RuntimeNodeId(2));

        // 只有 <= 150ms 的应该触发
        let due = q.drain_due(150.0);
        assert_eq!(due, vec![RuntimeNodeId(1)]);

        // 再 tick 到 350ms
        let due2 = q.drain_due(350.0);
        assert_eq!(due2.len(), 2);
    }

    #[test]
    fn test_user_events() {
        let mut q = EventQueue::new();
        q.push_user(AnimationEvent::OnNext { node_id: RuntimeNodeId(1) });
        q.push_user(AnimationEvent::OnClick { shape_id: Some(42), node_id: RuntimeNodeId(2) });

        let events = q.drain_user();
        assert_eq!(events.len(), 2);
        assert!(q.user.is_empty());
    }
}
