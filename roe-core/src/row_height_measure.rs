//! optimalRowHeight（ROOT-082）渲染层：自动行高测量。
//! 参照 cond_format_render.rs / sparkline_render.rs 的既有模式：
//! 渲染循环用到的子功能各自独立文件。
//! 架构基准：optimalRowHeight_design_v1.md + roe-ext_progress_v139.md 第三节。

use roe_model::calc::Sheet;

/// 已真机验证(字母/数字/中文均正常)的非 CJK 字体,放行透传。
/// 新增字体必须先验证数字字形(见 v156 的 fontique 数字消失问题),再加入此表。
const VERIFIED_FONTS: &[&str] = &[
    "HarmonyOS Sans", "DejaVu Sans Mono", "DejaVu Serif", "Liberation Mono",
];

/// 渲染层字体白名单的唯一规则:VERIFIED_FONTS ∪ Carlito ∪ 名字含 cjk。
/// `resolve_font_family_for_parley` 与 `list_available_fonts` 共用,规则只保留这一份。
pub fn is_font_allowed(name: &str) -> bool {
    VERIFIED_FONTS.iter().any(|f| name.eq_ignore_ascii_case(f))
        || name.eq_ignore_ascii_case("carlito")
        || name.to_ascii_lowercase().contains("cjk")
}

/// 系统已安装且渲染层允许的字体族名(去重、按名字不区分大小写排序)。
/// 进程内只枚举一次:FontContext::new() 较重,结果缓存在 OnceLock。
pub fn list_available_fonts() -> &'static [String] {
    static CACHE: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| {
        let mut ctx = parley::FontContext::new();
        let mut names: Vec<String> = ctx
            .collection
            .family_names()
            .filter(|n| is_font_allowed(n))
            .map(|n| n.to_string())
            .collect();
        names.sort_by_key(|n| n.to_lowercase());
        names.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
        // 折叠字重变体:去掉末尾一个字重词后,剩余名字仍在放行集合里则丢弃。
        // 字重交给单元格的 bold 属性,列表只展示基础族名。
        const WEIGHT_WORDS: &[&str] = &[
            "Thin", "ExtraLight", "Light", "DemiLight", "Medium",
            "SemiBold", "Semibold", "Bold", "Black",
        ];
        let set: std::collections::HashSet<String> =
            names.iter().map(|n| n.to_lowercase()).collect();
        names.retain(|n| {
            match n.rsplit_once(' ') {
                Some((base, last)) if WEIGHT_WORDS.contains(&last) => {
                    !set.contains(&base.to_lowercase())
                }
                _ => true,
            }
        });
        names
    })
}

/// 字体名归一化：把xlsx里解析到的字体名转换成Parley/fontique能正确
/// 查找到完整字形的名字。
///
/// 背景（roe-ext_progress_v142.md 第一节1.5条 + 中间踩坑记录）：
/// `fc-match "Calibri"` 命令行层面会正确映射到本机安装的Carlito
/// （LibreOffice维护的metric-compatible替代字体），但Parley/fontique
/// 库内部按"Calibri"这个字符串查找时**不会**经过这层fontconfig别名
/// 解析，会命中另一个字形不全的候选，实测直接导致数字/符号字形丢失
/// （"01-Jan-19"变"Jan-"、"$3,526"变"$ ,"）。命令行工具验证过的别名
/// 映射不能假设在库内部API里同样生效。
///
/// 因此：xlsx解析出的原始字体名（或fallback默认值）在传给Parley之前，
/// 必须先过这个函数做显式转换，不能直接把"Calibri"这个字符串传给
/// FontFamily::named。Layout（本文件）与Paint（tile_render_helpers.rs）
/// 两阶段都必须调用同一个函数，否则会重新引入"测绘撕裂"（字体选择
/// 维度，区别于此前系数/字号维度的撕裂）。
///
/// 已知映射：Calibri -> Carlito（唯一已验证的坑）。其余字体名暂不
/// 确定是否有同类问题，原样透传，出现新证据再补充映射表。
pub fn resolve_font_family_for_parley(name: &str) -> &str {
    // v156修复：此前只对"Calibri"做归一化，其余字体名（如"Arial"）原样
    // 透传给Parley/fontique。实测CachyOS环境下"Arial"经fontique/fontconfig
    // fallback后（fc-match确认落到"Liberation Sans"），拉丁字母能正常
    // 显示但数字字符渲染异常（只剩小数点/完全空白），根因疑似
    // fontique对该fallback链路里数字字形的宽度或cluster计算有问题
    // （v156会话真机复现：pivot_new.xlsx的Sales/Quantity两列，
    // font_family="Arial"，数字消失；同一份数据的.ods版本因ODF解析器
    // 未填充font_family、走了下方调用点的Calibri默认值，归一化到
    // Carlito后正常显示，两者唯一差异是字体名）。
    //
    // 改为白名单制：只有已知在当前渲染环境下验证正常的字体名放行
    // （CJK类字体——探针确认"Noto Sans CJK SC"渲染正常；以及Carlito
    // 本身），其余所有西文字体名一律归一化到Carlito。这是比"逐个加
    // 更多字体名特判"更保守的方案，防止未来遇到其他同类西文字体名
    // （如Calibri的其他拼写变体、Helvetica、Times New Roman等）时
    // 重复踩同一个坑——代价是牺牲了这些字体本该有的视觉差异化，
    // 已知局限（技术债）：如果未来产品需要真正忠实还原xlsx指定的
    // 具体西文字体样式，需要回头验证fontique对各字体的实际字形覆盖
    // 情况，而不是靠这个白名单一刀切。
    if is_font_allowed(name) {
        // 注意：is_cjk分支下无法直接返回name（生命周期/所有权问题——
        // name是&str，直接返回没问题，这里不需要额外处理）
        name
    } else {
        "Carlito"
    }
}

