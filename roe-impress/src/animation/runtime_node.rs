//! RuntimeAnimationNode：从文档层 AnimationNode 派生的运行时节点
//! 文档树只用于编辑和持久化，播放时构建独立的 RuntimeTree

use roe_model::impress::{
    AnimationNode, FillMode,
};
use super::event_queue::RuntimeNodeId;
use serde::Serialize;

/// 运行时节点状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeState {
    Idle,       // 未激活
    Active,     // 正在播放
    Frozen,     // fill=freeze，保持最终状态
    Done,       // 播放完毕
}

/// 一帧动画的属性变化
#[derive(Debug, Clone, Serialize)]
pub struct AnimationFrame {
    pub target_shape_id: u32,
    pub property: roe_model::impress::AnimProperty,
    pub value: roe_model::impress::AnimValue,
}

/// 运行时节点（扁平化存储，用 RuntimeNodeId 引用子节点）
#[derive(Debug, Clone)]
pub struct RuntimeNode {
    pub id:       RuntimeNodeId,
    pub state:    NodeState,
    pub kind:     RuntimeNodeKind,
    /// 开始时间（相对于播放起点，毫秒）
    pub start_ms: f64,
    /// 持续时间（毫秒，-1 表示 indefinite）
    pub duration_ms: f64,
    /// fill 模式
    pub fill: FillMode,
    /// 已播放时长（毫秒）
    pub elapsed_ms: f64,
    /// easing 曲线
    pub easing: roe_model::impress::EasingFunction,
}

#[derive(Debug, Clone)]
pub enum RuntimeNodeKind {
    /// 顺序容器（MainSequence / Sequence）
    Sequential {
        children: Vec<RuntimeNodeId>,
        current:  usize,
        is_main_sequence: bool,
    },
    /// 并行容器（Parallel）
    Parallel {
        children: Vec<RuntimeNodeId>,
    },
    /// 叶子节点：属性动画
    Animate {
        target: u32,
        property: roe_model::impress::AnimProperty,
        from: Option<roe_model::impress::AnimValue>,
        to:   roe_model::impress::AnimValue,
    },
    /// 叶子节点：颜色动画
    AnimateColor {
        target: u32,
        from: roe_model::common::Color,
        to:   roe_model::common::Color,
    },
    /// 叶子节点：位移动画
    AnimateMotion {
        target: u32,
        path: String,
    },
    /// 叶子节点：Set（瞬时赋值）
    AnimateSet {
        target: u32,
        property: roe_model::impress::AnimProperty,
        to: roe_model::impress::AnimValue,
    },
    /// 交互序列容器
    Interactive {
        trigger_shape: u32,
        children: Vec<RuntimeNodeId>,
        current: usize,
    },
    /// 根节点
    Root {
        children: Vec<RuntimeNodeId>,
    },
}

impl RuntimeNode {
    pub fn is_done(&self) -> bool {
        self.state == NodeState::Done
    }

    pub fn is_active(&self) -> bool {
        self.state == NodeState::Active
    }

    /// 计算当前进度 [0.0, 1.0]
    pub fn progress(&self) -> f64 {
        if self.duration_ms <= 0.0 { return 1.0; }
        (self.elapsed_ms / self.duration_ms).clamp(0.0, 1.0)
    }
}

/// 运行时动画树（扁平 Arena）
pub struct RuntimeTree {
    pub nodes: Vec<RuntimeNode>,
    pub root_id: RuntimeNodeId,
    next_id: u32,
}

impl RuntimeTree {
    pub fn new() -> Self {
        Self { nodes: Vec::new(), root_id: RuntimeNodeId(0), next_id: 0 }
    }

    fn alloc_id(&mut self) -> RuntimeNodeId {
        let id = RuntimeNodeId(self.next_id);
        self.next_id += 1;
        id
    }

    pub fn get(&self, id: RuntimeNodeId) -> Option<&RuntimeNode> {
        self.nodes.iter().find(|n| n.id == id)
    }

    pub fn get_mut(&mut self, id: RuntimeNodeId) -> Option<&mut RuntimeNode> {
        self.nodes.iter_mut().find(|n| n.id == id)
    }

