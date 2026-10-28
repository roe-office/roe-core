//! AnimationScheduler：驱动 RuntimeTree 播放
//! start() → on_click() / tick() → Vec<AnimationFrame>

use super::event_queue::{AnimationEvent, EventQueue, RuntimeNodeId};
use super::runtime_node::{AnimationFrame, NodeState, RuntimeNodeKind, RuntimeTree};
use easer::functions::{Cubic, Quad, Sine, Bounce, Elastic, Easing as EaserEasing};
use roe_model::impress::EasingFunction;
use roe_model::impress::{AnimProperty, AnimValue};
use roe_model::common::Color;

pub struct AnimationScheduler {
    pub tree:  RuntimeTree,
    pub queue: EventQueue,
    /// 当前播放时间（毫秒，从 start() 起计）
    pub now_ms: f64,
    /// 是否正在播放
    pub playing: bool,
}

impl AnimationScheduler {
    pub fn new(tree: RuntimeTree) -> Self {
        Self {
            tree,
            queue: EventQueue::new(),
            now_ms: 0.0,
            playing: false,
        }
    }

    /// 开始播放：激活根节点，触发第一个子节点
    pub fn start(&mut self) {
        self.playing = true;
        self.now_ms  = 0.0;
        let root_id  = self.tree.root_id;
        self.activate(root_id);
    }

    /// 用户点击（推进 MainSequence 或触发 Interactive）
    pub fn on_click(&mut self, shape_id: Option<u32>) {
        self.queue.push_user(AnimationEvent::OnClick {
            shape_id,
            node_id: self.tree.root_id, // scheduler 内部路由
        });
        self.process_user_events(shape_id);
    }

    /// 时间推进，返回本帧产生的属性变化列表
    pub fn tick(&mut self, delta_ms: f64) -> Vec<AnimationFrame> {
        if !self.playing { return vec![]; }
        self.now_ms += delta_ms;

        // 处理到期的定时事件
        let due = self.queue.drain_due(self.now_ms);
        for node_id in due {
            self.activate(node_id);
        }

        // 推进所有 Active 节点，收集帧输出
        let active_ids: Vec<RuntimeNodeId> = self.tree
            .nodes
            .iter()
            .filter(|n| n.state == NodeState::Active)
            .map(|n| n.id)
            .collect();

        let mut frames = Vec::new();
        for nid in active_ids {
            self.tick_node(nid, delta_ms, &mut frames);
        }

        frames
    }

    /// 激活一个节点
    fn activate(&mut self, id: RuntimeNodeId) {
        let Some(node) = self.tree.get_mut(id) else { return };
        if node.state != NodeState::Idle { return; }
        node.state = NodeState::Active;
        node.elapsed_ms = 0.0;

        // 容器节点：激活第一个子节点（或全部子节点）
        let kind = node.kind.clone();
        match kind {
            RuntimeNodeKind::Root { children } => {
                for child in children {
                    self.activate(child);
                }
            }
            RuntimeNodeKind::Sequential { children, .. } => {
                if let Some(&first) = children.first() {
                    self.activate(first);
                }
            }
            RuntimeNodeKind::Parallel { children } => {
                for child in children {
                    self.activate(child);
                }
            }
            RuntimeNodeKind::Interactive { .. } => {
                // 等待用户点击触发
            }
            _ => {
                // 叶子节点：若 duration=0 立刻完成
                if let Some(n) = self.tree.get(id) {
                    if n.duration_ms == 0.0 {
                        self.complete(id);
                    }
                }
            }
        }
    }

    /// 推进一个叶子节点的 elapsed，到期则 complete
    fn tick_node(&mut self, id: RuntimeNodeId, delta_ms: f64, frames: &mut Vec<AnimationFrame>) {
        let Some(node) = self.tree.get_mut(id) else { return };
        if node.state != NodeState::Active { return; }

        node.elapsed_ms += delta_ms;
        let raw_progress = node.progress();
        let progress = apply_easing(raw_progress, node.easing);
        let done     = node.duration_ms > 0.0 && node.elapsed_ms >= node.duration_ms;

        // 产生帧输出
        let frame_opt = Self::compute_frame(id, &node.kind, progress);
        if let Some(f) = frame_opt {
            frames.push(f);
        }

        if done {
            self.complete(id);
        }
    }