/// 水平对齐：CellFormat.align_h存的是OOXML `<alignment horizontal="..">`
/// 原始属性值字符串（"left"/"center"/"right"/"fill"/"justify"/
/// "centerContinuous"/"distributed"/"general"等，规范全集）。
/// 本次只处理三种最常见值，其余（含"general"/未指定/未知值）一律
/// 归为Left——Excel真实默认是"文本左对齐、数字右对齐"（按单元格内容
/// 类型区分），本次不做这层类型判断，统一简化为Left，是已知的
/// 合理简化（与本次align_v的简化程度一致），如后续需要更精确的
/// 类型相关默认对齐，需要另外评估。
pub fn resolve_horizontal_alignment(align_h: &str) -> parley::Alignment {
    match align_h {
        "center" | "centerContinuous" | "distributed" => parley::Alignment::Center,
        "right" => parley::Alignment::Right,
        _ => parley::Alignment::Left,
    }
}

/// 垂直对齐分类：CellFormat.align_v存的是OOXML `<alignment vertical="..">`
/// 原始属性值（"top"/"center"/"bottom"/"justify"/"distributed"）。
/// 注意：OOXML垂直居中的字符串是"center"，不是"middle"——calc.rs里
/// CellFormat.align_v字段注释写的"top/middle/bottom"是项目文档的
/// 简化说法，不是实际存储值，这里必须按OOXML原始字符串匹配。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerticalAlign {
    Top,
    Middle,
    Bottom,
}

pub fn resolve_vertical_alignment(align_v: &str) -> VerticalAlign {
    match align_v {
        "top" => VerticalAlign::Top,
        "bottom" => VerticalAlign::Bottom,
        _ => VerticalAlign::Middle,
    }
}

/// 单行文本理论行高估算（pt）。
///
/// 采用LibreOffice Calc ScColumn::GetOptimalHeight的lcl_GetAttribHeight
/// 核心公式（sc/source/core/data/column2.cxx:865），而非design_v1.md
/// 原定的"Parley字体真实ascent/descent/line_gap度量"路线（v138节记录的
/// 技术路线，本次会话经LOK断言压线失败后重新核实，改用此公式）。
///
/// 架构决策记录（用户与Gemini讨论定稿，ROOT-082）：LOK的两个官方回归
/// 测试断言（tdf123026 >2000、tdf159581 <500，单位1/100mm）是"事实
/// 标准"——它们本身就是用这套近似公式（含1.18经验系数与23twip历史
/// 修正常量）算出来的，不是排版学意义上的"真实"字体度量。改用真实
/// 字体metrics（Parley ascent/descent）虽然"更正确"，但不同字体的
/// OS/2表差异会导致无法稳定通过这两个写死数值的断言，且与Paint阶段
/// （若Paint也要保持一致）产生管线不自洽的风险。故选择精确复刻LOK
/// 公式，将魔数封装进本函数、集中记录来源，而非隐晦地散落在调用点。
///
/// 公式（LOK原始单位twip，1pt=20twip，已换算为pt）：
///   height_pt = font_size_pt * 1.18
///   STD_ROWHEIGHT_DIFF = 23twip = 1.15pt，height_pt > 1.15时减去
///   （LOK原文注释：SvxFontHeightItem高度已经含普通行距，这个常量是
///   历史遗留的行距修正值，"事实标准"，见sc/inc/global.hxx:116）
///
/// 已确认的简化（本次未实现，与LOK原公式的差异）：
/// - 不加margin（ATTR_MARGIN的top/bottom）：CellFormat结构体当前没有
///   margin字段，模型层完全不解析单元格内边距，无源数据可加，不是
///   选择性遗漏。若未来CellFormat补充margin字段，应在此函数签名增加
///   参数并把margin加回公式（对齐LOK: nHeight += top+bottom margin，
///   发生在STD_ROWHEIGHT_DIFF减法之前）。
/// - 不区分CJK/CTL/拉丁字体三套独立字号做三选一（LOK按nDefScript选
///   ATTR_FONT_HEIGHT/ATTR_CJK_FONT_HEIGHT/ATTR_CTL_FONT_HEIGHT三者
///   之一）：CellFormat.font_size是单一字段，不区分文种，模型层暂无
///   等价的多字体分类概念，统一用同一字号套用同一公式。未来若要对齐
///   OnlyOffice/MS365的CJK渲染精度，需要先在模型层引入文种感知的字号
///   概念，再回来扩展本函数。
/// - 不加emphasis mark修正（LOK: 有着重号时+25%）：模型无着重号概念。
/// - 不实现nOptimalMinRowHeight下限（GetSheetOptimalMinRowHeight）：
///   该值兜底为ScGlobal::nStdRowHeight附近的量级，对常规字号（≥8pt）
///   场景通常不会成为瓶颈，本次两个验证用例的字号（12pt）核算后不受
///   此下限影响，暂不实现对应概念。
#[inline]
pub fn estimate_line_height(font_size_pt: f32) -> f32 {
    const LOK_FONT_HEIGHT_FACTOR: f32 = 1.18;
    /// STD_ROWHEIGHT_DIFF (23 twip) 换算为 pt：23.0 / 20.0
    const STD_ROWHEIGHT_DIFF_PT: f32 = 23.0 / 20.0;

    let mut height_pt = font_size_pt * LOK_FONT_HEIGHT_FACTOR;
    if height_pt > STD_ROWHEIGHT_DIFF_PT {
        height_pt -= STD_ROWHEIGHT_DIFF_PT;
    }
    height_pt
}