    /// 从文档层 AnimationNode 构建 RuntimeTree
    pub fn build_from(doc_node: &AnimationNode) -> Self {
        let mut tree = Self::new();
        let root_id = tree.build_node(doc_node, 0.0);
        tree.root_id = root_id;
        tree
    }

    fn build_node(&mut self, node: &AnimationNode, start_ms: f64) -> RuntimeNodeId {
        let id = self.alloc_id();
        let runtime_node = match node {
            AnimationNode::TimingRoot { children } => {
                let child_ids: Vec<_> = children.iter()
                    .map(|c| self.build_node(c, start_ms))
                    .collect();
                RuntimeNode {
                    id, state: NodeState::Idle,
                    kind: RuntimeNodeKind::Root { children: child_ids },
                    start_ms, duration_ms: -1.0,
                    fill: FillMode::Default, elapsed_ms: 0.0, easing: roe_model::impress::EasingFunction::Linear,
                }
            }
            AnimationNode::MainSequence { effects } => {
                let child_ids: Vec<_> = effects.iter()
                    .map(|e| self.build_node(&e.node, start_ms))
                    .collect();
                RuntimeNode {
                    id, state: NodeState::Idle,
                    kind: RuntimeNodeKind::Sequential {
                        children: child_ids, current: 0,
                        is_main_sequence: true,
                    },
                    start_ms, duration_ms: -1.0,
                    fill: FillMode::Default, elapsed_ms: 0.0, easing: roe_model::impress::EasingFunction::Linear,
                }
            }
            AnimationNode::Sequence { children, timing } => {
                let child_ids: Vec<_> = children.iter()
                    .map(|c| self.build_node(c, start_ms))
                    .collect();
                RuntimeNode {
                    id, state: NodeState::Idle,
                    kind: RuntimeNodeKind::Sequential {
                        children: child_ids, current: 0,
                        is_main_sequence: false,
                    },
                    start_ms,
                    duration_ms: timing.duration * 1000.0,
                    fill: timing.fill, elapsed_ms: 0.0, easing: timing.easing,
                }
            }
            AnimationNode::Parallel { children, timing } => {
                let child_ids: Vec<_> = children.iter()
                    .map(|c| self.build_node(c, start_ms))
                    .collect();
                RuntimeNode {
                    id, state: NodeState::Idle,
                    kind: RuntimeNodeKind::Parallel { children: child_ids },
                    start_ms,
                    duration_ms: timing.duration * 1000.0,
                    fill: timing.fill, elapsed_ms: 0.0, easing: timing.easing,
                }
            }
            AnimationNode::InteractiveSequence { trigger_shape_id, effects } => {
                let child_ids: Vec<_> = effects.iter()
                    .map(|e| self.build_node(&e.node, start_ms))
                    .collect();
                RuntimeNode {
                    id, state: NodeState::Idle,
                    kind: RuntimeNodeKind::Interactive {
                        trigger_shape: *trigger_shape_id,
                        children: child_ids, current: 0,
                    },
                    start_ms, duration_ms: -1.0,
                    fill: FillMode::Default, elapsed_ms: 0.0, easing: roe_model::impress::EasingFunction::Linear,
                }
            }
            AnimationNode::Animate { target, property, timing } => {
                RuntimeNode {
                    id, state: NodeState::Idle,
                    kind: RuntimeNodeKind::Animate {
                        target: *target,
                        property: property.clone(),
                        from: None,
                        to: roe_model::impress::AnimValue::Number(1.0), // 占位
                    },
                    start_ms,
                    duration_ms: timing.duration * 1000.0,
                    fill: timing.fill, elapsed_ms: 0.0, easing: timing.easing,
                }
            }
            AnimationNode::AnimateColor { target, from, to, timing } => {
                RuntimeNode {
                    id, state: NodeState::Idle,
                    kind: RuntimeNodeKind::AnimateColor {
                        target: *target, from: *from, to: *to,
                    },
                    start_ms,
                    duration_ms: timing.duration * 1000.0,
                    fill: timing.fill, elapsed_ms: 0.0, easing: timing.easing,
                }
            }
            AnimationNode::AnimateMotion { target, path, timing } => {
                RuntimeNode {
                    id, state: NodeState::Idle,
                    kind: RuntimeNodeKind::AnimateMotion {
                        target: *target, path: path.clone(),
                    },
                    start_ms,
                    duration_ms: timing.duration * 1000.0,
                    fill: timing.fill, elapsed_ms: 0.0, easing: timing.easing,
                }
            }
            AnimationNode::AnimateSet { target, property, to, timing } => {
                RuntimeNode {
                    id, state: NodeState::Idle,
                    kind: RuntimeNodeKind::AnimateSet {
                        target: *target,
                        property: property.clone(),
                        to: to.clone(),
                    },
                    start_ms,
                    duration_ms: timing.duration * 1000.0,
                    fill: timing.fill, elapsed_ms: 0.0, easing: timing.easing,
                }
            }
            AnimationNode::AnimateTransform { target, timing, .. } => {
                // 简化：当前版本当作 Animate 处理
                RuntimeNode {
                    id, state: NodeState::Idle,
                    kind: RuntimeNodeKind::Animate {
                        target: *target,
                        property: roe_model::impress::AnimProperty::Rotation,
                        from: None,
                        to: roe_model::impress::AnimValue::Number(0.0),
                    },
                    start_ms,
                    duration_ms: timing.duration * 1000.0,
                    fill: timing.fill, elapsed_ms: 0.0, easing: timing.easing,
                }
            }
            AnimationNode::Audio { timing, .. } => {
                RuntimeNode {
                    id, state: NodeState::Idle,
                    kind: RuntimeNodeKind::Animate {
                        target: 0,
                        property: roe_model::impress::AnimProperty::Visibility,
                        from: None,
                        to: roe_model::impress::AnimValue::Bool(true),
                    },
                    start_ms,
                    duration_ms: timing.duration * 1000.0,
                    fill: timing.fill, elapsed_ms: 0.0, easing: timing.easing,
                }
            }
        };
        self.nodes.push(runtime_node);
        id
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }
}

