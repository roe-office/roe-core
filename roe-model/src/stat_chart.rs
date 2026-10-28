//! 统计图表(Office 2016+ chartEx)的数据模型。
//! 与普通 ChartModel 并列,保存原始数据,分箱/分位数在渲染时由 stats 模块计算。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StatChartKind {
    Histogram,
    BoxWhisker,
    Waterfall,
    Funnel,
    Sunburst,
    Treemap,
}

/// 一组数据(对应 `<cx:data>`)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatData {
    pub id: u32,
    /// 分类维度,每一层一个 Vec(旭日/树状图有多层),按文件中出现的顺序。
    pub categories: Vec<Vec<String>>,
    /// 数值列,空单元格为 None。
    pub values: Vec<Option<f64>>,
    /// numDim 的 type 属性:"val" 或 "size"。
    pub value_kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatDataLabels {
    /// pos 属性,如 "outEnd" "ctr" "inEnd"
    pub pos: String,
    pub show_value: bool,
    pub show_category: bool,
    pub show_series: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatSeries {
    pub name: String,
    pub layout_id: String,
    pub format_idx: u32,
    pub data_id: u32,
    /// 直方图:组距/组数(都为 None 表示自动)
    pub bin_width: Option<f64>,
    pub bin_count: Option<usize>,
    /// 直方图:intervalClosed="r" 为右闭
    pub interval_closed_right: bool,
    /// 箱线图:quartileMethod 是否为 exclusive
    pub quartile_exclusive: bool,
    pub mean_line: bool,
    pub mean_marker: bool,
    pub show_outliers: bool,
    pub show_non_outliers: bool,
    /// 瀑布图:汇总柱的下标
    pub subtotals: Vec<usize>,
    pub data_labels: Option<StatDataLabels>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatChartModel {
    pub kind: StatChartKind,
    /// 文档里有标题块(文字可能为空,渲染层用默认标题)
    pub title_present: bool,
    pub title: Option<String>,
    pub show_legend: bool,
    pub legend_pos: Option<String>,
    pub show_grid: bool,
    /// `<cx:catScaling gapWidth>`,柱间距比例(直方图 0、箱线图 1、瀑布 0.5、漏斗 0.06)
    pub gap_width: Option<f64>,
    pub data: Vec<StatData>,
    pub series: Vec<StatSeries>,
}