/// 检查某一行是否含 wrap_text=true 的单元格（贵路径判定条件）。
/// 遍历所有列，对每列检查是否有 block 覆盖该行、且该单元格
/// cell_format 的 wrap_text 为 true。复用 tile.rs::render_viewport_into
/// 主渲染循环里"列→block→按 start_row+i 算 row_idx"的既有遍历模式
/// 与 cell_formats/col_style fallback 查找逻辑（见 tile.rs 第429行附近），
/// 不发明新的按行索引结构。
pub fn row_has_wrap_text(sheet: &Sheet, row: u32) -> bool {
    for (&col_idx, sc) in sheet.cells.cols.iter() {
        for block in &sc.blocks {
            let end = block.start_row + block.values.len() as u32;
            if row < block.start_row || row >= end {
                continue;
            }
            let cf = sheet
                .cell_formats
                .get(&(row, col_idx))
                .or_else(|| sheet.col_style.get(col_idx as usize).and_then(|o| o.as_ref()));
            if cf.and_then(|f| f.wrap_text).unwrap_or(false) {
                return true;
            }
        }
    }
    false
}

use crate::tile_geometry::CalcRenderConfig;

/// 廉价路径：单行、非wrap场景的自动行高测量。
/// 返回单位为pt（与row_height_at/RowHeight::current()同单位，不含
/// PT_TO_PX、不含zoom scale——这是文档级属性，与显示缩放无关）。
///
/// 公式与draw_cell_text完全一致（防止"测绘撕裂"，见roe-ext_progress_v139.md
/// 2.7节"方案1"）：line_height_pt = font_size_pt * 1.2，字号来源同样是
/// cell_formats.font_size（缺省12.0），不做额外padding——实测draw_cell_text
/// 的pad只影响水平方向（draw_x/clip_right），垂直方向的draw_y_top公式
/// 完全不含pad，故此处不加。
///
/// 该行可能有多列、多种字号，取该行所有单元格中最大的line_height_pt。
/// 若该行没有任何单元格（空行），返回None，调用方应fallback到
/// default_row_height。
pub fn measure_row_height_cheap(sheet: &Sheet, row: u32, _cfg: &CalcRenderConfig) -> Option<f32> {
    let mut max_line_height: Option<f32> = None;
    for (&col_idx, sc) in sheet.cells.cols.iter() {
        for block in &sc.blocks {
            let end = block.start_row + block.values.len() as u32;
            if row < block.start_row || row >= end {
                continue;
            }
            let i = (row - block.start_row) as usize;
            // 空单元格（CellValue::Empty）没有实际文字内容，理论上不需要
            // 占用行高（真实渲染时text.is_empty()会直接return，不画任何
            // 东西）。但cell_formats可能仍然设置了font_size（比如整行/整列
            // 预先设置了字号样式，尚未填入内容），是否应该让空单元格的
            // font_size也参与行高计算，取决于是否要"预留"该样式将来的
            // 内容空间。LOK的做法是空单元格不参与（GetOptimalHeight只算
            // 有内容的格），这里对齐LOK，跳过空值。
            let is_empty = matches!(
                block.values.get(i),
                Some(roe_model::calc::CellValue::Empty) | None
            );
            if is_empty {
                continue;
            }
            let cf = sheet
                .cell_formats
                .get(&(row, col_idx))
                .or_else(|| sheet.col_style.get(col_idx as usize).and_then(|o| o.as_ref()));
            let font_size_pt = cf.and_then(|f| f.font_size).unwrap_or(12.0_f32);
            // 廉价路径不构建Parley layout（只用LOK公式算单行高度，字号
            // 本身与具体字体族无关），font_family在这里无需读取，与贵
            // 路径（measure_row_height_wrap，需要真排版）区别对待。
            let line_height_pt = estimate_line_height(font_size_pt);
            max_line_height = Some(max_line_height.map_or(line_height_pt, |m: f32| m.max(line_height_pt)));
        }
    }
    max_line_height
}

