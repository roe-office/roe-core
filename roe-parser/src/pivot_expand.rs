//! xlsx 透视表 r 编码 → PivotHeaderNode 树的展开算法
//! 独立模块，不与渲染逻辑耦合。

use roe_model::calc::{PivotHeaderNode};

/// 单条压缩路径记录（对应 xlsx colItems/rowItems 里的一个 <i> 元素）
#[derive(Debug, Clone)]
pub struct CompressedItem {
    /// 从第几层开始是新值（r 属性，省略=0）
    pub r: usize,
    /// 该条自己负责的各层值（从第 r 层到最内层）
    /// 长度 = 总层数 - r
    pub values: Vec<String>,
    /// 是否为小计/总计节点（t="default" 或 t="grand"）
    pub is_subtotal: bool,
}

/// 把压缩的 items 列表展开成 PivotHeaderNode 树
///
/// `depth`：总层数（col_field_names.len() 或 row_field_names.len()）
/// 返回虚拟根节点（value=""），其 children 是外层字段各取值节点。
pub fn expand_to_tree(items: &[CompressedItem], depth: usize) -> PivotHeaderNode {
    let mut root = PivotHeaderNode::new("");

    // 当前完整路径（长度=depth），回溯时复用
    let mut current_path: Vec<String> = vec![String::new(); depth];

    for item in items {
        // 用 item 自己的 values 填充第 r 层到最内层
        let new_start = item.r;
        for (i, v) in item.values.iter().enumerate() {
            if new_start + i < depth {
                current_path[new_start + i] = v.clone();
            }
        }

        // 只有当这条记录本身覆盖到最内层时才插入完整路径；
        // 若这条记录只声明了外层部分（r + values.len() < depth），
        // 说明它只是外层占位声明（如 <i><x/></i> 单独出现），
        // 不代表一条完整数据路径，跳过，等待后续更深记录以相同前缀插入。
        if item.r + item.values.len() >= depth {
            insert_path(&mut root, &current_path, item.is_subtotal);
        }
    }

    root
}

/// 把一条完整路径插入树（若路径已存在则复用节点，只在末端追加）
fn insert_path(root: &mut PivotHeaderNode, path: &[String], is_subtotal: bool) {
    if path.is_empty() {
        return;
    }
    let value = &path[0];
    // 找已有子节点
    let pos = root.children.iter().position(|c| &c.value == value && !c.is_subtotal);
    let child = if let Some(idx) = pos {
        &mut root.children[idx]
    } else {
        let node = PivotHeaderNode {
            value: value.clone(),
            is_subtotal: is_subtotal && path.len() == 1,
            ..Default::default()
        };
        root.children.push(node);
        root.children.last_mut().unwrap()
    };
    insert_path(child, &path[1..], is_subtotal);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 模拟样例文件的列字段：Color(红/绿/蓝) → Size(大/小) → 值字段层(Sales/Quantity)
    /// 共3层，depth=3
    #[test]
    fn test_expand_col_items() {
        let items = vec![
            CompressedItem { r: 0, values: vec!["红".into(), "大".into(), "Sales".into()], is_subtotal: false },
            CompressedItem { r: 2, values: vec!["Quantity".into()], is_subtotal: false },
            CompressedItem { r: 1, values: vec!["小".into(), "Sales".into()], is_subtotal: false },
            CompressedItem { r: 2, values: vec!["Quantity".into()], is_subtotal: false },
            CompressedItem { r: 0, values: vec!["绿".into(), "大".into(), "Sales".into()], is_subtotal: false },
            CompressedItem { r: 2, values: vec!["Quantity".into()], is_subtotal: false },
            CompressedItem { r: 1, values: vec!["小".into(), "Sales".into()], is_subtotal: false },
            CompressedItem { r: 2, values: vec!["Quantity".into()], is_subtotal: false },
        ];

        let mut tree = expand_to_tree(&items, 3);
        tree.compute_spans(1);

        // 根节点有2个子节点：红、绿
        assert_eq!(tree.children.len(), 2);
        assert_eq!(tree.children[0].value, "红");
        assert_eq!(tree.children[1].value, "绿");

        // 红 → 大、小
        let red = &tree.children[0];
        assert_eq!(red.children.len(), 2);
        assert_eq!(red.children[0].value, "大");
        assert_eq!(red.children[1].value, "小");

        // 红/大 → Sales、Quantity
        let red_big = &red.children[0];
        assert_eq!(red_big.children.len(), 2);
        assert_eq!(red_big.children[0].value, "Sales");
        assert_eq!(red_big.children[1].value, "Quantity");

        // cached_span：红 = 4（大2+小2），根 = 8
        assert_eq!(red.cached_span, 4);
        assert_eq!(tree.cached_span, 8);
    }
}