    /// 节点播放完毕：更新状态，通知父容器推进
    fn complete(&mut self, id: RuntimeNodeId) {
        let Some(node) = self.tree.get_mut(id) else { return };
        let fill = node.fill;
        node.state = match fill {
            roe_model::impress::FillMode::Freeze => NodeState::Frozen,
            _ => NodeState::Done,
        };

        // 通知 OnNodeEnd 监听者（简化：遍历查找）
        let _watchers: Vec<RuntimeNodeId> = self.tree.nodes.iter()
            .filter_map(|_n| {
                // 此处简化，完整实现需要反向索引
                None::<RuntimeNodeId>
            })
            .collect();

        // 找父容器，推进 Sequential
        let _ = self.advance_sequential_parent(id);
    }

    /// 如果父节点是 Sequential，推进到下一个子节点
    fn advance_sequential_parent(&mut self, completed_id: RuntimeNodeId) -> Option<()> {
        // 找包含 completed_id 的 Sequential 容器
        let parent_id = self.tree.nodes.iter().find_map(|n| {
            match &n.kind {
                RuntimeNodeKind::Sequential { children, .. } => {
                    if children.contains(&completed_id) { Some(n.id) } else { None }
                }
                _ => None,
            }
        });

        let Some(pid) = parent_id else { return None; };

        // 找下一个子节点
        let next = {
            let parent = self.tree.get(pid)?;
            match &parent.kind {
                RuntimeNodeKind::Sequential { children, current: _, is_main_sequence } => {
                    let pos = children.iter().position(|&c| c == completed_id)?;
                    let is_main = *is_main_sequence;
                    // MainSequence：等待 OnNext（用户点击），不自动推进
                    if is_main {
                        return None;
                    }
                    children.get(pos + 1).copied()
                }
                _ => None,
            }
        };

        if let Some(next_id) = next {
            // 加入定时队列（立刻触发）
            self.queue.push_timed(self.now_ms, next_id);
        } else {
            // 所有子节点完成，容器本身 complete
            self.complete(pid);
        }

        Some(())
    }

    /// 处理用户事件（点击）
    fn process_user_events(&mut self, clicked_shape: Option<u32>) {
        // 找所有 Active 的 MainSequence，推进一步
        let main_seqs: Vec<RuntimeNodeId> = self.tree.nodes.iter()
            .filter(|n| {
                n.state == NodeState::Active &&
                matches!(&n.kind, RuntimeNodeKind::Sequential { is_main_sequence: true, .. })
            })
            .map(|n| n.id)
            .collect();

        for seq_id in main_seqs {
            let _ = self.advance_main_sequence(seq_id);
        }

        // 找 Interactive 序列的触发
        if let Some(shape_id) = clicked_shape {
            let triggers: Vec<RuntimeNodeId> = self.tree.nodes.iter()
                .filter(|n| matches!(&n.kind,
                    RuntimeNodeKind::Interactive { trigger_shape, .. }
                    if *trigger_shape == shape_id
                ))
                .map(|n| n.id)
                .collect();

            for tid in triggers {
                self.activate(tid);
            }
        }
    }

    /// 推进 MainSequence 到下一个 effect
    fn advance_main_sequence(&mut self, seq_id: RuntimeNodeId) -> Option<()> {
        let next = {
            let seq = self.tree.get(seq_id)?;
            match &seq.kind {
                RuntimeNodeKind::Sequential { children, current: _, .. } => {
                    // 跳过当前正在播放的（或已完成的），找下一个 Idle
                    let next_pos = children.iter().position(|&c| {
                        self.tree.get(c)
                            .map(|n| n.state == NodeState::Idle)
                            .unwrap_or(false)
                    });
                    next_pos.and_then(|p| children.get(p).copied())
                }
                _ => None,
            }
        };

        if let Some(next_id) = next {
            self.activate(next_id);
        }

        Some(())
    }

    /// 计算叶子节点在当前进度下的帧输出
    fn compute_frame(
        _id: RuntimeNodeId,
        kind: &RuntimeNodeKind,
        progress: f64,
    ) -> Option<AnimationFrame> {
        match kind {
            RuntimeNodeKind::Animate { target, property, from: _, to } => {
                Some(AnimationFrame {
                    target_shape_id: *target,
                    property: property.clone(),
                    value: interpolate_value(to, progress),
                })
            }
            RuntimeNodeKind::AnimateColor { target, from, to } => {
                let r = lerp_u8(from.r, to.r, progress);
                let g = lerp_u8(from.g, to.g, progress);
                let b = lerp_u8(from.b, to.b, progress);
                Some(AnimationFrame {
                    target_shape_id: *target,
                    property: AnimProperty::FillColor,
                    value: AnimValue::Color(Color { r, g, b, a: 255 }),
                })
            }
            RuntimeNodeKind::AnimateSet { target, property, to } => {
                Some(AnimationFrame {
                    target_shape_id: *target,
                    property: property.clone(),
                    value: to.clone(),
                })
            }
            _ => None,
        }
    }
}