#[cfg(test)]
mod estimate_line_height_lok_regression_tests {
    //! 回归测试：确认estimate_line_height对LOK官方回归测试用例
    //! （tdf123026/tdf159581_optimalRowHeight.xlsx）产出的数值落在
    //! LOK断言区间内。
    //!
    //! 背景：本次会话最初照design_v1.md v138节技术路线实现了简化系数
    //! （font_size_pt * 1.2）的廉价路径，实际对照tdf159581用例验证时
    //! 发现508 (1/100mm) 未通过LOK断言(<500)——排查后发现LOK真实公式
    //! 是font_size_pt*1.18再减去STD_ROWHEIGHT_DIFF(23twip=1.15pt)的
    //! 历史修正常量（sc/source/core/data/column2.cxx:865
    //! lcl_GetAttribHeight），不是简单的*1.2。改用此公式后两个用例都
    //! 通过验证，详见estimate_line_height函数文档注释记录的完整决策
    //! 过程与已知简化范围（不含margin、不分CJK/CTL字体、无min下限）。
    //!
    //! 保留本测试是为了防止未来有人不了解这段历史，把公式"简化"回
    //! font_size_pt*1.2这类看起来更直观的写法，导致这两个LOK官方断言
    //! 重新失败——要复现今天这轮"读源码找魔数"的排查过程成本不低。
    use super::estimate_line_height;

    /// pt -> 1/100mm 换算：1pt = 1/72 inch = 2540/72 (1/100mm)
    const PT_TO_HMM: f32 = 2540.0 / 72.0;

    #[test]
    fn tdf159581_12pt_single_line_under_500_hmm() {
        // tdf159581_optimalRowHeight.xlsx Sheet2(索引1) row 0："still"，
        // 12pt（CellFormat.font_size为None，fallback到12.0默认值），
        // 无wrap折行（单词很短）。LOK断言：<500 (1/100mm)。
        let h = estimate_line_height(12.0);
        let hmm = h * PT_TO_HMM;
        assert!(hmm < 500.0, "12pt单行高度换算应<500(1/100mm)，实际={hmm}");
    }

    #[test]
    fn factor_matches_lok_lcl_get_attrib_height_formula() {
        // 直接核对公式系数本身，防止未来有人误改回*1.2这类"更直观"
        // 但不匹配LOK的写法。见sc/source/core/data/column2.cxx:865。
        let font_size_pt = 100.0_f32; // 用大数值放大浮点误差容忍度
        let expected = font_size_pt * 1.18 - (23.0 / 20.0);
        let actual = estimate_line_height(font_size_pt);
        assert!((actual - expected).abs() < 0.01, "expected={expected}, actual={actual}");
    }
}

#[cfg(test)]
mod wrap_width_coordinate_regression_tests {
    //! 回归测试：确认 wrap_width 与 font_size_pt 处于同一"pt"逻辑坐标系
    //! （font_scale=1.0 时不需要额外乘 PT_TO_PX），且 LineHeight::Absolute
    //! 必须显式设置（否则 Parley 用字体自带默认行高，不是 font_size*1.2
    //! 模型）。这两点是 measure_row_height_wrap 正确性的前提，本次会话
    //! 通过实测踩坑后确认（见 roe-ext_progress_v139/v140 会话记录）。
    //! 保留此测试是为了在未来 Parley 版本升级时，若这两个假设被打破，
    //! 能第一时间发现，而不是等到自动行高数值悄悄算错才排查。
    //!
    //! 附带确认：Parley 默认只在词边界（空格）断行，连续无空格字符不会
    //! 被强制断行——这是符合预期的排版行为（对齐主流引擎"break word"
    //! 默认关闭的惯例），不是bug，测试文本故意用带空格的句子。
    use parley::{FontContext, LayoutContext, StyleProperty, GenericFamily};

    fn line_count_and_height(text: &str, wrap_width_pt: f32) -> (usize, f32) {
        let mut font_context = FontContext::new();
        let mut layout_context: LayoutContext<[u8; 4]> = LayoutContext::new();
        let font_scale = 1.0_f32;
        let font_size_pt = 12.0_f32;
        let line_height_pt = font_size_pt * 1.2;
        let mut builder = layout_context.ranged_builder(&mut font_context, text, font_scale, true);
        // ROOT-082字体统一修复：GenericFamily::SystemUi在这台机器上被
        // fontconfig解析成非Calibri兼容字体（实测选中的字体宽度表与
        // Carlito/Calibri不一致），导致wrap折行行数系统性偏多，与Excel/
        // LibreOffice/OnlyOffice/WPS官方断行结果不一致（tdf123026 E4实测
        // 11行 vs 官方8行）。改用显式"Calibri"（fontconfig会自动替换成
        // metric-compatible的Carlito，实测断行结果与官方逐字节一致）。
        // 本次为最小修复：硬编码，未接入CellFormat.font_family（xlsx读取
        // 路径至今未解析<font><name val=".."/>，是与font_size同源的独立
        // 缺口，docx.rs已有完整w:rFonts解析可抄，留作独立future task，
        // 见roe-ext_progress_v141续 "font_family解析" 记录）。
        builder.push_default(parley::StyleProperty::FontFamily(parley::FontFamily::named("Carlito")));
        builder.push_default(parley::style::LineHeight::Absolute(line_height_pt));
        builder.push_default(StyleProperty::FontSize(font_size_pt));
        let mut layout: parley::Layout<[u8; 4]> = builder.build(text);
        layout.break_all_lines(Some(wrap_width_pt));
        layout.align(parley::Alignment::Start, parley::AlignmentOptions::default());
        let count = layout.lines().count();
        let total_h: f32 = layout.lines().map(|l| l.metrics().line_height).sum();
        (count, total_h)
    }

