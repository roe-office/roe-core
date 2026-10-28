#[derive(Debug, Clone, Default)]
pub struct LayoutConstraints {
    pub remaining_height: i32,  // 已扣除 footnote + float + table
    pub footnote_height:  i32,
    pub grow_potential:   i32,
}