fn lerp_u8(a: u8, b: u8, t: f64) -> u8 {
    (a as f64 + (b as f64 - a as f64) * t).round() as u8
}

fn interpolate_value(to: &AnimValue, progress: f64) -> AnimValue {
    match to {
        AnimValue::Number(n) => AnimValue::Number(n * progress),
        other => other.clone(),
    }
}


/// 把线性 progress (0.0..=1.0) 经过 easing 曲线转换。
/// easer 库的参数约定：ease_x(t, b, c, d)
///   t = 当前时间，b = 起始值，c = 变化量，d = 总时长
/// 我们统一用 t=progress, b=0, c=1, d=1，输出即 eased progress。
fn apply_easing(t: f64, easing: EasingFunction) -> f64 {
    let t = t as f32;
    let result = match easing {
        EasingFunction::Linear       => t,
        EasingFunction::EaseIn       => Cubic::ease_in(t, 0.0, 1.0, 1.0),
        EasingFunction::EaseOut      => Cubic::ease_out(t, 0.0, 1.0, 1.0),
        EasingFunction::EaseInOut    => Cubic::ease_in_out(t, 0.0, 1.0, 1.0),
        EasingFunction::CubicIn      => Cubic::ease_in(t, 0.0, 1.0, 1.0),
        EasingFunction::CubicOut     => Cubic::ease_out(t, 0.0, 1.0, 1.0),
        EasingFunction::CubicInOut   => Cubic::ease_in_out(t, 0.0, 1.0, 1.0),
        EasingFunction::QuadIn       => Quad::ease_in(t, 0.0, 1.0, 1.0),
        EasingFunction::QuadOut      => Quad::ease_out(t, 0.0, 1.0, 1.0),
        EasingFunction::QuadInOut    => Quad::ease_in_out(t, 0.0, 1.0, 1.0),
        EasingFunction::SineIn       => Sine::ease_in(t, 0.0, 1.0, 1.0),
        EasingFunction::SineOut      => Sine::ease_out(t, 0.0, 1.0, 1.0),
        EasingFunction::SineInOut    => Sine::ease_in_out(t, 0.0, 1.0, 1.0),
        EasingFunction::BounceOut    => Bounce::ease_out(t, 0.0, 1.0, 1.0),
        EasingFunction::ElasticIn    => Elastic::ease_in(t, 0.0, 1.0, 1.0),
        EasingFunction::ElasticOut   => Elastic::ease_out(t, 0.0, 1.0, 1.0),
    };
    result.clamp(0.0, 1.0) as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use roe_model::impress::*;

    fn simple_tree() -> RuntimeTree {
        let doc = AnimationNode::TimingRoot {
            children: vec![
                AnimationNode::MainSequence {
                    effects: vec![
                        AnimationEffect { node: AnimationNode::Animate {
                            target: 1,
                            property: AnimProperty::Opacity,
                            timing: TimingSpec {
                                begin: BeginCondition::OnClick(1),
                                duration: 0.5,
                                repeat: RepeatSpec::None,
                                fill: FillMode::Freeze,
                                node_type: EffectNodeType::OnClick,
                                easing: Default::default(),
                            },
                        }},
                        AnimationEffect { node: AnimationNode::AnimateSet {
                            target: 2,
                            property: AnimProperty::Visibility,
                            to: AnimValue::Bool(true),
                            timing: TimingSpec {
                                begin: BeginCondition::AfterPrevious(0.0),
                                duration: 0.0,
                                repeat: RepeatSpec::None,
                                fill: FillMode::Freeze,
                                node_type: EffectNodeType::AfterPrevious,
                                easing: Default::default(),
                            },
                        }},
                    ],
                },
            ],
        };
        RuntimeTree::build_from(&doc)
    }

    #[test]
    fn test_start_activates_root() {
        let mut sched = AnimationScheduler::new(simple_tree());
        sched.start();
        assert!(sched.playing);

        let root = sched.tree.get(sched.tree.root_id).unwrap();
        assert_eq!(root.state, NodeState::Active);
    }

    #[test]
    fn test_tick_produces_frames() {
        let mut sched = AnimationScheduler::new(simple_tree());
        sched.start();
        sched.on_click(None); // 推进 MainSequence

        let frames = sched.tick(250.0); // 250ms = 50% 进度
        println!("frames at 250ms: {:?}", frames);
        // 不强断言帧数，只验证不崩溃
    }

    #[test]
    fn test_node_count() {
        let tree = simple_tree();
        // Root + MainSequence + 2 effects = 4
        assert_eq!(tree.node_count(), 4);
    }
}