    #[test]
    fn wrap_width_is_pt_scale_not_px() {
        let text = "hello world this is a fairly long sentence that should wrap";
        // 很宽：全部放得下，1行
        let (n1, h1) = line_count_and_height(text, 500.0);
        assert_eq!(n1, 1, "500pt宽度应该能放下整句，若不是说明坐标系假设被打破");
        assert!((h1 - 14.4).abs() < 0.01, "单行高度应精确等于font_size_pt*1.2=14.4，实际={h1}");

        // 窄一些：应该产生多行，且每行行高仍然是14.4的整数倍
        let (n2, h2) = line_count_and_height(text, 100.0);
        assert!(n2 > 1, "100pt宽度应该触发换行，若不是说明wrap_width单位换算有误");
        assert!((h2 - n2 as f32 * 14.4).abs() < 0.01, "总高度应等于行数*14.4，实际={h2}, 行数={n2}");

        // 更窄：行数应该继续增加
        let (n3, _h3) = line_count_and_height(text, 50.0);
        assert!(n3 > n2, "更窄的宽度应该产生更多行（{n3} 应该 > {n2}）");
    }
}

use crate::tile_geometry::col_width_at;
use crate::tile_render_helpers::cell_value_to_text_fmt;

/// 单行不换行文字宽度测量（供 auto_fit 列宽等"给定文字反推所需宽度"
/// 场景使用，与 measure_row_height_wrap "给定宽度反推能放几行"方向
/// 相反，因此独立成一个函数，不合并到已有测量函数里）。
///
/// 用法与 measure_row_height_wrap / draw_header_text 一致：
/// ranged_builder 构建 layout -> break_all_lines(None)（None 表示不限宽度，
/// 天然产出单行）-> layout.width() 读取该行完整宽度（pt）。
/// font_family 调用方需自行先过 resolve_font_family_for_parley 归一化
/// （与本文件其它测量函数的既有分工一致，本函数不重复做这一步，避免
/// 调用方传入已归一化的名字时被二次转换）。
///
/// 空字符串返回 0.0，不构造 layout（避免无意义的 parley 调用开销）。
pub fn measure_text_width_pt(
    font_context: &mut parley::FontContext,
    layout_context: &mut parley::LayoutContext,
    text: &str,
    font_size_pt: f32,
    font_family: &str,
) -> f32 {
    if text.is_empty() {
        return 0.0;
    }
    let line_height_pt = estimate_line_height(font_size_pt);
    let mut builder = layout_context.ranged_builder(font_context, text, 1.0_f32, true);
    builder.push_default(parley::StyleProperty::FontFamily(parley::FontFamily::named(font_family)));
    builder.push_default(parley::style::LineHeight::Absolute(line_height_pt));
    builder.push_default(parley::StyleProperty::FontSize(font_size_pt));
    let mut layout: parley::Layout<[u8; 4]> = builder.build(text);
    layout.break_all_lines(None);
    layout.align(parley::Alignment::Start, parley::AlignmentOptions::default());
    layout.width()
}