impl Default for RuntimeTree {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roe_model::impress::*;

    use roe_model::common::Color;
    fn make_timing() -> TimingSpec {
        TimingSpec {
            begin: BeginCondition::OnClick(1),
            duration: 0.5,
            repeat: RepeatSpec::None,
            fill: FillMode::Freeze,
            node_type: EffectNodeType::OnClick,
            easing: Default::default(),
        }
    }

    #[test]
    fn test_build_simple_tree() {
        let doc = AnimationNode::TimingRoot {
            children: vec![
                AnimationNode::MainSequence {
                    effects: vec![
                        AnimationEffect {
                            node: AnimationNode::Animate {
                                target: 1,
                                property: AnimProperty::Opacity,
                                timing: make_timing(),
                            },
                        },
                        AnimationEffect {
                            node: AnimationNode::AnimateColor {
                                target: 2,
                                from: Color { r: 0, g: 0, b: 0, a: 255 },
                                to:   Color { r: 255, g: 0, b: 0, a: 255 },
                                timing: make_timing(),
                            },
                        },
                    ],
                },
            ],
        };

        let tree = RuntimeTree::build_from(&doc);
        // Root + MainSequence + 2 effects = 4 nodes
        assert_eq!(tree.node_count(), 4);

        let root = tree.get(tree.root_id).unwrap();
        assert!(matches!(root.kind, RuntimeNodeKind::Root { .. }));
        assert_eq!(root.state, NodeState::Idle);
    }

    #[test]
    fn test_progress_calculation() {
        let mut node = RuntimeNode {
            id: RuntimeNodeId(0),
            state: NodeState::Active,
            kind: RuntimeNodeKind::AnimateSet {
                target: 1,
                property: roe_model::impress::AnimProperty::Opacity,
                to: roe_model::impress::AnimValue::Number(1.0),
            },
            start_ms: 0.0,
            duration_ms: 1000.0,
            fill: FillMode::Freeze,
            elapsed_ms: 500.0,
            easing: Default::default(),
        };
        assert!((node.progress() - 0.5).abs() < 0.001);

        node.elapsed_ms = 1000.0;
        assert!((node.progress() - 1.0).abs() < 0.001);
    }
}