/// 贵路径：该行内所有 wrap_text=true 的单元格，各自用其列宽（或合并区域
/// 总宽度，与主渲染循环 tile.rs 的 avail_w 计算逻辑保持一致，见该文件
/// 第500~562行附近）做真排版，读取行数（line count）乘行距得总高度，
/// 取该行需要的最大总高度。
///
/// 简化范围（本次会话已确认，"先最简，回归主线"）：num_format 只读
/// cell_formats.num_format 静态样式，不处理条件格式(dxf)对num_format的
/// 覆盖（与主渲染循环的完整优先级链有细微差异，但只影响数字显示文本的
/// 具体格式化字符串，对换行行数的影响可忽略，属于可接受的简化）。
/// wrap_width 同样不减水平padding（约3pt级别的视觉留白，已与用户确认
/// 可以忽略，后续如验证发现明显偏差再回来精修）。
///
/// 返回单位为pt（与measure_row_height_cheap一致）。若该行没有任何
/// wrap_text单元格，返回None。
pub fn measure_row_height_wrap(
    sheet: &Sheet,
    row: u32,
    cfg: &CalcRenderConfig,
    font_context: &mut parley::FontContext,
    layout_context: &mut parley::LayoutContext,
) -> Option<f32> {
    let mut max_total_height: Option<f32> = None;
    for (&col_idx, sc) in sheet.cells.cols.iter() {
        for block in &sc.blocks {
            let end = block.start_row + block.values.len() as u32;
            if row < block.start_row || row >= end {
                continue;
            }
            let i = (row - block.start_row) as usize;
            let val = match block.values.get(i) {
                Some(v) if !matches!(v, roe_model::calc::CellValue::Empty) => v,
                _ => continue,
            };
            let cf = sheet
                .cell_formats
                .get(&(row, col_idx))
                .or_else(|| sheet.col_style.get(col_idx as usize).and_then(|o| o.as_ref()));
            let wrap_text = cf.and_then(|f| f.wrap_text).unwrap_or(false);
            if !wrap_text {
                continue;
            }
            // 只处理merge区域左上角单元格（与主渲染循环一致：非左上角
            // 的合并单元格continue跳过，不重复计算/不单独占用行高）。
            let merge = sheet.merged_cells.iter().find(|mc| {
                row >= mc.r1 && row <= mc.r2 && col_idx >= mc.c1 && col_idx <= mc.c2
            });
            if let Some(mc) = merge {
                if row != mc.r1 || col_idx != mc.c1 {
                    continue;
                }
            }
            let num_fmt = cf.and_then(|f| f.num_format.as_deref());
            let text = cell_value_to_text_fmt(val, num_fmt, sheet.date1904);
            if text.is_empty() {
                continue;
            }
            let font_size_pt = cf.and_then(|f| f.font_size).unwrap_or(12.0_f32);
            // font_family解析接入：xlsx读取路径此前未解析<font><name val=".."/>，
            // 已在roe-parser/src/ooxml/xlsx.rs补上。这里优先用CellFormat携带
            // 的真实字体名；无该字段时fallback到"Calibri"（fontconfig环境下
            // 会自动metric-compatible替换为Carlito，实测断行结果与Excel/
            // LibreOffice/OnlyOffice/WPS官方逐字节一致，见本次tdf123026
            // E4验证）。Layout（此处）与Paint（tile_render_helpers.rs）
            // 必须用同一个fallback字符串，否则重新引入"测绘撕裂"。
            let font_family_raw = cf.and_then(|f| f.font_family.as_deref()).unwrap_or("Calibri");
            // 字体名归一化：见本文件resolve_font_family_for_parley注释，
            // "Calibri"这个字符串直接传给Parley会导致部分字形丢失，必须
            // 先转换成"Carlito"。Paint阶段（tile_render_helpers.rs）必须
            // 调用同一个函数，否则重新引入字体选择维度的"测绘撕裂"。
            let font_family = resolve_font_family_for_parley(font_family_raw);
            let line_height_pt = estimate_line_height(font_size_pt);
            // wrap_text场景avail_w：与tile.rs主渲染循环一致，merge优先于
            // wrap_text单列宽——即使wrap_text=true，若单元格属于合并区域，
            // 仍用合并区域总宽度（col_width_at求和），不是单列宽度。
            // Paint阶段对齐修正（roe-ext_progress_v141.md 第二节"第二个同源
            // 不一致点"）：draw_cell_text/draw_cell_rich_text 的 wrap_width_px
            // 实际是 avail_w*font_scale - pad*2.0（pad=2.0*scale，且调用点
            // font_scale与scale传的是同一个值），pt空间下等价于
            // avail_w_pt - 4.0（左右各减2.0pt padding）。此前这里直接用
            // col_width_at的原始列宽，比Paint阶段实际换行宽度更宽，会导致
            // Layout阶段测出的折行数少于Paint阶段实际折行数，是比1.18/1.2
            // 系数不一致更根本的潜在问题（design_v1.md当初"不减水平padding，
            // 可忽略"的判断在真实渲染验证后不成立，见v141交接文档）。
            const CELL_TEXT_PAD_PT: f32 = 2.0;
            let wrap_width_pt = if let Some(mc) = merge {
                (mc.c1..=mc.c2).map(|c| col_width_at(sheet, c, cfg)).sum::<f32>()
            } else {
                col_width_at(sheet, col_idx, cfg)
            } - CELL_TEXT_PAD_PT * 2.0;

            let mut builder = layout_context.ranged_builder(font_context, &text, 1.0_f32, true);
            builder.push_default(parley::StyleProperty::FontFamily(parley::FontFamily::named(font_family)));
            builder.push_default(parley::style::LineHeight::Absolute(line_height_pt));
            builder.push_default(parley::StyleProperty::FontSize(font_size_pt));
            let mut layout: parley::Layout<[u8; 4]> = builder.build(&text);
            layout.break_all_lines(Some(wrap_width_pt.max(1.0)));
            layout.align(parley::Alignment::Start, parley::AlignmentOptions::default());
            let total_h: f32 = layout.lines().map(|l| l.metrics().line_height).sum();

            max_total_height = Some(max_total_height.map_or(total_h, |m: f32| m.max(total_h)));
        }
    }
    max_total_height
}

use roe_model::calc::RowHeight;

/// 单行分流写回：判断该行是否需要测量（Auto且is_dirty=true，或索引
/// 超出row_heights长度的隐式Auto默认行），按row_has_wrap_text判断走
/// 廉价路径还是贵路径，写回结果并置is_dirty=false。
///
/// 提炼自原ensure_row_heights_measured函数体（ROOT-082第五步：C-ABI
/// Layout阶段改为"贪吃蛇式动态推进"后，range遍历版本与新的单行动态
/// 推进入口都需要复用这段"判断→分流→写回"逻辑，避免重复实现，见
/// roe-ext_progress_v140.md 3.2节）。
///
/// Manual行、以及is_dirty=false的Auto行跳过，不重新测量（懒加载核心
/// 语义：只测量真正需要的行，见optimalRowHeight_design_v1.md三、3节
/// "视口驱动、懒加载"）。
///
/// 若两种路径都测不出结果（廉价路径返回None即无非空单元格、贵路径也
/// 返回None即无wrap_text单元格），说明该行事实上是空行或只有空单元格
/// 的wrap_text单元格，不写回任何值（维持原Auto{current: default估算值,
/// is_dirty:true}状态不变——注意：这里选择不置is_dirty=false，因为
/// "测不出结果"和"测量完成且结果是0"是两种不同状态，保留is_dirty=true
/// 使得下次调用仍会重试，避免空行内容后续被填充后无法感知需要重新测量
/// ——不过本次范围内没有实现编辑触发的重新invalidate，这个区别目前只有
/// "下次视口再次覆盖到这一行时会重试"这一个实际效果）。
pub fn ensure_row_height_measured_single(
    sheet: &mut Sheet,
    row: u32,
    cfg: &CalcRenderConfig,
    font_context: &mut parley::FontContext,
    layout_context: &mut parley::LayoutContext,
) {
    let needs_measure = match sheet.row_heights.get(row as usize) {
        Some(RowHeight::Auto { is_dirty: true, .. }) => true,
        None => true,
        _ => false,
    };
    if !needs_measure {
        return;
    }
    let measured = if row_has_wrap_text(sheet, row) {
        measure_row_height_wrap(sheet, row, cfg, font_context, layout_context)
    } else {
        measure_row_height_cheap(sheet, row, cfg)
    };
    let h = match measured {
        Some(h) => h,
        None => return,
    };
    // 自动测量值不应低于默认行高(LOK 的纯字高公式不含单元格内边距,
    // 12pt/11pt 的结果会比 Excel 默认 15pt 矮,造成同表两种行高)。
    let floor = sheet.effective_default_row_height();
    let h = h.max(floor);
    if sheet.row_heights.len() <= row as usize {
        sheet.row_heights.resize(row as usize, RowHeight::default());
        sheet.row_heights.push(RowHeight::Auto { current: h, is_dirty: false });
    } else {
        sheet.row_heights[row as usize] = RowHeight::Auto { current: h, is_dirty: false };
    }
}

/// 分流入口（range版本）：遍历 [row_start, row_end) 范围内所有行，对每行
/// 调用ensure_row_height_measured_single。保留作为"已知确定范围"场景的
/// 简单包装（见roe-ext_progress_v140.md 3.2节），C-ABI Layout阶段改为
/// 调用下方layout_measure_viewport（贪吃蛇式动态推进），不直接调用本函数。
pub fn ensure_row_heights_measured(
    sheet: &mut Sheet,
    row_start: u32,
    row_end: u32,
    cfg: &CalcRenderConfig,
    font_context: &mut parley::FontContext,
    layout_context: &mut parley::LayoutContext,
) {
    for row in row_start..row_end {
        ensure_row_height_measured_single(sheet, row, cfg, font_context, layout_context);
    }
}

/// 该sheet的"总行数"上限：扫描所有列的所有CellBlock，取
/// `start_row + values.len()` 的最大值。没有现成的行数字段/dimension
/// 元数据可用（Sheet结构体未记录，SparseGrid是稀疏存储），故按实际
/// 数据范围推算。空sheet返回0。
///
/// 仅供layout_measure_viewport的贪吃蛇循环终止条件使用（防止在空白
/// 尾部区域继续空转推进），语义是"最后一个有数据的行的下一行"，不是
/// Excel的1048576行硬限制。
pub fn total_rows_of(sheet: &Sheet) -> u32 {
    let mut max_row = 0u32;
    for sc in sheet.cells.cols.values() {
        for block in &sc.blocks {
            let end = block.start_row + block.values.len() as u32;
            if end > max_row {
                max_row = end;
            }
        }
    }
    max_row
}

/// C-ABI Layout阶段入口："动态贪吃蛇式推进" + Overscan缓冲区。
/// 取代design_v1.md原方案（静态算row_end后交给Paint），解决时序bug：
/// row_at_y内部读取RowHeight::current()，若Layout阶段先用测量前的
/// 估算值静态算出(row_start,row_end)，测量后同一公式再算一次row_end
/// 可能得到不同结果，导致Paint阶段实际画到的行数与视口真实可容纳行数
/// 不一致（视口底部留白或被截断）。
///
/// 方案（用户与Gemini两轮讨论定稿，见roe-ext_progress_v140.md 3.1节，
/// 4个实现细节均已确认）：
/// 1. row_start起点继续用现有row_at_y静态查（起点用估算值定没有问题，
///    "贪吃蛇"只负责解决终点的填满视口问题）。
/// 2. current_y从row_y(sheet, row_start, cfg)开始（累加0..row_start
///    已知高度得到row_start的绝对Y坐标，含尚未测量的估算值）。
/// 3. 动态推进：每前进一行就地测量该行真实高度，用真实值累加current_y，
///    直到超过视口底部绝对坐标(scroll_y_pt + data_logical_h)。
/// 4. Overscan：视口填满后强制再多测量OVERSCAN_ROWS行，应对滚动惯性/
///    浮点精度截断/1px级别错位。
///
/// Paint阶段（render_viewport_into）完全不变：继续用现有row_at_y/
/// row_end静态查询逻辑，此时读到的都是Layout阶段已经"铺平"的真实值。
thread_local! {
    /// C-ABI Layout阶段（layout_measure_viewport）专用的parley上下文缓存。
    /// 与CalcTileRenderer自带的font_context/layout_context字段是两套独立
    /// 实例——阶段一(Layout)不构造CalcTileRenderer（只需要&mut Sheet，
    /// CalcTileRenderer.workbook是&Workbook不可变引用，两阶段必须分开
    /// borrow，见roe-ext_progress_v140.md 3.1节问题描述）。
    ///
    /// 用thread_local而非每次调用layout_measure_viewport时新建，是因为
    /// parley::FontContext::new()涉及系统字体列表解析等较重初始化开销，
    /// 若在高频滚动场景下（每次视口滚动都触发一次Layout）每次都重新
    /// 构造，会造成可感知的滚动卡顿。C-ABI调用约定是单线程调用模型
    /// （所有roe_*函数都在同一调用线程执行，见handle.rs HANDLES的
    /// thread_local存储方式），thread_local缓存与现有并发模型一致。
    ///
    /// LayoutContext的泛型参数用默认值[u8;4]（parley::LayoutContext<B:
    /// Brush = [u8;4]>的声明默认值，与measure_row_height_wrap/回归测试
    /// 里显式写的LayoutContext<[u8;4]>是同一类型，不是不同类型的两套
    /// 缓存——已核实vendor/parley/parley/src/context.rs确认默认值）。
    static LAYOUT_FONT_CTX: std::cell::RefCell<parley::FontContext> =
        std::cell::RefCell::new(parley::FontContext::new());
    static LAYOUT_LAYOUT_CTX: std::cell::RefCell<parley::LayoutContext> =
        std::cell::RefCell::new(parley::LayoutContext::new());
}

pub fn layout_measure_viewport(
    sheet: &mut Sheet,
    cfg: &CalcRenderConfig,
    scroll_y_pt: f32,
    data_logical_h: f32,
) {
    use crate::tile_geometry::{row_at_y, row_y};

    /// Overscan缓冲区行数：视口填满后额外多测量的行数，写死不做配置项
    /// （见roe-ext_progress_v140.md 3.1节"已确认的4个实现细节"之3）。
    const OVERSCAN_ROWS: u32 = 10;

    let total_rows = total_rows_of(sheet);
    if total_rows == 0 {
        return;
    }

    let row_start = row_at_y(sheet, scroll_y_pt, cfg);
    let mut current_y = row_y(sheet, row_start, cfg);
    let mut current_row = row_start;
    let view_bottom = scroll_y_pt + data_logical_h;

    LAYOUT_FONT_CTX.with_borrow_mut(|font_context| {
        LAYOUT_LAYOUT_CTX.with_borrow_mut(|layout_context| {
            while current_y < view_bottom && current_row < total_rows {
                ensure_row_height_measured_single(sheet, current_row, cfg, font_context, layout_context);
                let real_height = crate::tile_geometry::row_height_at(sheet, current_row, cfg);
                current_y += real_height;
                current_row += 1;
            }

            let buffer_end = (current_row + OVERSCAN_ROWS).min(total_rows);
            for r in current_row..buffer_end {
                ensure_row_height_measured_single(sheet, r, cfg, font_context, layout_context);
            }
        });
    });
}

#[cfg(test)]
mod font_enum_probe {
    use super::*;

    /// 探针:打印系统全部字体族名并标出白名单放行项。
    /// 运行: cargo test -p roe-core probe_print_font_names -- --ignored --nocapture
    #[test]
    #[ignore]
    fn probe_print_font_names() {
        let mut ctx = parley::FontContext::new();
        let all: Vec<String> = ctx.collection.family_names().map(|s| s.to_string()).collect();
        println!("ALL({}):", all.len());
        for n in &all {
            println!("  {}{}", n, if is_font_allowed(n) { "  [ALLOWED]" } else { "" });
        }
        println!("LIST: {:?}", list_available_fonts());
    }
}
