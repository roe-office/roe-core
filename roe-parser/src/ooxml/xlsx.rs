//! XLSX (OOXML SpreadsheetML) → roe-model::calc::Workbook
//!
//! OPC 流程：
//!   ZIP → xl/workbook.xml → xl/worksheets/sheetN.xml
//!         xl/sharedStrings.xml → CellValue::Text
//!         xl/styles.xml        → (暂存 styleIndex，本版本忽略样式)

use std::collections::HashMap;
use std::io::Read;

use quick_xml::events::Event;
use quick_xml::Reader;
use zip::ZipArchive;

use crate::ParseError;
use crate::ooxml::sniff_image_mime;
use crate::ooxml::chart_common::{parse_ooxml_chart_xml, parse_ooxml_theme_colors};
use roe_model::common::Color;
use roe_model::calc::{CellBlock, CellError, CellValue, FormulaDialectSource, Sheet, SparseColumn, SparseGrid, TextRun, Workbook};

// ─── 公开入口 ────────────────────────────────────────────────────────────────

pub struct XlsxParser;

impl XlsxParser {
    /// 解析 xlsx 字节流 → Workbook 内存模型
    pub fn parse(data: &[u8]) -> Result<Workbook, ParseError> {
        let cursor = std::io::Cursor::new(data);
        let mut zip = ZipArchive::new(cursor)
            .map_err(|e| ParseError::Malformed(format!("zip open failed: {e}")))?;

        // 1. SharedStrings（可能不存在）
        let sst = read_shared_strings(&mut zip)?;

        // ROOT-022/025修复(v133)：读取xl/theme/theme1.xml主题色，
        // 供dataBar/colorScale条件格式的theme+tint颜色解析使用。
        // ROOT CalcThemeTest修复：提前到read_styles之前读取，因为普通单元格
        // 样式（cellXfs的字体色/填充色）的theme+tint解析也需要这份主题色表，
        // 此前只在dataBar/colorScale这一条路径接了theme_colors，read_styles
        // 完全没拿到，导致<color theme="N"/>在普通单元格样式场景下静默丢失。
        let theme_colors = read_xlsx_theme_colors(&mut zip);

        // 1b. styles.xml → xf格式 + dxf格式
        let (style_formats, dxf_formats) = read_styles(&mut zip, &theme_colors)?;

        // 2. workbook.xml → sheet 列表（name, rId, sheetId）
        let sheet_refs = read_workbook_sheets(&mut zip)?;

        // 2b. ROOT-041: workbook.xml → date1904 标记（Mac Excel 1904日期系统）
        let date1904 = read_date1904_flag(&mut zip);

        // 3. workbook.xml.rels → rId → target 路径映射
        let rels = read_workbook_rels(&mut zip)?;

        // 3b. workbook.xml → definedNames（命名区域）
        let (named_ranges, named_formulas) = read_defined_names(&mut zip)?;

        // 4. 逐张 worksheet 解析
        let mut sheets = Vec::with_capacity(sheet_refs.len());
        let mut pivot_pending: Vec<(usize, Option<String>, crate::ooxml::pivot::PivotConfigExtract, Option<String>, Option<String>, Option<String>)> = Vec::new();
        // chartEx 统计图:循环内只收集,全部 sheet 读完后再回填数据(支持跨 sheet 引用)
        let mut stat_pending: Vec<(usize, String, i64, i64, i64, i64)> = Vec::new();
        for sr in &sheet_refs {
            let target = rels.get(&sr.r_id).ok_or_else(|| {
                ParseError::Malformed(format!("no rel for sheet rId={}", sr.r_id))
            })?;
            // target 可能是 "worksheets/sheet1.xml"，拼成 "xl/worksheets/sheet1.xml"
            let path = normalize_sheet_path(target);
            let mut sheet = read_worksheet(&mut zip, &path, &sr.name, &sst, &style_formats, &dxf_formats, &theme_colors)?;
            // ROOT-041: 广播workbook级别的date1904标记到每个sheet
            // （同一workbook内所有sheet共享同一个日期系统基准）。
            sheet.date1904 = date1904;
            // 迷你图（sparkline）解析：独立二次读取同一 sheet XML，
            // 不复用 read_worksheet 的流式状态机（extLst 结构嵌套较深，
            // 隔离出去风险更低）。读取失败或无 extLst 时静默留空 vec。
            if let Ok(sheet_xml) = read_zip_entry(&mut zip, &path) {
                sheet.sparkline_groups = parse_sparkline_groups_from_xml(&sheet_xml);
                // ROOT-009问题B：extLst里的x14扩展条件格式，合并进标准解析
                // 流程已产出的cond_formats列表。
                // ROOT-066(v128)：IconSet场景legacy/x14互斥（不会同range重复
                // 出现），直接追加没问题；但DataBar场景Excel会同时写legacy
                // <dataBar>(占位min=0/max=0废值，供旧版本兜底显示)和x14扩展
                // (真实阈值+negativeFillColor+axisColor)到同一个range，这是
                // Excel标准的向后兼容写法。若直接extend会导致同一个range画
                // 两条重叠的数据条（一条用废值、一条用真实值）。
                // 处理方式：x14返回的DataBar规则本身bar_color等字段是占位值
                // （x14结构里不含正色，正色只在legacy<color>里），所以对每条
                // x14 DataBar规则，找同range的legacy DataBar规则，把正色/
                // 边框/方向/bar_only/solid_fill从legacy继承过来，然后删除
                // 该legacy规则、插入合并后的新规则；找不到对应legacy规则
                // 时（理论上不应发生）就直接用x14规则本身兜底。
                let x14_rules = parse_x14_cond_formats_from_xml(&sheet_xml, &theme_colors);
                for x14_rule in x14_rules {
                    if let roe_model::calc::ConditionalFormatKind::DataBar {
                        min, max, negative_fill_color, axis_color, axis_position, solid_fill,
                        border_color: x14_border, direction: x14_dir, bar_only: x14_bar_only, ..
                    } = &x14_rule.kind {
                        let legacy_idx = sheet.cond_formats.iter().position(|r| {
                            r.range == x14_rule.range
                                && matches!(r.kind, roe_model::calc::ConditionalFormatKind::DataBar { .. })
                        });
                        if let Some(idx) = legacy_idx {
                            if let roe_model::calc::ConditionalFormatKind::DataBar {
                                bar_color, border_color, direction, bar_only, ..
                            } = sheet.cond_formats[idx].kind.clone() {
                                let border_color = x14_border.clone().or(border_color);
                                let direction = if matches!(x14_dir, roe_model::calc::BarDirection::Auto) { direction } else { x14_dir.clone() };
                                let bar_only = *x14_bar_only || bar_only;
                                // ROOT-066(v128) 第五轮修正：solid_fill此前错误地从legacy
                                // 继承（legacy dataBar根本没有gradient概念，只能是写死的
                                // 占位默认值），改为从x14继承——gradient="0"/"1"属性只存在
                                // 于x14:dataBar标签，是solid_fill唯一的真实数据来源
                                // （condformat_theme_color等用例回归：显式设置gradient="0"
                                // 的实心条被错误渲染成了渐变条）。
                                sheet.cond_formats[idx].kind = roe_model::calc::ConditionalFormatKind::DataBar {
                                    min: min.clone(),
                                    max: max.clone(),
                                    bar_color,
                                    border_color,
                                    direction,
                                    bar_only,
                                    solid_fill: *solid_fill,
                                    negative_fill_color: negative_fill_color.clone(),
                                    axis_color: axis_color.clone(),
                                    axis_position: axis_position.clone(),
                                };
                                continue;
                            }
                        }
                        // 没找到对应legacy规则，兜底直接追加x14规则本身
                        sheet.cond_formats.push(x14_rule);
                    } else {
                        // 非DataBar（如IconSet）沿用原有"直接追加"逻辑
                        sheet.cond_formats.push(x14_rule);
                    }
                }
                // ROOT-005修复(v126)：解析<autoFilter>标签，此前sheet.auto_filter
                // 恒为None，导致筛选下拉按钮UI完全不渲染。
                sheet.auto_filter = parse_autofilter_from_xml(&sheet_xml);
            }
            // drawing → chart 解析
            let sheet_rels_path = path.replace("worksheets/", "worksheets/_rels/").replace(".xml", ".xml.rels");
            if let Ok(rels_xml) = read_zip_entry(&mut zip, &sheet_rels_path) {
                let sheet_rel_map = parse_simple_rels(&rels_xml);
                for (_rid, target) in &sheet_rel_map {
                    if !target.contains("drawing") { continue; }
                    let drawing_path = normalize_sheet_path(target);
                    let drawing_xml = match read_zip_entry(&mut zip, &drawing_path) {
                        Ok(x) => x,
                        Err(_) => continue,
                    };
                    let drawing_rels_path = drawing_path.replace("drawings/", "drawings/_rels/").replace(".xml", ".xml.rels");
                    let drawing_rels_xml = read_zip_entry(&mut zip, &drawing_rels_path).unwrap_or_default();
                    let drawing_rel_map = parse_simple_rels(&drawing_rels_xml);
                    let chart_entries = extract_chart_rids_from_drawing(&drawing_xml);
                    for (chart_rid, x_emu, y_emu, cx_emu, cy_emu) in chart_entries {
                        let chart_target = match drawing_rel_map.get(&chart_rid) {
                            Some(t) => t.clone(),
                            None => continue,
                        };
                        let chart_path = normalize_sheet_path(&chart_target);
                        let chart_xml = match read_zip_entry(&mut zip, &chart_path) {
                            Ok(x) => x,
                            Err(_) => continue,
                        };
                        if chart_xml.contains("<cx:chartSpace") {
                            stat_pending.push((sheets.len(), chart_xml, x_emu, y_emu, cx_emu, cy_emu));
                            continue;
                        }
                        if let Some(mut model) = parse_ooxml_chart_xml(&chart_xml, &theme_colors, &HashMap::new()) {
                            // EMU → pt（1 pt = 12700 EMU），参照图片坐标换算的既有模式
                            model.x_pt = x_emu as f32 / 12700.0;
                            model.y_pt = y_emu as f32 / 12700.0;
                            if cx_emu > 0 {
                                model.width_pt = cx_emu as f32 / 12700.0;
                            }
                            if cy_emu > 0 {
                                model.height_pt = cy_emu as f32 / 12700.0;
                            }
                            // 导入图表 id 补发:同 sheet 内 max+1,0 为哨兵值(与 insert_chart 一致)
                            model.id = sheet.floating_charts.iter().map(|c| c.id)
                                .chain(sheet.floating_stat_charts.iter().map(|c| c.id))
                                .max().unwrap_or(0) + 1;
                            sheet.floating_charts.push(model);
                        }
                    }
                    let drawing_xml_clean = strip_chartex_fallback(&drawing_xml);
                    collect_xlsx_images(&mut zip, &drawing_xml_clean, &drawing_rel_map, &mut sheet);
                    collect_xlsx_shapes(&drawing_xml_clean, &mut sheet);
                }
            }
            // v156：B路径接线 —— 检测 sheet rels 里的 pivotTable 引用，
            // 提取透视表"配置"（字段轴分类+聚合方式），不在此处构建
            // row_tree/col_tree（那部分交给下游 pivot_sheet_into_workbook_v2
            // 现场重算，见v153 3.3节 expand_to_tree bug 定位、v154架构决策）。
            // sheet.pivot 此时仍为 None，留给下游用 PivotMeta 填充。
            if let Ok(rels_xml) = read_zip_entry(&mut zip, &sheet_rels_path) {
                let sheet_rel_map = parse_simple_rels(&rels_xml);
                for (_rid, target) in &sheet_rel_map {
                    if !target.contains("pivotTable") { continue; }
                    let pivot_path = normalize_sheet_path(target);
                    let pivot_xml = match read_zip_entry(&mut zip, &pivot_path) {
                        Ok(x) => x, Err(_) => continue,
                    };
                    let pivot_rels_path = pivot_path
                        .replace("pivotTables/", "pivotTables/_rels/")
                        .replace(".xml", ".xml.rels");
                    let pivot_rels_xml = read_zip_entry(&mut zip, &pivot_rels_path).unwrap_or_default();
                    let pivot_rel_map = parse_simple_rels(&pivot_rels_xml);
                    let cache_xml = pivot_rel_map.values()
                        .find(|t| t.contains("pivotCache"))
                        .and_then(|t| {
                            let p = t.replace("../", "xl/");
                            read_zip_entry(&mut zip, &p).ok()
                        })
                        .unwrap_or_default();
                    let (field_names, _cache_fields, data_source_sheet, data_source_ref) = crate::ooxml::pivot::parse_cache_definition_full(&cache_xml);
                    let (pt_name, pt_style) = crate::ooxml::pivot::parse_pivot_table_info(&pivot_xml);
                    if let Some(cfg_extract) = crate::ooxml::pivot::extract_pivot_config(&pivot_xml, &field_names) {
                        pivot_pending.push((sheets.len(), data_source_sheet, cfg_extract, data_source_ref, pt_name, pt_style));
                    }
                    break; // 每个 sheet 只处理第一个 pivotTable
                }
            }
            // ROOT-011：table1.xml 解析，通过 sheet rels 找到该 sheet 关联的 table 定义
            if let Ok(rels_xml) = read_zip_entry(&mut zip, &sheet_rels_path) {
                let sheet_rel_map = parse_simple_rels(&rels_xml);
                for (_rid, target) in &sheet_rel_map {
                    if !target.contains("/table") && !target.contains("tables/") { continue; }
                    let table_path = normalize_sheet_path(target);
                    let table_xml = match read_zip_entry(&mut zip, &table_path) {
                        Ok(x) => x, Err(_) => continue,
                    };
                    if let Some(mut table) = parse_table_xml(&table_xml) {
                        table.theme_palette = build_theme_palette(&theme_colors);
                        sheet.tables.push(table);
                    }
                }
            }
            sheets.push(sheet);
        }


        // chartEx 统计图后处理:回填 definedName 数据 → 解析 → 发 id → 入 floating_stat_charts
        for (si, chart_xml, x_emu, y_emu, cx_emu, cy_emu) in stat_pending {
            let filled = {
                let get = |sn: &str, r: u32, c: u32| -> CellValue {
                    sheets.iter().find(|s| s.name == sn)
                        .map(|s| s.cells.get(r, c))
                        .unwrap_or(CellValue::Empty)
                };
                inject_chartex_cache(&chart_xml, &named_ranges, &get)
            };
            let Some(model) = crate::ooxml::chartex::parse_chartex_xml(&filled) else { continue };
            let sh = &mut sheets[si];
            let next_id = sh.floating_charts.iter().map(|c| c.id)
                .chain(sh.floating_stat_charts.iter().map(|c| c.id))
                .max().unwrap_or(0) + 1;
            sh.floating_stat_charts.push(roe_model::calc::FloatingStatChart {
                id: next_id,
                x_pt: x_emu as f32 / 12700.0,
                y_pt: y_emu as f32 / 12700.0,
                width_pt: cx_emu as f32 / 12700.0,
                height_pt: cy_emu as f32 / 12700.0,
                model,
            });
        }

        let mut wb = Workbook {
            sheets,
            named_ranges,
            named_formulas,
            styles: Default::default(),
            // ROOT-019: xlsx是OOXML，公式本就是Excel方言，无需转换。
            formula_dialect_source: FormulaDialectSource::Excel,
        };

        // v156：B路径接线 —— 消费 pivot_pending，调用
        // pivot_sheet_into_workbook_v2（B路径）现场重算出正确的
        // PivotMeta（含小计，不依赖有bug的 expand_to_tree，见v153 3.3节
        // 定位、v154架构决策整体禁用A路径），原地挂到目标sheet上，替代
        // 原来"打开时读xlsx静态快照"的做法。
        //
        // v155诊断确认的关键修复：Excel/WPS保存透视表时，会把当前展开
        // 状态的完整视觉内容作为普通单元格值写入<sheetData>（供不支持
        // 透视表交互的软件也能看到内容），这份静态快照与xlsx格式本身
        // 的pivotTable.xml结构定义相互独立、永远共存。若只挂PivotMeta
        // 而不清空这批静态单元格，渲染时两者同屏叠加，产生文字重叠/
        // 小数点残留/字号不一致等视觉异常（v155根因定位，非渲染逻辑
        // bug）。清空范围必须用 location ref 的绝对坐标（cfg中
        // 未提供——此处用parse_pivot_table重新解析一次location用于
        // 清空定界，与B路径数值重算完全独立、互不影响），不能用
        // pivot_sheet_into_workbook_v2内部算出的相对坐标（那是相对
        // 透视表自身、不含base_row/base_col偏移的值，用错会导致
        // 清空范围计算错误）。
        //
        // 已知限制（当前样本均为单透视表场景，故先只处理第一个）：
        // pivot_sheet_into_workbook_v2 是"追加新sheet"语义，取走其
        // PivotMeta 后 remove 掉这个临时sheet 会导致 sheets 后续索引
        // 整体前移。若 pivot_pending 有多个待处理条目，处理第一个之后，
        // 后面条目记录的 sheet_idx/source_idx 会因索引前移而失效
        // （轻则挂错sheet，重则越界panic）。因此这里明确只处理
        // pivot_pending 的第一个条目，其余直接跳过（不尝试猜测修正
        // 索引），避免产生更隐蔽的数据错误。多透视表场景的支持留待
        // 后续升级（需要重新设计索引管理，比如逆序处理或改用稳定
        // 标识而非位置索引）。
        if let Some((pivot_sheet_idx, data_source_sheet_name, cfg_extract, pv_source_ref, pv_name, pv_style)) = pivot_pending.into_iter().next() {
            if let Some(source_name) = data_source_sheet_name {
                if let Some(source_idx) = wb.sheets.iter().position(|s| s.name == source_name) {
                    // xlsx dataField subtotal 命名 -> ROE 内部聚合命名映射
                    // （沿用 ROOT-057 既有映射表）。项目当前所有值字段实际
                    // 都是 "sum"，此映射为将来支持其他聚合方式预留。
                    let map_agg = |agg: &str| -> String {
                        match agg {
                            "average" => "mean",
                            "countNums" => "count",
                            a @ ("sum" | "mean" | "count" | "min" | "max") => a,
                            _ => "sum",
                        }.to_string()
                    };
                    // PivotConfig.agg 是整个透视表单一聚合方式
                    // （pivot-engine 当前设计限制，见roe-ext_progress_v154.md
                    // 技术债记录），取第一个值字段的agg；项目当前所有值
                    // 字段均为 "sum"，不存在多值字段agg不同导致信息丢失
                    // 的实际场景。
                    let agg = cfg_extract.value_fields.first()
                        .map(|(_, a)| map_agg(a))
                        .unwrap_or_else(|| "sum".to_string());
                    let values: Vec<String> = cfg_extract.value_fields.iter()
                        .map(|(col, _)| col.clone())
                        .collect();
                    let cfg = roe_polars_bridge::PivotConfig {
                        rows: cfg_extract.row_fields,
                        cols: cfg_extract.col_fields,
                        values,
                        agg,
                        filters: cfg_extract.filter_fields.iter().map(|f| (f.clone(), Vec::new())).collect(),
                    };
                    if let Ok(new_idx) = roe_polars_bridge::pivot_sheet_into_workbook_v2(
                        &mut wb, source_idx, &cfg, "__pivot_pathB_tmp__".to_string(),
                    ) {
                        let mut taken_pivot = wb.sheets[new_idx].pivot.take();
                        if let Some(m) = taken_pivot.as_mut() {
                            m.source_ref = pv_source_ref.clone();
                            m.name = pv_name.clone();
                            m.style_name = pv_style.clone();
                            let order = cfg_extract.filter_fields.clone();
                            m.filter_fields.sort_by_key(|f| order.iter().position(|n| n == &f.name).unwrap_or(usize::MAX));
                        }
                        if let Some(target) = wb.sheets.get_mut(pivot_sheet_idx) {
                            target.pivot = taken_pivot;
                        }
                        wb.sheets.remove(new_idx);
                    }
                }
            }

            // v156修正：原方案（只清空PivotMeta.base_row..last_row矩形）实测
            // 未覆盖xlsx原始静态快照的真实范围——真机验证发现快照包含总计行
            // 等内容，物理范围比B路径重算出的PivotMeta范围更大，导致部分
            // 残留内容仍与动态渲染重叠（v156会话真机复现：第13/14/16/18/19
            // 物理行仍有残留文字/孤立小数点）。
            //
            // 修正为清空该sheet的整个已用区域（used_bounds），而非局限于
            // PivotMeta认定的矩形。理由：这个sheet的定位就是"透视表结果
            // 展示区"，重算完成后不应保留xlsx原始文件里任何手写的静态
            // 内容——不管那份静态快照实际范围多大，用已用区域兜底可以
            // 保证不会有遗漏（已知局限：如果这个sheet除了透视表快照还有
            // 其他人工计算内容或透视图表，会被一并清空，见下方记录）。
            if let Some(target) = wb.sheets.get_mut(pivot_sheet_idx) {
                if let Some((max_row, max_col)) = target.cells.used_bounds() {
                    for r in 0..=max_row {
                        for c in 0..=max_col {
                            target.cells.set(r, c, roe_model::calc::CellValue::Empty);
                        }
                    }
                }
                // 清空静态快照后,把重算出的透视结果物化进 cells(公式栏/get_cell/AI 可读)
                roe_model::pivot_layout::materialize_pivot(target);
            }
        }

        Ok(wb)
    }
}

// ─── SharedStrings ───────────────────────────────────────────────────────────

/// 返回 SST 数组（索引即位置），如果文件不存在返回空 Vec。
/// ROOT-078：元素类型从纯String改为CellValue——`<si>`内只有0或1个`<r>`
/// （或没有`<r>`，直接`<t>`）时产出`CellValue::Text`，行为与改动前
/// 完全一致；`<si>`内有≥2个`<r>`时产出`CellValue::RichText`，保留每个
/// run的文本+`<rPr><strike/>`格式。本次只解析`strike`一个run级属性，
/// 对齐实测用例`strike-through.xlsx`的实际需求（bold/italic/underline
/// 留空/false占位，非本次范围）。
fn read_shared_strings<R: Read + std::io::Seek>(
    zip: &mut ZipArchive<R>,
) -> Result<Vec<CellValue>, ParseError> {
    let xml = match read_zip_entry(zip, "xl/sharedStrings.xml") {
        Ok(s) => s,
        Err(_) => return Ok(Vec::new()), // 无 SST 文件是合法的
    };

    // ROOT-078验证过程中新发现：trim_text(true)是quick_xml的全局配置，
    // 会无差别去掉每个文本节点前后的空白，不理解OOXML的xml:space="preserve"
    // 属性语义（该属性正是用来告诉解析器"这段文字的首尾空白不能被裁剪"）。
    // 此前所有sharedStrings用例都没有恰好依赖首尾空格保留的场景，直到本次
    // strike-through.xlsx的第二个run文本"  this not"（前导两个空格，
    // 标了xml:space="preserve"）才第一次暴露。改为trim_text(false)，
    // 交由下面的事件处理逻辑自行决定是否需要trim——本函数当前的处理逻辑
    // 本身没有对<t>内容做任何二次trim，所以关掉全局trim后<t>内的空白会被
    // 完整保留；标签之间的格式化换行/缩进空白会作为独立的Event::Text吐出，
    // 但这些事件只在current_run_text.is_some()（正在<t>内部）时才被累加，
    // 不会误吃到run文本里，故此改动风险可控。
    let mut reader = Reader::from_str(&xml);
    reader.config_mut().trim_text(false);

    let mut buf = Vec::new();
    let mut sst: Vec<CellValue> = Vec::new();
    let mut in_si = false;
    // 当前<si>内已收集完的run列表（每个run是文本+strike标记）
    let mut current_runs: Vec<TextRun> = Vec::new();
    // 当前正在收集文本的run（None表示当前不在任何<r>内部，比如<si>
    // 直接套<t>没有<r>包裹的老式写法，或者根本还没进<si>）
    let mut current_run_text: Option<String> = None;
    let mut current_run_strike = false;
    let mut in_r = false;
    let mut in_rpr = false;

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Start(e) | Event::Empty(e) => match e.name().as_ref() {
                b"si" => {
                    in_si = true;
                    current_runs.clear();
                    current_run_text = None;
                    current_run_strike = false;
                }
                b"r" if in_si => {
                    in_r = true;
                    current_run_text = Some(String::new());
                    current_run_strike = false;
                }
                b"rPr" if in_r => {
                    in_rpr = true;
                }
                b"strike" if in_rpr => {
                    current_run_strike = true;
                }
                // 没有<r>包裹、<si>直接套<t>的老式写法：也当作一个匿名run
                // 收集文本，行为与"只有一个普通run"完全等价。
                b"t" if in_si && !in_r && current_run_text.is_none() => {
                    current_run_text = Some(String::new());
                }
                _ => {}
            },
            Event::End(e) => match e.name().as_ref() {
                b"rPr" => {
                    in_rpr = false;
                }
                b"r" => {
                    if let Some(s) = current_run_text.take() {
                        current_runs.push(TextRun {
                            text: s,
                            bold: false,
                            italic: false,
                            underline: false,
                            strikethrough: current_run_strike,
                        });
                    }
                    in_r = false;
                    current_run_strike = false;
                }
                b"t" if !in_r => {
                    // 老式无<r>包裹场景：<t>结束即收尾这个匿名run
                    if let Some(s) = current_run_text.take() {
                        current_runs.push(TextRun {
                            text: s,
                            bold: false,
                            italic: false,
                            underline: false,
                            strikethrough: false,
                        });
                    }
                }
                b"si" => {
                    // 0个run：空字符串；1个run：退化为普通Text（不产生
                    // RichText，保持绝大多数单元格走原有轻量路径）；
                    // ≥2个run：真正的富文本，产出RichText。
                    let value = match current_runs.len() {
                        0 => CellValue::Text(String::new()),
                        1 => CellValue::Text(current_runs.remove(0).text),
                        _ => CellValue::RichText(std::mem::take(&mut current_runs)),
                    };
                    sst.push(value);
                    in_si = false;
                }
                _ => {}
            },
            Event::Text(e) if current_run_text.is_some() => {
                if let Some(ref mut s) = current_run_text {
                    s.push_str(&e.unescape()?);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(sst)
}

// ─── workbook.xml ────────────────────────────────────────────────────────────

struct SheetRef {
    name: String,
    r_id: String,
}

fn read_workbook_sheets<R: Read + std::io::Seek>(
    zip: &mut ZipArchive<R>,
) -> Result<Vec<SheetRef>, ParseError> {
    let xml = read_zip_entry(zip, "xl/workbook.xml")?;
    let mut reader = Reader::from_str(&xml);
    reader.config_mut().trim_text(true);

    let mut buf = Vec::new();
    let mut sheets = Vec::new();
    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Empty(e) | Event::Start(e) if e.name().as_ref() == b"sheet" => {
                let mut name = String::new();
                let mut r_id = String::new();
                for attr in e.attributes().flatten() {
                    match attr.key.as_ref() {
                        b"name" => name = attr.unescape_value()?.into_owned(),
                        b"r:id" => r_id = attr.unescape_value()?.into_owned(),
                        _ => {}
                    }
                }
                if !name.is_empty() && !r_id.is_empty() {
                    sheets.push(SheetRef { name, r_id });
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(sheets)
}

/// 解析 workbook.xml 里的 <definedNames><definedName name="...">Sheet1!$A$1:$B$10</definedName>...
/// 命名区域。localSheetId（sheet 级作用域命名区域）暂不区分，一律按 workbook 级处理——
/// 跟 roe_model::calc::Workbook.named_ranges 是单一扁平 HashMap 的既有设计保持一致，
/// 不引入作用域概念（如果后续需要区分，得先扩展 roe_model 的数据结构，不是这里的事）。
fn read_defined_names<R: Read + std::io::Seek>(
    zip: &mut ZipArchive<R>,
) -> Result<(HashMap<String, roe_model::calc::CellRange>, HashMap<String, String>), ParseError> {
    let xml = match read_zip_entry(zip, "xl/workbook.xml") {
        Ok(x) => x,
        Err(_) => return Ok((HashMap::new(), HashMap::new())),
    };
    let mut reader = Reader::from_str(&xml);
    reader.config_mut().trim_text(true);

    let mut buf = Vec::new();
    let mut named_ranges = HashMap::new();
    // ROOT tdf164895: definedName 值文本没有 '!' 分隔符时，parse_defined_name_ref
    // 直接返回 None（区域解析失败），此前该名字被静默丢弃。这类值实际是"公式类型"
    // 命名定义（如 COUNTA($A:$A)），单独收集进 named_formulas，交给 roe-calc 在
    // 求值期按公式重新解析，而不是在解析期直接放弃。
    let mut named_formulas = HashMap::new();
    let mut current_name: Option<String> = None;

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Start(e) if e.name().as_ref() == b"definedName" => {
                let mut name = String::new();
                for attr in e.attributes().flatten() {
                    if attr.key.as_ref() == b"name" {
                        name = attr.unescape_value()?.into_owned();
                    }
                }
                current_name = if name.is_empty() { None } else { Some(name) };
            }
            Event::Text(t) => {
                if let Some(name) = current_name.take() {
                    let raw = t.unescape()?.into_owned();
                    if let Some(range) = parse_defined_name_ref(&raw) {
                        named_ranges.insert(name, range);
                    } else {
                        named_formulas.insert(name, raw);
                    }
                }
            }
            Event::End(e) if e.name().as_ref() == b"definedName" => {
                current_name = None;
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok((named_ranges, named_formulas))
}

/// ROOT-041: 解析 workbook.xml 里的 <workbookPr date1904="1"/> 标记。
/// 决定该workbook的日期序列号基准是1904系统（Mac Excel传统）还是标准
/// 1900系统。省略该属性、或属性值不是"1"/"true"时，按OOXML规范默认为
/// false（1900系统）。读取失败（文件不存在等）也保守返回false。
fn read_date1904_flag<R: Read + std::io::Seek>(
    zip: &mut ZipArchive<R>,
) -> bool {
    let xml = match read_zip_entry(zip, "xl/workbook.xml") {
        Ok(x) => x,
        Err(_) => return false,
    };
    let mut reader = Reader::from_str(&xml);
    reader.config_mut().trim_text(true);

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Empty(e)) | Ok(Event::Start(e)) if e.name().as_ref() == b"workbookPr" => {
                for attr in e.attributes().flatten() {
                    if attr.key.as_ref() == b"date1904" {
                        if let Ok(v) = attr.unescape_value() {
                            return v.as_ref() == "1" || v.as_ref() == "true";
                        }
                    }
                }
                return false;
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }
    false
}

/// 解析 definedName 的引用文本，格式形如 "Sheet1!$A$1:$B$10" 或 "Sheet1!$A$1"。
/// 跨 sheet 前缀是必须的（同名 sheet 引用 xlsx 里也总是带 sheet 名前缀），
/// 没有 "!" 分隔符的情况（理论上不应出现在合法 xlsx 里）直接放弃解析，返回 None。
pub fn parse_defined_name_ref(raw: &str) -> Option<roe_model::calc::CellRange> {
    // 只接受"一个 sheet 前缀 + 一个单格/矩形区域"；其余（OFFSET(...)、多区域、
    // 外部引用 [1]Sheet!、整列 $A:$A / 整行 $1:$1 等）返回 None，由调用方当公式类
    // 名称原样保留，避免被错误解析成垃圾区域后写出时损坏原值。
    let (sheet_part, range_part) = raw.rsplit_once('!')?;
    let sheet = if sheet_part.len() >= 2 && sheet_part.starts_with('\'') && sheet_part.ends_with('\'') {
        let inner = &sheet_part[1..sheet_part.len() - 1];
        if inner.is_empty() || inner.contains('[') || inner.contains(']') {
            return None;
        }
        // 内部单引号只允许成对出现（''）
        if inner.replace("''", "").contains('\'') {
            return None;
        }
        inner.replace("''", "'")
    } else {
        if sheet_part.is_empty()
            || sheet_part.chars().any(|c| {
                !(c.is_alphanumeric() || c == '_' || c == '.')
            })
        {
            return None;
        }
        sheet_part.to_string()
    };
    let cleaned_range = range_part.replace('$', "");
    // 每个端点必须是 列字母+行号 都齐全
    let endpoints: Vec<&str> = cleaned_range.split(':').collect();
    if endpoints.is_empty() || endpoints.len() > 2 {
        return None;
    }
    for ep in &endpoints {
        let letters = ep.chars().take_while(|c| c.is_ascii_alphabetic()).count();
        let digits = ep.chars().skip(letters).take_while(|c| c.is_ascii_digit()).count();
        if letters == 0 || digits == 0 || letters + digits != ep.chars().count() {
            return None;
        }
    }
    let mut range = parse_cell_range(&cleaned_range)?;
    if range.start.0 > range.end.0 || range.start.1 > range.end.1 {
        return None;
    }
    range.sheet = sheet;
    Some(range)
}

// ─── workbook.xml.rels ───────────────────────────────────────────────────────

fn read_workbook_rels<R: Read + std::io::Seek>(
    zip: &mut ZipArchive<R>,
) -> Result<HashMap<String, String>, ParseError> {
    let xml = read_zip_entry(zip, "xl/_rels/workbook.xml.rels")?;
    let mut reader = Reader::from_str(&xml);
    reader.config_mut().trim_text(true);

    let mut buf = Vec::new();
    let mut map = HashMap::new();
    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Empty(e) | Event::Start(e) if e.name().as_ref() == b"Relationship" => {
                let mut id = String::new();
                let mut target = String::new();
                for attr in e.attributes().flatten() {
                    match attr.key.as_ref() {
                        b"Id" => id = attr.unescape_value()?.into_owned(),
                        b"Target" => target = attr.unescape_value()?.into_owned(),
                        _ => {}
                    }
                }
                if !id.is_empty() && !target.is_empty() {
                    map.insert(id, target);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(map)
}

fn normalize_sheet_path(target: &str) -> String {
    // target 形如 "worksheets/sheet1.xml"、"/xl/worksheets/sheet1.xml"
    // 或 "../drawings/drawing1.xml"（相对于 xl/worksheets/）
    if target.starts_with('/') {
        target.trim_start_matches('/').to_string()
    } else if target.starts_with("xl/") {
        target.to_string()
    } else if target.starts_with("../") {
        // ../drawings/drawing1.xml → xl/drawings/drawing1.xml
        format!("xl/{}", &target[3..])
    } else {
        format!("xl/{target}")
    }
}

// ─── worksheet ───────────────────────────────────────────────────────────────

fn read_worksheet<R: Read + std::io::Seek>(
    zip: &mut ZipArchive<R>,
    path: &str,
    name: &str,
    sst: &[CellValue],
    style_formats: &[Option<roe_model::calc::CellFormat>],
    dxf_formats: &[roe_model::calc::CellFormat],
    theme_colors: &HashMap<String, Color>,
) -> Result<Sheet, ParseError> {
    // 流式解析：直接从 zip entry 读取，不全量缓存 XML
    let entry = zip.by_name(path)
        .map_err(|e| ParseError::Malformed(format!("zip entry '{path}' not found: {e}")))?;
    let mut reader = Reader::from_reader(std::io::BufReader::new(entry));
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();

    // 按列收集：col → [(row, CellValue)]
    let mut col_data: HashMap<u32, Vec<(u32, CellValue)>> = HashMap::new();

    let mut cur_row: u32 = 0;
    let mut cur_col: u32 = 0;
    let mut cur_type: CellType = CellType::Number;
    // 列宽（pt，key=列号0-based）/ 行高（pt，key=行号0-based）
    // xlsx <col width> 是字符数单位，近似换算：pt = (width*7+5)*0.75
    // xlsx <row ht> 本身就是 pt，直接用
    let mut col_width_map: HashMap<u32, f32> = HashMap::new();
    let mut row_height_map: HashMap<u32, roe_model::calc::RowHeight> = HashMap::new();
    let mut default_col_width: f32 = 0.0;
    let mut default_row_height: f32 = 0.0;
    let mut zoom_scale: Option<u32> = None;
    let mut show_grid_lines: Option<bool> = None;
    let mut frozen_rows_val: u32 = 0;
    let mut frozen_cols_val: u32 = 0;
    let mut col_hidden_map: HashMap<u32, bool> = HashMap::new();
    let mut col_outline_map: HashMap<u32, u8> = HashMap::new();
    let mut col_style_map: HashMap<u32, roe_model::calc::CellFormat> = HashMap::new();
    let mut row_hidden_map: HashMap<u32, bool> = HashMap::new();
    let mut row_outline_map: HashMap<u32, u8> = HashMap::new();
    let mut merged_cells: Vec<roe_model::calc::MergedCell> = Vec::new();
    let mut cell_formats: std::collections::HashMap<(u32,u32), roe_model::calc::CellFormat> = std::collections::HashMap::new();
    let mut cond_formats: Vec<roe_model::calc::ConditionalFormatRule> = Vec::new();
    // 条件格式解析状态
    let mut cf_sqref = String::new();
    let mut in_cf = false;
    let mut cf_type = String::new();
    let mut cf_operator = String::new();
    let mut cf_formula: Vec<String> = Vec::new();
    let mut cf_dxf_id: Option<u32> = None;
    let mut in_cf_formula = false;
    let mut cf_formula_buf = String::new();
    // dataBar/colorScale/iconSet 解析状态
    let mut in_cf_databar = false;
    let mut in_cf_colorscale = false;
    let mut in_cf_iconset = false;
    let mut cf_cfvos: Vec<roe_model::calc::CfValue> = Vec::new();
    let mut cf_colors: Vec<String> = Vec::new();
    let mut cf_databar_direction = String::new();
    let mut cf_databar_bar_only = false;
    let mut cf_stop_if_true = false;
    let mut cf_databar_gradient = true; // xlsx: gradient 默认 true (无 attr 视为 gradient)
    let mut cf_iconset_style = String::new();
    let mut cf_iconset_percent = true;
    let mut cf_iconset_reverse = false;
    let mut cf_iconset_icons_only = false;

    // 数据验证解析状态
    let mut data_validations: Vec<roe_model::calc::DataValidationRule> = Vec::new();
    let mut in_dv = false;
    let mut dv_sqref = String::new();
    let mut dv_type = String::new();
    let mut dv_operator = String::new();
    let mut dv_allow_blank = false;
    let mut dv_show_error_message = false;
    let mut dv_error_msg: Option<String> = None;
    let mut in_dv_formula1 = false;
    let mut in_dv_formula2 = false;
    let mut dv_formula1_buf = String::new();
    let mut dv_formula2_buf = String::new();
    let mut in_v = false;
    let mut in_f = false;
    let mut cur_style: u32 = 0;
    let mut v_buf = String::new();
    let mut f_buf = String::new();

    // ROOT-014: 共享公式支持。si -> (模板公式文本, 模板所在row0, 模板所在col0)。
    // 每个worksheet(<sheetData>)独立一份映射，随本函数(每sheet调用一次)生命周期存在。
    let mut shared_formula_templates: HashMap<u32, (String, u32, u32)> = HashMap::new();
    // 当前<f>标签的t="shared"标记 + si属性值（None表示当前不是共享公式）。
    let mut cur_f_shared_si: Option<u32> = None;

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Start(e) | Event::Empty(e) => match e.name().as_ref() {
                b"row" => {
                    let mut row_no: Option<u32> = None;
                    let mut ht_pt: Option<f32> = None;
                    let mut row_hidden: bool = false;
                    let mut row_outline: u8 = 0;
                    // ROOT-082 optimalRowHeight: customHeight 属性缺失与显式
                    // false 语义等价（均代表"这行需要自动计算行高，ht 只是
                    // 上次保存的建议值"），故默认值为 false，与 LOK
                    // sheetdatacontext.cxx:296 的 mbCustomHeight 默认行为对齐。
                    // 参见 optimalRowHeight_design_v1.md 三、1节。
                    let mut row_custom_height: bool = false;
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"r" => {
                                row_no = attr.unescape_value()?.parse().ok();
                            }
                            b"ht" => {
                                ht_pt = attr.unescape_value()?.parse::<f32>().ok();
                            }
                            b"customHeight" => {
                                if let Ok(v) = attr.unescape_value() {
                                    row_custom_height = v.as_ref() == "1" || v.as_ref() == "true";
                                }
                            }
                            b"hidden" => {
                                if let Ok(v) = attr.unescape_value() {
                                    row_hidden = v.as_ref() == "1" || v.as_ref() == "true";
                                }
                            }
                            b"outlineLevel" => {
                                if let Ok(v) = attr.unescape_value() {
                                    row_outline = v.parse::<u8>().unwrap_or(0);
                                }
                            }
                            _ => {}
                        }
                    }
                    if let Some(r) = row_no {
                        cur_row = r;
                    }
                    // ROOT-038修复：每行开始时重置cur_col为0。
                    // 根因：cur_col此前只在<c r="...">显式给出r属性时更新，
                    // 从不在<row>边界重置，也不在缺失r属性时自动递增。
                    // 若上一行最后一个cell列号较大、本行第一个cell又恰好
                    // 省略了r属性，会错误继承上一行残留的列号。此处重置为0，
                    // 配合下面<c>处理逻辑中"无r属性时cur_col+1"的自动递增，
                    // 保证每行都从正确的起点开始顺序推算列号。
                    cur_col = 0;
                    if let Some(r) = row_no {
                        if r > 0 {
                            match ht_pt {
                                Some(h) if h > 0.0 => {
                                    // xlsx row 行号是 1-based，内存模型是 0-based
                                    // h 已是 pt，直接存储
                                    // ROOT-082: customHeight="true" 时 ht 是权威值
                                    // （Manual）；否则 ht 只是建议起始估算值，仍需
                                    // 后续懒加载测量覆盖（Auto，is_dirty=true）。
                                    let rh = if row_custom_height {
                                        roe_model::calc::RowHeight::Manual(h)
                                    } else {
                                        roe_model::calc::RowHeight::Auto { current: h, is_dirty: true }
                                    };
                                    row_height_map.insert(r - 1, rh);
                                }
                                _ => {
                                    // ROOT-082延伸修复：此前<row>标签存在但完全没有ht
                                    // 属性（tdf123026核心case）时，本分支此前直接跳过
                                    // 不insert任何记录，导致两个连带问题：(1) 这一行
                                    // 自己没有Auto占位入口，后续Layout阶段测量逻辑无从
                                    // 覆盖；(2) map_to_dense_vec_rowheight按max_key构建
                                    // 稠密数组，这一行"从map里完全消失"会拖累max_key
                                    // 偏小，导致该行及之后所有行在row_heights里全部
                                    // 数组越界返回None，回退到写死兜底默认值——这正是
                                    // design_v1.md开篇描述、本应由本次ROOT-082解决、
                                    // 但因为这个分支条件遗漏而未被真正修复的bug。
                                    // 无ht属性时插入Auto占位（is_dirty=true待测量覆盖），
                                    // current先给15.0字面量兜底，不依赖default_row_height
                                    // 变量当前是否已解析到（sheetFormatPr顺序不保证），
                                    // 反正is_dirty=true会触发后续真实测量覆盖这个值。
                                    row_height_map.insert(r - 1, roe_model::calc::RowHeight::Auto { current: 15.0, is_dirty: true });
                                }
                            }
                        }
                    }
                    if let Some(r) = row_no {
                        if r > 0 {
                            if row_hidden {
                                row_hidden_map.insert(r - 1, true);
                            }
                            if row_outline > 0 {
                                row_outline_map.insert(r - 1, row_outline);
                            }
                        }
                    }
                }
                b"sheetView" => {
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"zoomScale" => {
                                if let Ok(v) = attr.unescape_value() {
                                    if let Ok(z) = v.parse::<u32>() {
                                        zoom_scale = Some(z);
                                    }
                                }
                            }
                            b"showGridLines" => {
                                if let Ok(v) = attr.unescape_value() {
                                    show_grid_lines = Some(v.as_ref() != "0" && v.as_ref() != "false");
                                }
                            }
                            _ => {}
                        }
                    }
                }
                // ROOT-032: <pane> 是 <sheetView> 的子标签（quick-xml 扁平事件流下
                // 与 sheetView 属于同一层 match，无需状态机嵌套）。
                // xSplit/ySplit 是冻结线左侧/上方的列数/行数（xlsx 规范，非坐标）。
                // 只有 state="frozen" 或 "frozenSplit" 才是真正冻结；普通 "split"
                // 是可拖动分割窗格，不属于冻结，ROE 当前不支持不冻结的 pane，忽略即可。
                b"pane" => {
                    let mut pane_state = String::new();
                    let mut x_split: f32 = 0.0;
                    let mut y_split: f32 = 0.0;
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"xSplit" => {
                                if let Ok(v) = attr.unescape_value() {
                                    x_split = v.parse::<f32>().unwrap_or(0.0);
                                }
                            }
                            b"ySplit" => {
                                if let Ok(v) = attr.unescape_value() {
                                    y_split = v.parse::<f32>().unwrap_or(0.0);
                                }
                            }
                            b"state" => {
                                if let Ok(v) = attr.unescape_value() {
                                    pane_state = v.to_string();
                                }
                            }
                            _ => {}
                        }
                    }
                    if pane_state == "frozen" || pane_state == "frozenSplit" {
                        // xSplit/ySplit 在 frozen 状态下是整数列/行计数，向下取整防御性处理
                        frozen_cols_val = x_split.round().max(0.0) as u32;
                        frozen_rows_val = y_split.round().max(0.0) as u32;
                    }
                }
                b"sheetFormatPr" => {
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"defaultColWidth" => {
                                if let Ok(v) = attr.unescape_value() {
                                    if let Ok(w) = v.parse::<f32>() {
                                        // 同 col width 换算：pt = (w*7+5)*0.75
                                        default_col_width = (w * 7.0 + 5.0) * 0.75;
                                    }
                                }
                            }
                            b"defaultRowHeight" => {
                                if let Ok(v) = attr.unescape_value() {
                                    if let Ok(h) = v.parse::<f32>() {
                                        default_row_height = h;
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                b"col" => {
                    let mut min_col: Option<u32> = None;
                    let mut max_col: Option<u32> = None;
                    let mut width_chars: Option<f32> = None;
                    let mut col_hidden: bool = false;
                    let mut col_outline: u8 = 0;
                    let mut col_style_idx: Option<u32> = None;
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"min" => {
                                min_col = attr.unescape_value()?.parse().ok();
                            }
                            b"max" => {
                                max_col = attr.unescape_value()?.parse().ok();
                            }
                            b"width" => {
                                width_chars = attr.unescape_value()?.parse::<f32>().ok();
                            }
                            b"hidden" => {
                                if let Ok(v) = attr.unescape_value() {
                                    col_hidden = v.as_ref() == "1" || v.as_ref() == "true";
                                }
                            }
                            b"outlineLevel" => {
                                if let Ok(v) = attr.unescape_value() {
                                    col_outline = v.parse::<u8>().unwrap_or(0);
                                }
                            }
                            b"style" => {
                                col_style_idx = attr.unescape_value()?.parse::<u32>().ok();
                            }
                            _ => {}
                        }
                    }
                    if let (Some(min_c), Some(max_c), Some(wc)) = (min_col, max_col, width_chars) {
                        if min_c > 0 && max_c >= min_c && wc > 0.0 {
                            // xlsx width 是字符数，px = width*7+5，pt = px*0.75
                            let pt = (wc * 7.0 + 5.0) * 0.75;
                            // xlsx col min/max 是 1-based 闭区间，内存模型是 0-based
                            for c in min_c..=max_c {
                                col_width_map.insert(c - 1, pt);
                            }
                        }
                    }
                    if let (Some(min_c), Some(max_c)) = (min_col, max_col) {
                        if min_c > 0 && max_c >= min_c {
                            // ROOT: xlsx <col style="N"> 列级默认样式，N 是 cellXfs
                            // 下标，用调用方已解析好的 style_formats 直接查出完整
                            // CellFormat（Some(_) 才有效，越界/未定义样式静默跳过）。
                            let col_fmt = col_style_idx
                                .and_then(|idx| style_formats.get(idx as usize))
                                .and_then(|opt| opt.clone());
                            for c in min_c..=max_c {
                                if col_hidden {
                                    col_hidden_map.insert(c - 1, true);
                                }
                                if col_outline > 0 {
                                    col_outline_map.insert(c - 1, col_outline);
                                }
                                if let Some(ref fmt) = col_fmt {
                                    col_style_map.insert(c - 1, fmt.clone());
                                }
                            }
                        }
                    }
                }
                b"conditionalFormatting" => {
                    in_cf = true;
                    cf_sqref.clear();
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"sqref" {
                            cf_sqref = attr.unescape_value().unwrap_or_default().into_owned();
                        }
                    }
                }
                b"cfRule" if in_cf => {
                    cf_type.clear();
                    cf_operator.clear();
                    cf_formula.clear();
                    cf_dxf_id = None;
                    cf_cfvos.clear();
                    cf_colors.clear();
                    cf_databar_direction.clear();
                    cf_databar_bar_only = false;
                    cf_stop_if_true = false;
                    cf_databar_gradient = true;
                    cf_iconset_style.clear();
                    cf_iconset_percent = true;
                    cf_iconset_reverse = false;
                    cf_iconset_icons_only = false;
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"type"     => { cf_type = attr.unescape_value().unwrap_or_default().into_owned(); }
                            b"operator" => { cf_operator = attr.unescape_value().unwrap_or_default().into_owned(); }
                            b"dxfId"    => { cf_dxf_id = attr.unescape_value().ok().and_then(|v| v.parse().ok()); }
                            b"stopIfTrue" => { cf_stop_if_true = attr.unescape_value().map(|v| v == "1" || v == "true").unwrap_or(false); }
                            _ => {}
                        }
                    }
                }
                b"formula" if in_cf => {
                    in_cf_formula = true;
                    cf_formula_buf.clear();
                }
                b"dataBar" if in_cf => {
                    in_cf_databar = true;
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"minLength" => {}
                            b"showValue" => {
                                cf_databar_bar_only = attr.unescape_value().map(|v| v == "0" || v == "false").unwrap_or(false);
                            }
                            b"maxLength" => {}
                            b"gradient" => {
                                cf_databar_gradient = attr.unescape_value().map(|v| v != "0" && v != "false").unwrap_or(true);
                            }
                            b"direction" => {
                                cf_databar_direction = attr.unescape_value().unwrap_or_default().into_owned();
                            }
                            _ => {}
                        }
                    }
                }
                b"colorScale" if in_cf => { in_cf_colorscale = true; }
                b"iconSet" if in_cf => {
                    in_cf_iconset = true;
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"iconSet" => {
                                cf_iconset_style = attr.unescape_value().unwrap_or_default().into_owned();
                            }
                            b"percent" => {
                                cf_iconset_percent = attr.unescape_value().map(|v| v != "0" && v != "false").unwrap_or(true);
                            }
                            b"reverse" => {
                                cf_iconset_reverse = attr.unescape_value().map(|v| v == "1" || v == "true").unwrap_or(false);
                            }
                            b"showValue" => {
                                cf_iconset_icons_only = attr.unescape_value().map(|v| v == "0" || v == "false").unwrap_or(false);
                            }
                            _ => {}
                        }
                    }
                }
                b"cfvo" if in_cf_databar || in_cf_colorscale || in_cf_iconset => {
                    let mut kind = String::new();
                    let mut val = String::new();
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"type" => { kind = attr.unescape_value().unwrap_or_default().into_owned(); }
                            b"val"  => { val  = attr.unescape_value().unwrap_or_default().into_owned(); }
                            _ => {}
                        }
                    }
                    cf_cfvos.push(roe_model::calc::CfValue { kind, val, gte: true });
                }
                b"color" if in_cf_databar || in_cf_colorscale => {
                    // ROOT-022/025修复(v133)：优先处理rgb属性（原有逻辑不变），
                    // rgb不存在时尝试theme+tint组合解析主题色，此前完全没有
                    // 这个分支，导致theme主题色场景下cf_colors留空、渲染时
                    // 回退到硬编码默认色。
                    let mut rgb_hex: Option<String> = None;
                    let mut theme_idx: Option<u32> = None;
                    let mut tint: f64 = 0.0;
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"rgb" => {
                                let raw = attr.unescape_value().unwrap_or_default();
                                rgb_hex = Some(if raw.len() == 8 { format!("#{}", &raw[2..]) } else { format!("#{}", raw) });
                            }
                            b"theme" => {
                                theme_idx = attr.unescape_value().ok().and_then(|v| v.parse::<u32>().ok());
                            }
                            b"tint" => {
                                tint = attr.unescape_value().ok().and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
                            }
                            _ => {}
                        }
                    }
                    if let Some(hex) = rgb_hex {
                        cf_colors.push(hex);
                    } else if let Some(idx) = theme_idx {
                        if let Some(hex) = resolve_theme_color(idx, tint, theme_colors) {
                            cf_colors.push(hex);
                        }
                    }
                }
                b"mergeCell" => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"ref" {
                            let ref_str = attr.unescape_value()?;
                            if let Some(mc) = parse_merge_ref(&ref_str) {
                                merged_cells.push(mc);
                            }
                        }
                    }
                }
                b"dataValidations" => {}
                b"dataValidation" => {
                    in_dv = true;
                    dv_sqref.clear();
                    dv_type.clear();
                    dv_operator.clear();
                    dv_allow_blank = false;
                    dv_show_error_message = false;
                    dv_error_msg = None;
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"sqref" => { dv_sqref = attr.unescape_value().unwrap_or_default().into_owned(); }
                            b"type" => { dv_type = attr.unescape_value().unwrap_or_default().into_owned(); }
                            b"operator" => { dv_operator = attr.unescape_value().unwrap_or_default().into_owned(); }
                            b"allowBlank" => {
                                dv_allow_blank = attr.unescape_value().map(|v| v == "1" || v == "true").unwrap_or(false);
                            }
                            b"showErrorMessage" => {
                                dv_show_error_message = attr.unescape_value().map(|v| v == "1" || v == "true").unwrap_or(false);
                            }
                            b"error" => {
                                dv_error_msg = attr.unescape_value().ok().map(|v| v.into_owned());
                            }
                            _ => {}
                        }
                    }
                }
                b"formula1" if in_dv => {
                    in_dv_formula1 = true;
                    dv_formula1_buf.clear();
                }
                b"formula2" if in_dv => {
                    in_dv_formula2 = true;
                    dv_formula2_buf.clear();
                }
                b"c" => {
                    cur_type = CellType::Number;
                    cur_style = 0;
                    v_buf.clear();
                    f_buf.clear();
                    // ROOT-038修复：先假定本cell没有r属性，按OOXML规范自动
                    // 递增列号（同一行内，省略r的cell紧跟在上一个cell右侧）。
                    // 下面的属性循环若实际读到r属性，会覆盖这个推算值。
                    cur_col += 1;
                    // v121修复(ROOT-013)：显式强制复位in_v/in_f。
                    // 根因：<f t="shared" si="N"/>或<v/>这类自闭合标签只会
                    // 触发Event::Empty（走上面Start|Empty合流分支设true），
                    // 不会再触发对应的Event::End，导致in_f/in_v可能从上一个
                    // 单元格残留为true。若不在这里清零，本单元格的<v>文本
                    // （甚至后续多个单元格的文本）会被误判为还在<f>内部，
                    // 持续追加进f_buf，表现为大片文本单元格被解析成公式
                    // （tdf98481.xlsx可复现）。每个新<c>开始时强制清零，
                    // 保证状态不跨单元格污染，不依赖判断Event类型。
                    in_v = false;
                    in_f = false;
                    // ROOT-014修复的同时预防重蹈ROOT-013覆辙：cur_f_shared_si也必须
                    // 在每个新<c>开始时清零，否则上一个共享公式cell的si会残留，
                    // 污染下一个完全不含<f>的普通值cell（被误判成共享公式引用者）。
                    cur_f_shared_si = None;
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"r" => {
                                let addr = attr.unescape_value()?;
                                cur_col = col_from_ref(&addr);
                            }
                            b"t" => {
                                cur_type = CellType::from_str(&attr.unescape_value()?);
                            }
                            b"s" => {
                                cur_style = attr.unescape_value()?.parse().unwrap_or(0);
                            }
                            _ => {}
                        }
                    }
                }
                b"v" => {
                    in_v = true;
                    v_buf.clear();
                }
                b"f" => {
                    in_f = true;
                    f_buf.clear();
                    // ROOT-014: 解析共享公式属性。t="shared" + si="N"标记这是共享公式的
                    // 一员：模板"<f t=\"shared\" ref=\"B3:B7\" si=\"0\">A3</f>"（有ref属性，
                    // 有文本内容，会走正常的Event::End(b\"f\")把in_f复位），
                    // 或引用者"<f t=\"shared\" si=\"0\"/>"（自闭合、无ref属性、无内容）。
                    cur_f_shared_si = None;
                    let mut has_ref_attr = false;
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"t" => {
                                if attr.unescape_value().map(|v| v == "shared").unwrap_or(false) {
                                    if cur_f_shared_si.is_none() { cur_f_shared_si = Some(u32::MAX); }
                                }
                            }
                            b"ref" => { has_ref_attr = true; }
                            _ => {}
                        }
                    }
                    if cur_f_shared_si.is_some() {
                        for attr in e.attributes().flatten() {
                            if attr.key.as_ref() == b"si" {
                                if let Ok(v) = attr.unescape_value() {
                                    cur_f_shared_si = v.parse().ok();
                                }
                            }
                        }
                    }
                    // 关键修复：共享公式引用者(t="shared"且无ref属性)本质上是自闭合标签
                    // "<f .../>", quick-xml对自闭合标签只产生Event::Empty，不会再触发
                    // Event::End(b"f")，若不在这里立刻把in_f复位，后续紧跟的<v>文本内容
                    // 会被误判为还在<f>标签内部，持续追加进f_buf（本质是ROOT-013同类问题
                    // 在共享公式场景下的再次触发，v122实测tdf98481式的文本污染在
                    // shared-formula__text-results.xlsx上复现：B4的<v>C</v>被吃进了f_buf，
                    // 导致f_buf="C"而非空，共享公式展开逻辑因此被绕过）。
                    // 引用者没有文本内容需要累积（真正的公式文本来自模板map，不来自这个
                    // 标签的body），所以直接复位是安全的，不会丢失任何应该被解析的内容。
                    if cur_f_shared_si.is_some() && !has_ref_attr {
                        in_f = false;
                    }
                }
                _ => {}
            },
            Event::End(e) => match e.name().as_ref() {
                b"v" => in_v = false,
                b"f" => in_f = false,
                b"formula" if in_cf => {
                    in_cf_formula = false;
                    cf_formula.push(cf_formula_buf.trim().to_string());
                    cf_formula_buf.clear();
                }
                b"dataBar" if in_cf => { in_cf_databar = false; }
                b"colorScale" if in_cf => { in_cf_colorscale = false; }
                b"iconSet" if in_cf => { in_cf_iconset = false; }
                b"cfRule" if in_cf => {
                    let kind: Option<roe_model::calc::ConditionalFormatKind> = match cf_type.as_str() {
                        "dataBar" => {
                            let min = cf_cfvos.get(0).cloned().unwrap_or(roe_model::calc::CfValue {
                                kind: "min".to_string(), val: String::new(), gte: true,
                            });
                            let max = cf_cfvos.get(1).cloned().unwrap_or(roe_model::calc::CfValue {
                                kind: "max".to_string(), val: String::new(), gte: true,
                            });
                            let bar_color = cf_colors.get(0).cloned().unwrap_or_else(|| "#638EC6".to_string());
                            let direction = match cf_databar_direction.as_str() {
                                "leftToRight" => roe_model::calc::BarDirection::LeftToRight,
                                "rightToLeft" => roe_model::calc::BarDirection::RightToLeft,
                                _ => roe_model::calc::BarDirection::Auto,
                            };
                            Some(roe_model::calc::ConditionalFormatKind::DataBar {
                                min,
                                max,
                                bar_color,
                                border_color: None,
                                direction,
                                bar_only: cf_databar_bar_only,
                                solid_fill: !cf_databar_gradient,
                                // legacy <dataBar>(2007兼容格式)没有这三个字段，
                                // 后续若同range存在x14扩展databar，会在下方
                                // extend前的去重逻辑里被x14版本整体替换掉。
                                negative_fill_color: None,
                                axis_color: None,
                                axis_position: "automatic".to_string(),
                            })
                        }
                        "colorScale" => {
                            let n = cf_cfvos.len();
                            let min_cfvo = cf_cfvos.get(0).cloned().unwrap_or(roe_model::calc::CfValue {
                                kind: "min".to_string(), val: String::new(), gte: true,
                            });
                            let max_cfvo = cf_cfvos.get(n.saturating_sub(1)).cloned().unwrap_or(roe_model::calc::CfValue {
                                kind: "max".to_string(), val: String::new(), gte: true,
                            });
                            let min_color = cf_colors.get(0).cloned().unwrap_or_else(|| "#F8696B".to_string());
                            let max_color = cf_colors.get(n.saturating_sub(1)).cloned().unwrap_or_else(|| "#63BE7B".to_string());
                            let mid = if n >= 3 {
                                let mid_cfvo = &cf_cfvos[1];
                                let mid_color = cf_colors.get(1).cloned().unwrap_or_else(|| "#FFEB84".to_string());
                                Some((mid_cfvo.kind.clone(), mid_cfvo.val.clone(), mid_color))
                            } else {
                                None
                            };
                            Some(roe_model::calc::ConditionalFormatKind::ColorScale {
                                min_type: min_cfvo.kind.clone(),
                                min_value: if min_cfvo.val.is_empty() { None } else { Some(min_cfvo.val.clone()) },
                                min_color,
                                mid,
                                max_type: max_cfvo.kind.clone(),
                                max_value: if max_cfvo.val.is_empty() { None } else { Some(max_cfvo.val.clone()) },
                                max_color,
                            })
                        }
                        "iconSet" => {
                            Some(roe_model::calc::ConditionalFormatKind::IconSet {
                                icon_style: cf_iconset_style.clone(),
                                thresholds: cf_cfvos.clone(),
                                percent: cf_iconset_percent,
                                reverse: cf_iconset_reverse,
                                icons_only: cf_iconset_icons_only,
                                tier_overrides: None, // legacy <iconSet>标签无custom/cfIcon概念
                            })
                        }
                        _ => {
                            // 原有逻辑：普通规则
                            let condition = build_cf_condition(&cf_type, &cf_operator, &cf_formula);
                            let fmt = cf_dxf_id
                                .and_then(|id| dxf_formats.get(id as usize))
                                .cloned()
                                .unwrap_or_default();
                            Some(roe_model::calc::ConditionalFormatKind::CellFormat {
                                condition,
                                format: fmt,
                            })
                        }
                    };
                    if let Some(kind) = kind {
                        // 解析 sqref 范围（可能多个，空格分隔）
                        for sq in cf_sqref.split_whitespace() {
                            if let Some(range) = parse_cell_range(sq) {
                                cond_formats.push(roe_model::calc::ConditionalFormatRule {
                                    range,
                                    kind: kind.clone(),
                                    stop_if_true: cf_stop_if_true,
                                });
                            }
                        }
                    }
                }
                b"conditionalFormatting" => { in_cf = false; }
                b"formula1" if in_dv => { in_dv_formula1 = false; }
                b"formula2" if in_dv => { in_dv_formula2 = false; }
                b"dataValidation" => {
                    in_dv = false;
                    if !dv_type.is_empty() {
                        let formula1 = dv_formula1_buf.trim().to_string();
                        let formula2 = if dv_formula2_buf.trim().is_empty() {
                            None
                        } else {
                            Some(dv_formula2_buf.trim().to_string())
                        };
                        for sq in dv_sqref.split_whitespace() {
                            if let Some(range) = parse_cell_range(sq) {
                                data_validations.push(roe_model::calc::DataValidationRule {
                                    range,
                                    kind: dv_type.clone(),
                                    operator: dv_operator.clone(),
                                    formula1: formula1.clone(),
                                    formula2: formula2.clone(),
                                    allow_blank: dv_allow_blank,
                                    show_error: dv_show_error_message,
                                    error_msg: dv_error_msg.clone(),
                                });
                            }
                        }
                    }
                }
                b"c" => {
                    // 单元格结束：组装 CellValue
                    let row0 = cur_row.saturating_sub(1);
                    let col0 = cur_col.saturating_sub(1);

                    // ROOT-014: 共享公式模板登记 / 引用者展开。
                    // - 模板："<f t=\"shared\" si=\"N\">公式文本</f>"，f_buf非空，
                    //   登记到shared_formula_templates，本cell自身按f_buf原样求值
                    //   （模板cell自己不需要平移，它本来就是基准）。
                    // - 引用者："<f t=\"shared\" si=\"N\"/>"自闭合，f_buf为空，
                    //   查表取模板文本+模板坐标，按(row0,col0)与模板坐标的差值平移。
                    // - si不在表里（文件不合规，或模板尚未出现——理论上模板应先出现，
                    //   若引用者先出现说明文件顺序异常）：保留f_buf原样（空字符串，
                    //   build_cell_value对空f走正常value分支，不会误报公式）。
                    let effective_f: String = if let Some(si) = cur_f_shared_si {
                        if !f_buf.is_empty() {
                            shared_formula_templates.insert(si, (f_buf.clone(), row0, col0));
                            f_buf.clone()
                        } else if let Some((tmpl, t_row0, t_col0)) = shared_formula_templates.get(&si) {
                            let row_delta = row0 as i32 - *t_row0 as i32;
                            let col_delta = col0 as i32 - *t_col0 as i32;
                            shift_formula_refs(tmpl, row_delta, col_delta)
                        } else {
                            f_buf.clone()
                        }
                    } else {
                        f_buf.clone()
                    };

                    let val = build_cell_value(&cur_type, &v_buf, &effective_f, sst);
                    if !matches!(val, CellValue::Empty) {
                        col_data
                            .entry(col0)
                            .or_default()
                            .push((row0, val));
                    }
                    // 填入完整单元格样式（bold/颜色/数字格式等，由 read_styles 反查组装）
                    if let Some(Some(fmt)) = style_formats.get(cur_style as usize) {
                        cell_formats.insert((row0, col0), fmt.clone());
                    }
                }
                _ => {}
            },
            Event::Text(e) => {
                let t = e.unescape()?;
                if in_cf_formula {
                    cf_formula_buf.push_str(&t);
                } else if in_dv_formula1 {
                    dv_formula1_buf.push_str(&t);
                } else if in_dv_formula2 {
                    dv_formula2_buf.push_str(&t);
                } else if in_f {
                    f_buf.push_str(&t);
                } else if in_v {
                    v_buf.push_str(&t);
                } else if matches!(cur_type, CellType::InlineStr) {
                    // v318k: 内联字符串文本在 <is><t> 里，没有 <v>
                    v_buf.push_str(&t);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }

    // 把 col_data 转成 SparseGrid
    let grid = build_sparse_grid(col_data);

    // map → 下标连续数组：取最大 key 决定长度，空缺填 0.0
    // （roe-core::tile 的 col_width_at/row_height_at 对 <=0.0 的值会
    //  fallback 到默认宽高，逻辑上等价于 fods.rs 那种"未声明即默认"）
    let col_widths = map_to_dense_vec(&col_width_map);
    let row_heights = map_to_dense_vec_rowheight(&row_height_map);
    let col_hidden = map_to_dense_vec_bool(&col_hidden_map);
    let row_hidden = map_to_dense_vec_bool(&row_hidden_map);
    let col_outline_level = map_to_dense_vec_u8(&col_outline_map);
    let row_outline_level = map_to_dense_vec_u8(&row_outline_map);
    let col_style = map_to_dense_vec_option_cellformat(&col_style_map);

    Ok(Sheet {
        tables: vec![],
        sparkline_groups: vec![],
        col_style,
        name: name.to_string(),
        cells: grid,
        col_widths,
        row_heights,
        default_col_width,
        default_row_height,
        frozen_rows: frozen_rows_val,
        frozen_cols: frozen_cols_val,
        date1904: false, // ROOT-041: 此处占位，实际值由parse()主流程读取workbook.xml后统一广播覆盖
        zoom_scale,
        show_grid_lines,
        col_hidden,
        row_hidden,
        col_outline_level,
        row_outline_level,
            cell_formats,
            cond_formats,
            data_validations,
            auto_filter:      None,
            merged_cells,
            pivot:            None,
            floating_charts:  vec![],
            floating_stat_charts: vec![],
            floating_images:  vec![],
            floating_shapes: vec![],
            next_object_id: 0,
    })
}

// ─── 辅助：col/row map → 下标连续数组 ────────────────────────────────────────

fn map_to_dense_vec(map: &HashMap<u32, f32>) -> Vec<f32> {
    if map.is_empty() {
        return Vec::new();
    }
    let max_key = *map.keys().max().unwrap();
    let mut v = vec![0.0f32; max_key as usize + 1];
    for (k, val) in map {
        v[*k as usize] = *val;
    }
    v
}

/// ROOT-082: RowHeight 版本的稀疏 map → 稠密数组转换。
/// 空缺（该行既无 ht 也无任何行属性）填 RowHeight::default()
/// （即 Auto{current:0.0, is_dirty:true}），与 roe-core::tile 的
/// row_height_at 对 current<=0.0 会 fallback 到默认行高的既有逻辑
/// 保持等价（default_row_height fallback 语义不变）。
fn map_to_dense_vec_rowheight(map: &HashMap<u32, roe_model::calc::RowHeight>) -> Vec<roe_model::calc::RowHeight> {
    let max_key = map.keys().max().copied();
    match max_key {
        None => Vec::new(),
        Some(max_k) => {
            let mut v = vec![roe_model::calc::RowHeight::default(); (max_k + 1) as usize];
            for (&k, &val) in map.iter() {
                v[k as usize] = val;
            }
            v
        }
    }
}

fn map_to_dense_vec_bool(map: &HashMap<u32, bool>) -> Vec<bool> {
    if map.is_empty() {
        return Vec::new();
    }
    let max_key = *map.keys().max().unwrap();
    let mut v = vec![false; max_key as usize + 1];
    for (k, val) in map {
        v[*k as usize] = *val;
    }
    v
}

fn map_to_dense_vec_u8(map: &HashMap<u32, u8>) -> Vec<u8> {
    if map.is_empty() {
        return Vec::new();
    }
    let max_key = *map.keys().max().unwrap();
    let mut v = vec![0u8; max_key as usize + 1];
    for (k, val) in map {
        v[*k as usize] = *val;
    }
    v
}

/// col_style 专用：下标=col(0-based)，值=Option<CellFormat>（clone，因为
/// 同一 CellFormat 可能被多个 col 共享——一个 <col min="1" max="1025"
/// style="1"> 覆盖上千列，都指向同一份样式）。
fn map_to_dense_vec_option_cellformat(
    map: &HashMap<u32, roe_model::calc::CellFormat>,
) -> Vec<Option<roe_model::calc::CellFormat>> {
    if map.is_empty() {
        return Vec::new();
    }
    let max_key = *map.keys().max().unwrap();
    let mut v = vec![None; max_key as usize + 1];
    for (k, val) in map {
        v[*k as usize] = Some(val.clone());
    }
    v
}

// ─── 辅助：CellType ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
enum CellType {
    Number,
    SharedString,
    InlineStr,
    Bool,
    Error,
    Formula, // 有 <f> 无特殊 t
}

impl CellType {
    fn from_str(s: &str) -> Self {
        match s {
            "s" => Self::SharedString,
            "inlineStr" => Self::InlineStr,
            "b" => Self::Bool,
            "e" => Self::Error,
            "str" => Self::Formula,
            _ => Self::Number,
        }
    }
}

/// 导入公式时剥掉 `_xlfn.` 前缀(内部引擎使用无前缀函数名)。
/// 跳过字符串字面量与单引号工作表名;只剥标识符边界处的 `_xlfn.`。
fn strip_xlfn_prefix(f: &str) -> String {
    if !f.contains("_xlfn.") {
        return f.to_string();
    }
    let chars: Vec<char> = f.chars().collect();
    let pat: Vec<char> = "_xlfn.".chars().collect();
    let is_id = |c: char| c.is_alphanumeric() || c == '_' || c == '.' || !c.is_ascii();
    let mut out = String::with_capacity(f.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '"' || c == '\'' {
            let q = c;
            out.push(c);
            i += 1;
            while i < chars.len() {
                out.push(chars[i]);
                if chars[i] == q {
                    if i + 1 < chars.len() && chars[i + 1] == q {
                        out.push(chars[i + 1]);
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        if chars[i..].starts_with(&pat) && (i == 0 || !is_id(chars[i - 1])) {
            i += pat.len();
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

fn build_cell_value(t: &CellType, v: &str, f: &str, sst: &[CellValue]) -> CellValue {
    // 有公式优先存公式
    if !f.is_empty() {
        return CellValue::Formula { source: format!("={}", strip_xlfn_prefix(f)), cached: Box::new(CellValue::Empty) };
    }
    if v.is_empty() {
        return CellValue::Empty;
    }
    match t {
        // ROOT-078: sst元素本身已经是CellValue（Text或RichText），
        // 直接clone返回，不再额外包一层Text——sst.get(idx)拿到的就是
        // read_shared_strings按<si>内run数量组装好的最终值。
        CellType::SharedString => {
            let idx: usize = v.parse().unwrap_or(usize::MAX);
            sst.get(idx).cloned().unwrap_or(CellValue::Empty)
        }
        CellType::InlineStr | CellType::Formula => CellValue::Text(v.to_string()),
        CellType::Bool => CellValue::Bool(v == "1"),
        CellType::Error => CellValue::Error(parse_cell_error(v)),
        CellType::Number => v
            .parse::<f64>()
            .map(CellValue::Number)
            .unwrap_or(CellValue::Empty),
    }
}

fn parse_cell_error(s: &str) -> CellError {
    match s {
        "#DIV/0!" => CellError::Div0,
        "#N/A" => CellError::Na,
        "#NAME?" => CellError::Name,
        "#NULL!" => CellError::Null,
        "#NUM!" => CellError::Num,
        "#REF!" => CellError::Ref,
        "#VALUE!" => CellError::Value,
        _ => CellError::Na,
    }
}

// ─── 辅助：列地址解析 ─────────────────────────────────────────────────────────

/// "A1" → col=1, "B3" → col=2, "AA5" → col=27
fn col_from_ref(addr: &str) -> u32 {
    // 正常列字母最多3位（Excel上限 XFD=16384）。畸形/异常长的伪列字符串
    // （如命名范围里解析出的异常长字母串）用 saturating 运算避免乘法溢出panic
    // （v117修复，tdf144397触发）。
    let mut col: u32 = 0;
    for ch in addr.chars() {
        if ch.is_ascii_alphabetic() {
            col = col.saturating_mul(26).saturating_add(ch.to_ascii_uppercase() as u32 - b'A' as u32 + 1);
        } else {
            break;
        }
    }
    col
}

/// 列号(0-based)转字母，如 0→A, 25→Z, 26→AA。用于共享公式引用平移(ROOT-014)时
/// 把偏移后的数字列号写回公式文本。
fn col_to_letters(col0: u32) -> String {
    let mut col = col0 + 1; // 转成1-based，复用常见的"减1取余"算法
    let mut s = String::new();
    while col > 0 {
        let rem = (col - 1) % 26;
        s.insert(0, (b'A' + rem as u8) as char);
        col = (col - 1) / 26;
    }
    if s.is_empty() { s.push('A'); }
    s
}

/// 共享公式(ROOT-014)引用平移：把模板公式（在 (tmpl_row0, tmpl_col0) 处）里所有
/// 相对引用，按行/列偏移量平移到目标单元格的坐标系。
/// 规则（对齐Excel共享公式语义）：
/// - "$A$1" 绝对引用：行列都不平移
/// - "A$1" 列相对/行绝对：只平移列
/// - "$A1" 列绝对/行相对：只平移行
/// - "A1"  全相对：行列都平移
/// 不处理跨sheet引用（"Sheet1!A1"这种，本次不支持，按原样跳过不平移，
/// 避免误伤——共享公式规范上也极少跨sheet使用）。
/// 简单字符扫描实现，不引入regex依赖（项目当前无regex crate）。
fn shift_formula_refs(formula: &str, row_delta: i32, col_delta: i32) -> String {
    if row_delta == 0 && col_delta == 0 {
        return formula.to_string();
    }
    let chars: Vec<char> = formula.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let n = chars.len();

    while i < n {
        let ch = chars[i];

        // 尝试匹配一个引用token，起点必须是 '$' 或字母，且前一个字符不能是字母/数字
        // （避免把函数名里的字母、或sheet名当成引用起点误匹配）。
        let prev_is_ident = i > 0 && (chars[i - 1].is_ascii_alphanumeric() || chars[i - 1] == '_' || chars[i - 1] == '\'');
        let could_start_ref = (ch == '$' || ch.is_ascii_alphabetic()) && !prev_is_ident;

        if !could_start_ref {
            out.push(ch);
            i += 1;
            continue;
        }

        let start = i;
        let mut j = i;
        let mut col_abs = false;

        if j < n && chars[j] == '$' {
            col_abs = true;
            j += 1;
        }
        let col_start = j;
        while j < n && chars[j].is_ascii_alphabetic() {
            j += 1;
        }
        let col_str: String = chars[col_start..j].iter().collect();

        if col_str.is_empty() || col_str.len() > 3 {
            // 不是合法列字母（比如函数名 "SUM("），原样输出当前字符，继续扫描
            out.push(ch);
            i += 1;
            continue;
        }

        let mut row_abs = false;
        let mut k = j;
        if k < n && chars[k] == '$' {
            row_abs = true;
            k += 1;
        }
        let row_start = k;
        while k < n && chars[k].is_ascii_digit() {
            k += 1;
        }
        let row_str: String = chars[row_start..k].iter().collect();

        // 后面紧跟的字符不能是字母/左括号（排除函数名如 "ROW(" 被误认为列字母+左括号）
        let next_is_paren = k < n && chars[k] == '(';
        if row_str.is_empty() || next_is_paren {
            // 不构成完整的 "列字母+行数字"，原样输出当前字符，继续扫描
            out.push(ch);
            i += 1;
            continue;
        }

        // 匹配成功：col_str + row_str 是一个引用，按 col_abs/row_abs 决定是否平移
        let col0 = col_from_ref(&col_str);
        if col0 == 0 {
            out.push_str(&formula[start..k]);
            i = k;
            continue;
        }
        let row_num: i64 = row_str.parse().unwrap_or(0);
        if row_num == 0 {
            out.push_str(&formula[start..k]);
            i = k;
            continue;
        }

        let new_col0: i64 = if col_abs { (col0 as i64) - 1 } else { (col0 as i64) - 1 + col_delta as i64 };
        let new_row: i64 = if row_abs { row_num } else { row_num + row_delta as i64 };

        if new_col0 < 0 || new_row < 1 {
            // 平移后越界（理论上不该发生在合法共享公式范围内），原样保留不平移，
            // 避免生成非法引用文本
            out.push_str(&formula[start..k]);
        } else {
            if col_abs { out.push('$'); }
            out.push_str(&col_to_letters(new_col0 as u32));
            if row_abs { out.push('$'); }
            out.push_str(&new_row.to_string());
        }
        i = k;
    }

    out
}

/// 解析 "A1" → (row_0based, col_0based)
fn parse_cell_ref(addr: &str) -> Option<(u32, u32)> {
    // 同 col_from_ref：saturating 运算避免异常输入导致乘法溢出panic（v117修复）。
    let mut col: u32 = 0;
    let mut row_str = "";
    for (i, ch) in addr.char_indices() {
        if ch.is_ascii_alphabetic() {
            col = col.saturating_mul(26).saturating_add(ch.to_ascii_uppercase() as u32 - b'A' as u32 + 1);
        } else {
            row_str = &addr[i..];
            break;
        }
    }
    if col == 0 { return None; }
    let row: u32 = row_str.parse().ok()?;
    if row == 0 { return None; }
    Some((row - 1, col - 1))
}

/// 解析 "A1:C3" → MergedCell (0-based)
/// 从 cfRule type/operator/formula 构建 condition 字符串
fn build_cf_condition(cf_type: &str, operator: &str, formulas: &[String]) -> String {
    let f0 = formulas.get(0).map(|s| s.as_str()).unwrap_or("");
    let f1 = formulas.get(1).map(|s| s.as_str()).unwrap_or("");
    match cf_type {
        "cellIs" => match operator {
            "greaterThan"          => format!("gt:{}", f0),
            "greaterThanOrEqual"   => format!("gte:{}", f0),
            "lessThan"             => format!("lt:{}", f0),
            "lessThanOrEqual"      => format!("lte:{}", f0),
            "equal"                => format!("eq:{}", f0),
            "notEqual"             => format!("ne:{}", f0),
            "between"              => format!("between:{}:{}", f0, f1),
            "notBetween"           => format!("not_between:{}:{}", f0, f1),
            _                      => format!("cellIs:{}:{}", operator, f0),
        },
        "containsText"    => format!("text_contains:{}", f0),
        "notContainsText" => format!("text_not_contains:{}", f0),
        "beginsWith"      => format!("begins_with:{}", f0),
        "endsWith"        => format!("ends_with:{}", f0),
        "expression"      => format!("expr:{}", f0),
        "duplicateValues" => "duplicate".to_string(),
        "uniqueValues"    => "unique".to_string(),
        "top10"           => format!("top10:{}", f0),
        "aboveAverage"    => "above_average".to_string(),
        "timePeriod"      => format!("time_period:{}", f0),
        _                 => format!("{}:{}", cf_type, f0),
    }
}

/// 解析 "A1:C10" → CellRange（0-based）
/// ROOT-011: 解析 xl/tables/tableN.xml -> roe_model::calc::Table
/// 参考结构（tdf131424样本）：
/// <table name="MyTable1" displayName="MyTable1" ref="A1:C5" totalsRowShown="0">
///   <tableColumns count="3">
///     <tableColumn id="1" name="col name, may contain comma"/>
///     ...
///   </tableColumns>
/// </table>
/// 注意：列名可能含逗号，必须读 XML 属性值本身，不能按逗号切分字符串。
/// headerRowCount 省略时按 xlsx 规范默认视为 1（有表头）。
/// totalsRowCount 省略或为 "0" 视为无汇总行。
fn parse_table_xml(xml: &str) -> Option<roe_model::calc::Table> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();

    let mut display_name = String::new();
    let mut range: Option<roe_model::calc::CellRange> = None;
    let mut has_header = true;
    let mut has_totals = false;
    let mut columns: Vec<String> = Vec::new();
    // ROOT-050修复：table内嵌的<autoFilter ref="..."/>，真实样本里只带ref属性
    // （未见filterColumn/filters子标签用例），故只解析ref算出header_row/header_col/end_col。
    let mut auto_filter: Option<roe_model::calc::AutoFilter> = None;
    // tableStyleInfo:showRowStripes 省略时按规范视为 true,其余默认 false
    let mut style_name: Option<String> = None;
    let mut show_first_column = false;
    let mut show_last_column = false;
    let mut show_row_stripes = true;
    let mut show_column_stripes = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                let local = e.local_name();
                let tag = std::str::from_utf8(local.as_ref()).unwrap_or("");
                match tag {
                    "tableStyleInfo" => {
                        for attr in e.attributes().flatten() {
                            let key_name = attr.key.local_name();
                            let key = std::str::from_utf8(key_name.as_ref()).unwrap_or("");
                            let val = attr.unescape_value().unwrap_or_default().to_string();
                            let on = val == "1" || val.eq_ignore_ascii_case("true");
                            match key {
                                "name" if !val.is_empty() => style_name = Some(val),
                                "showFirstColumn" => show_first_column = on,
                                "showLastColumn" => show_last_column = on,
                                "showRowStripes" => show_row_stripes = on,
                                "showColumnStripes" => show_column_stripes = on,
                                _ => {}
                            }
                        }
                    }
                    "table" => {
                        for attr in e.attributes().flatten() {
                            let key_name = attr.key.local_name();
                            let key = std::str::from_utf8(key_name.as_ref()).unwrap_or("");
                            let val = attr.unescape_value().unwrap_or_default().to_string();
                            match key {
                                "displayName" => display_name = val,
                                "name" if display_name.is_empty() => display_name = val,
                                "ref" => range = parse_cell_range(&val),
                                "headerRowCount" => has_header = val != "0",
                                "totalsRowCount" => has_totals = val.parse::<u32>().unwrap_or(0) > 0,
                                _ => {}
                            }
                        }
                    }
                    "tableColumn" => {
                        for attr in e.attributes().flatten() {
                            let key_name = attr.key.local_name();
                            let key = std::str::from_utf8(key_name.as_ref()).unwrap_or("");
                            if key == "name" {
                                let val = attr.unescape_value().unwrap_or_default().to_string();
                                columns.push(val);
                            }
                        }
                    }
                    "autoFilter" => {
                        for attr in e.attributes().flatten() {
                            if attr.key.local_name().as_ref() == b"ref" {
                                let ref_str = attr.unescape_value().unwrap_or_default().into_owned();
                                if let Some(mc) = parse_merge_ref(&ref_str) {
                                    auto_filter = Some(roe_model::calc::AutoFilter {
                                        header_row: mc.r1,
                                        header_col: mc.c1,
                                        end_col: mc.c2,
                                        col_filters: std::collections::HashMap::new(),
                                        hidden_buttons: std::collections::HashSet::new(),
                                    });
                                } else if let Some((r, c)) = parse_cell_ref(ref_str.split(':').next().unwrap_or("")) {
                                    auto_filter = Some(roe_model::calc::AutoFilter {
                                        header_row: r,
                                        header_col: c,
                                        end_col: c,
                                        col_filters: std::collections::HashMap::new(),
                                        hidden_buttons: std::collections::HashSet::new(),
                                    });
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => return None,
            _ => {}
        }
        buf.clear();
    }

    let range = range?;
    if display_name.is_empty() {
        return None;
    }
    Some(roe_model::calc::Table {
        name: display_name,
        range,
        has_header,
        has_totals,
        columns,
        auto_filter,
        style_name,
        show_first_column,
        show_last_column,
        show_row_stripes,
        show_column_stripes,
        theme_palette: Vec::new(),
    })
}

/// 按 Excel theme 索引顺序(lt1,dk1,lt2,dk2,accent1~6)从主题色表取 10 色,缺失回退 Office 默认。
fn build_theme_palette(theme_colors: &HashMap<String, Color>) -> Vec<[u8; 3]> {
    const DEFAULTS: [(&str, [u8; 3]); 10] = [
        ("lt1", [0xFF, 0xFF, 0xFF]), ("dk1", [0x00, 0x00, 0x00]),
        ("lt2", [0xE7, 0xE6, 0xE6]), ("dk2", [0x44, 0x54, 0x6A]),
        ("accent1", [0x44, 0x72, 0xC4]), ("accent2", [0xED, 0x7D, 0x31]),
        ("accent3", [0xA5, 0xA5, 0xA5]), ("accent4", [0xFF, 0xC0, 0x00]),
        ("accent5", [0x5B, 0x9B, 0xD5]), ("accent6", [0x70, 0xAD, 0x47]),
    ];
    DEFAULTS.iter().map(|(k, d)| {
        theme_colors.get(*k).map(|c| [c.r, c.g, c.b]).unwrap_or(*d)
    }).collect()
}

#[cfg(test)]
mod table_style_tests {
    use super::*;

    #[test]
    fn parse_table_style_info() {
        let xml = r#"<table name="T1" displayName="T1" ref="A1:B3"><tableColumns count="2"><tableColumn id="1" name="a"/><tableColumn id="2" name="b"/></tableColumns><tableStyleInfo name="TableStyleMedium2" showFirstColumn="1" showLastColumn="0" showRowStripes="1" showColumnStripes="0"/></table>"#;
        let t = parse_table_xml(xml).expect("parse");
        assert_eq!(t.style_name.as_deref(), Some("TableStyleMedium2"));
        assert!(t.show_first_column);
        assert!(!t.show_last_column);
        assert!(t.show_row_stripes);
        assert!(!t.show_column_stripes);
    }

    #[test]
    fn default_font_black_not_written_as_fg_color() {
        // tdf131424: 数据格全是默认 xf 0,fonts[0] 为 theme=1(黑);
        // 不应带 fg_color,否则会盖掉表格样式字色
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../test_samples/xlsx/sc_data_xlsx__tdf131424.xlsx");
        let data = std::fs::read(path).expect("read sample");
        let wb = XlsxParser::parse(&data).expect("parse");
        let sh = wb.sheets.iter().find(|s| !s.tables.is_empty()).expect("sheet with table");
        for r in 0..5u32 {
            let fg = sh.cell_formats.get(&(r, 0)).and_then(|c| c.fg_color.clone());
            assert_eq!(fg, None, "row {} should have no fg_color", r);
        }
    }

    #[test]
    fn table_theme_palette_from_sample() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../test_samples/xlsx/sc_data_xlsx__TableStyleTest.xlsx");
        let data = std::fs::read(path).expect("read sample");
        let wb = XlsxParser::parse(&data).expect("parse");
        let t = &wb.sheets.iter().flat_map(|s| s.tables.iter()).next().expect("table");
        assert_eq!(t.theme_palette.len(), 10);
        assert_eq!(t.theme_palette[4], [0x15, 0x60, 0x82]);
    }

    #[test]
    fn parse_table_without_style_info_defaults() {
        let xml = r#"<table name="T2" displayName="T2" ref="A1:B3"><tableColumns count="2"><tableColumn id="1" name="a"/><tableColumn id="2" name="b"/></tableColumns></table>"#;
        let t = parse_table_xml(xml).expect("parse");
        assert_eq!(t.style_name, None);
        assert!(t.show_row_stripes);
    }
}

fn parse_cell_range(s: &str) -> Option<roe_model::calc::CellRange> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() == 2 {
        let c1 = col_from_ref(parts[0]).saturating_sub(1);
        let r1 = row_from_ref(parts[0]).saturating_sub(1);
        let c2 = col_from_ref(parts[1]).saturating_sub(1);
        let r2 = row_from_ref(parts[1]).saturating_sub(1);
        Some(roe_model::calc::CellRange { sheet: String::new(), start: (r1, c1), end: (r2, c2) })
    } else if parts.len() == 1 {
        // 单格 "A1"
        let c = col_from_ref(parts[0]).saturating_sub(1);
        let r = row_from_ref(parts[0]).saturating_sub(1);
        Some(roe_model::calc::CellRange { sheet: String::new(), start: (r, c), end: (r, c) })
    } else {
        None
    }
}

/// 从单元格地址提取行号（1-based）
fn row_from_ref(addr: &str) -> u32 {
    addr.chars()
        .filter(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .unwrap_or(1)
}

// ROOT-005修复(v126)：解析 <autoFilter ref="A1:K1"> 及其子标签
// <filterColumn colId="N" showButton="0"/>（不显示该列筛选按钮）
// <filterColumn colId="N"><filters><filter val="X"/>...</filters></filterColumn>（当前筛选值列表）
// colId 是相对 header_col（ref起始列）的偏移，写入 hidden_buttons/col_filters 时
// 统一转换成"相对header_col的偏移"存储，与AutoFilter结构体注释"col_offset"语义一致。
fn parse_autofilter_from_xml(xml: &str) -> Option<roe_model::calc::AutoFilter> {
    use quick_xml::events::Event;

    let start_idx = xml.find("<autoFilter")?;
    // autoFilter 标签可能是 <autoFilter ref="..."/> 自闭合，也可能带子标签直到 </autoFilter>
    let end_idx = match xml[start_idx..].find("</autoFilter>") {
        Some(i) => start_idx + i + "</autoFilter>".len(),
        None => {
            // 自闭合场景：找到本标签自己的 "/>"
            let rel_end = xml[start_idx..].find("/>")?;
            start_idx + rel_end + 2
        }
    };
    let slice = &xml[start_idx..end_idx];

    let mut reader = Reader::from_str(slice);
    reader.config_mut().trim_text(true);

    let mut header_row = 0u32;
    let mut header_col = 0u32;
    let mut end_col = 0u32;
    let mut col_filters: std::collections::HashMap<u32, Vec<String>> = std::collections::HashMap::new();
    let mut hidden_buttons: std::collections::HashSet<u32> = std::collections::HashSet::new();

    let mut cur_col_id: Option<u32> = None;
    let mut cur_filters: Vec<String> = Vec::new();
    let mut in_filters = false;

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                match e.local_name().as_ref() {
                    b"autoFilter" => {
                        for attr in e.attributes().flatten() {
                            if attr.key.local_name().as_ref() == b"ref" {
                                let ref_str = attr.unescape_value().unwrap_or_default().into_owned();
                                if let Some(mc) = parse_merge_ref(&ref_str) {
                                    header_row = mc.r1;
                                    header_col = mc.c1;
                                    end_col = mc.c2;
                                } else if let Some((r, c)) = parse_cell_ref(ref_str.split(':').next().unwrap_or("")) {
                                    // 单cell ref（无冒号，理论上罕见），兜底解析
                                    header_row = r;
                                    header_col = c;
                                    end_col = c;
                                }
                            }
                        }
                    }
                    b"filterColumn" => {
                        cur_col_id = None;
                        cur_filters.clear();
                        let mut show_button_zero = false;
                        for attr in e.attributes().flatten() {
                            match attr.key.local_name().as_ref() {
                                b"colId" => {
                                    cur_col_id = attr.unescape_value().ok()
                                        .and_then(|s| s.parse::<u32>().ok());
                                }
                                b"showButton" => {
                                    let v = attr.unescape_value().unwrap_or_default();
                                    if v.as_ref() == "0" || v.as_ref() == "false" {
                                        show_button_zero = true;
                                    }
                                }
                                _ => {}
                            }
                        }
                        if show_button_zero {
                            if let Some(cid) = cur_col_id {
                                hidden_buttons.insert(cid);
                            }
                        }
                    }
                    b"filter" => {
                        if in_filters {
                            for attr in e.attributes().flatten() {
                                if attr.key.local_name().as_ref() == b"val" {
                                    cur_filters.push(attr.unescape_value().unwrap_or_default().into_owned());
                                }
                            }
                        }
                    }
                    b"filters" => { in_filters = true; }
                    _ => {}
                }
            }
            Ok(Event::End(ref e)) => {
                match e.local_name().as_ref() {
                    b"filters" => { in_filters = false; }
                    b"filterColumn" => {
                        if let Some(cid) = cur_col_id {
                            if !cur_filters.is_empty() {
                                col_filters.insert(cid, cur_filters.clone());
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    Some(roe_model::calc::AutoFilter {
        header_row,
        header_col,
        end_col,
        col_filters,
        hidden_buttons,
    })
}

fn parse_merge_ref(ref_str: &str) -> Option<roe_model::calc::MergedCell> {
    let mut parts = ref_str.splitn(2, ':');
    let (r1, c1) = parse_cell_ref(parts.next()?)?;
    let (r2, c2) = parse_cell_ref(parts.next()?)?;
    Some(roe_model::calc::MergedCell { r1, c1, r2, c2 })
}

// ─── 辅助：SparseGrid 构建 ───────────────────────────────────────────────────

fn build_sparse_grid(col_data: HashMap<u32, Vec<(u32, CellValue)>>) -> SparseGrid {
    let mut cols = HashMap::new();
    for (col, mut cells) in col_data {
        cells.sort_by_key(|(r, _)| *r);

        let mut blocks: smallvec::SmallVec<[CellBlock; 4]> = smallvec::SmallVec::new();
        let mut cur_start = 0u32;
        let mut cur_vals: smallvec::SmallVec<[CellValue; 8]> = smallvec::SmallVec::new();

        for (row, val) in cells {
            if cur_vals.is_empty() {
                cur_start = row;
                cur_vals.push(val);
            } else {
                let expected = cur_start + cur_vals.len() as u32;
                if row == expected {
                    // 连续
                    cur_vals.push(val);
                } else {
                    // 有空洞，flush 当前 block
                    blocks.push(CellBlock {
                        start_row: cur_start,
                        values: cur_vals,
                    });
                    cur_vals = smallvec::SmallVec::new();
                    cur_start = row;
                    cur_vals.push(val);
                }
            }
        }
        if !cur_vals.is_empty() {
            blocks.push(CellBlock {
                start_row: cur_start,
                values: cur_vals,
            });
        }

        cols.insert(col, SparseColumn { blocks });
    }
    SparseGrid { cols }
}

// ─── 辅助：ZIP 读文件 ─────────────────────────────────────────────────────────

fn read_zip_entry<R: Read + std::io::Seek>(
    zip: &mut ZipArchive<R>,
    name: &str,
) -> Result<String, ParseError> {
    let mut entry = zip.by_name(name).map_err(|e| {
        ParseError::Malformed(format!("zip entry '{name}' not found: {e}"))
    })?;
    let mut buf = String::new();
    entry.read_to_string(&mut buf)?;
    Ok(buf)
}

// ─── 测试 ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // PPTX-13(a) 验证：图例/网格线"是否存在"判断，替代此前硬编码 true, true
    // （xlsx.rs 与 pptx.rs 各自维护一份几乎相同的 parse_pptx_chart_xml，
    // 两处均需要同步验证，参照 roe-ext 调研文档 §2.1 记录的复制维护现状）
    #[test]
    fn test_parse_chart_legend_and_grid_present() {
        let xml = r#"<?xml version="1.0"?>
<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart">
  <c:chart>
    <c:plotArea>
      <c:barChart>
        <c:ser>
          <c:tx><c:v>Series1</c:v></c:tx>
          <c:cat><c:strRef><c:strCache><c:pt idx="0"><c:v>A</c:v></c:pt></c:strCache></c:strRef></c:cat>
          <c:val><c:numRef><c:numCache><c:pt idx="0"><c:v>1</c:v></c:pt></c:numCache></c:numRef></c:val>
        </c:ser>
      </c:barChart>
      <c:catAx/>
      <c:valAx>
        <c:majorGridlines/>
      </c:valAx>
    </c:plotArea>
    <c:legend><c:legendPos val="r"/></c:legend>
  </c:chart>
</c:chartSpace>"#;
        let model = parse_ooxml_chart_xml(xml, &HashMap::new(), &HashMap::new()).expect("should parse chart");
        assert!(model.show_legend, "有 <c:legend> 标签，show_legend 应为 true");
        assert!(model.show_grid, "有 <c:majorGridlines/> 标签，show_grid 应为 true");
    }

    #[test]
    fn test_parse_chart_legend_and_grid_absent() {
        let xml = r#"<?xml version="1.0"?>
<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart">
  <c:chart>
    <c:plotArea>
      <c:barChart>
        <c:ser>
          <c:tx><c:v>Series1</c:v></c:tx>
          <c:cat><c:strRef><c:strCache><c:pt idx="0"><c:v>A</c:v></c:pt></c:strCache></c:strRef></c:cat>
          <c:val><c:numRef><c:numCache><c:pt idx="0"><c:v>1</c:v></c:pt></c:numCache></c:numRef></c:val>
        </c:ser>
      </c:barChart>
      <c:catAx/>
      <c:valAx/>
    </c:plotArea>
  </c:chart>
</c:chartSpace>"#;
        let model = parse_ooxml_chart_xml(xml, &HashMap::new(), &HashMap::new()).expect("should parse chart");
        assert!(!model.show_legend, "无 <c:legend> 标签，show_legend 应为 false");
        assert!(!model.show_grid, "无 <c:majorGridlines/> 标签，show_grid 应为 false");
    }

    // PPTX-13(c) 验证：xlsx 图表位置解析，参照图片坐标提取的既有模式，
    // 用 <xdr:graphicFrame> 内 <a:off>/<a:ext> 提取 EMU 坐标。
    #[test]
    fn test_extract_chart_rids_from_drawing_with_position() {
        let xml = r#"<?xml version="1.0"?>
<xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing"
          xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
          xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <xdr:twoCellAnchor>
    <xdr:from><xdr:col>1</xdr:col><xdr:row>1</xdr:row></xdr:from>
    <xdr:to><xdr:col>8</xdr:col><xdr:row>16</xdr:row></xdr:to>
    <xdr:graphicFrame>
      <xdr:nvGraphicFramePr/>
      <xdr:xfrm>
        <a:off x="1219200" y="609600"/>
        <a:ext cx="4572000" cy="2743200"/>
      </xdr:xfrm>
      <a:graphic>
        <a:graphicData>
          <c:chart xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" r:id="rId1"/>
        </a:graphicData>
      </a:graphic>
    </xdr:graphicFrame>
    <xdr:clientData/>
  </xdr:twoCellAnchor>
</xdr:wsDr>"#;
        let entries = extract_chart_rids_from_drawing(xml);
        assert_eq!(entries.len(), 1);
        let (rid, x_emu, y_emu, cx_emu, cy_emu) = &entries[0];
        assert_eq!(rid, "rId1");
        assert_eq!(*x_emu, 1219200);
        assert_eq!(*y_emu, 609600);
        assert_eq!(*cx_emu, 4572000);
        assert_eq!(*cy_emu, 2743200);

        // EMU -> pt 换算（1 pt = 12700 EMU），和调用点的换算方式保持一致
        let x_pt = *x_emu as f32 / 12700.0;
        let y_pt = *y_emu as f32 / 12700.0;
        assert!((x_pt - 96.0).abs() < 0.01);
        assert!((y_pt - 48.0).abs() < 0.01);
    }

    #[test]
    fn test_extract_chart_rids_from_drawing_no_graphicframe_returns_empty() {
        let xml = r#"<?xml version="1.0"?>
<xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing">
  <xdr:twoCellAnchor>
    <xdr:pic><xdr:nvPicPr/></xdr:pic>
  </xdr:twoCellAnchor>
</xdr:wsDr>"#;
        let entries = extract_chart_rids_from_drawing(xml);
        assert!(entries.is_empty());
    }

    #[test]
    fn test_col_from_ref() {
        assert_eq!(col_from_ref("A1"), 1);
        assert_eq!(col_from_ref("B3"), 2);
        assert_eq!(col_from_ref("Z1"), 26);
        assert_eq!(col_from_ref("AA1"), 27);
        assert_eq!(col_from_ref("AB5"), 28);
    }

    #[test]
    fn test_build_cell_value_number() {
        let v = build_cell_value(&CellType::Number, "3.14", "", &[]);
        assert!(matches!(v, CellValue::Number(n) if (n - 3.14).abs() < 1e-10));
    }

    #[test]
    fn test_build_cell_value_shared_string() {
        let sst = vec![CellValue::Text("hello".to_string()), CellValue::Text("world".to_string())];
        let v = build_cell_value(&CellType::SharedString, "1", "", &sst);
        assert!(matches!(v, CellValue::Text(s) if s == "world"));
    }

    #[test]
    fn test_strip_xlfn_prefix() {
        assert_eq!(strip_xlfn_prefix("_xlfn.T.TEST(A1,B1,2,1)"), "T.TEST(A1,B1,2,1)");
        assert_eq!(strip_xlfn_prefix("1-_xlfn.F.DIST(J5,H5,H6,1)"), "1-F.DIST(J5,H5,H6,1)");
        assert_eq!(strip_xlfn_prefix("\"_xlfn.X(\""), "\"_xlfn.X(\"");
        assert_eq!(strip_xlfn_prefix("SUM(A1:A3)"), "SUM(A1:A3)");
        assert_eq!(strip_xlfn_prefix("MY_xlfn.F(1)"), "MY_xlfn.F(1)");
    }
    #[test]
    fn test_build_cell_value_formula() {
        let v = build_cell_value(&CellType::Number, "42", "SUM(A1:A10)", &[]);
        assert!(matches!(v, CellValue::Formula { ref source, .. } if source == "=SUM(A1:A10)"));
    }

    #[test]
    fn test_build_sparse_grid_continuous() {
        let mut data = HashMap::new();
        data.insert(
            1u32,
            vec![
                (1u32, CellValue::Number(1.0)),
                (2u32, CellValue::Number(2.0)),
                (3u32, CellValue::Number(3.0)),
            ],
        );
        let grid = build_sparse_grid(data);
        let col = grid.cols.get(&1).unwrap();
        assert_eq!(col.blocks.len(), 1);
        assert_eq!(col.blocks[0].start_row, 1);
        assert_eq!(col.blocks[0].values.len(), 3);
    }

    #[test]
    fn test_build_sparse_grid_gap() {
        let mut data = HashMap::new();
        data.insert(
            1u32,
            vec![
                (1u32, CellValue::Number(1.0)),
                (5u32, CellValue::Number(5.0)), // 有空洞
            ],
        );
        let grid = build_sparse_grid(data);
        let col = grid.cols.get(&1).unwrap();
        assert_eq!(col.blocks.len(), 2);
    }

    #[test]
    fn test_map_to_dense_vec_basic() {
        let mut m = HashMap::new();
        m.insert(2u32, 50.0f32);
        m.insert(0u32, 30.0f32);
        let v = map_to_dense_vec(&m);
        assert_eq!(v.len(), 3);
        assert_eq!(v[0], 30.0);
        assert_eq!(v[1], 0.0); // 空缺补0
        assert_eq!(v[2], 50.0);
    }

    #[test]
    fn test_map_to_dense_vec_empty() {
        let m: HashMap<u32, f32> = HashMap::new();
        assert_eq!(map_to_dense_vec(&m), Vec::<f32>::new());
    }

    // 构造一个内存 zip，仅含一个 worksheet xml entry，用于直接测试
    // read_worksheet 的 col/row 解析逻辑
    fn build_zip_with_sheet(sheet_xml: &str) -> ZipArchive<std::io::Cursor<Vec<u8>>> {
        let mut buf = Vec::new();
        {
            let cursor = std::io::Cursor::new(&mut buf);
            let mut writer = zip::ZipWriter::new(cursor);
            let opts = zip::write::SimpleFileOptions::default();
            writer.start_file("xl/worksheets/sheet1.xml", opts).unwrap();
            std::io::Write::write_all(&mut writer, sheet_xml.as_bytes()).unwrap();
            writer.finish().unwrap();
        }
        ZipArchive::new(std::io::Cursor::new(buf)).unwrap()
    }

    #[test]
    fn test_read_worksheet_parses_col_widths() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <cols>
    <col min="1" max="2" width="20"/>
    <col min="3" max="3" width="10"/>
  </cols>
  <sheetData>
    <row r="1">
      <c r="A1" t="n"><v>1</v></c>
    </row>
  </sheetData>
</worksheet>"#;
        let mut zip = build_zip_with_sheet(xml);
        let sst: Vec<CellValue> = vec![];
        let sheet = read_worksheet(&mut zip, "xl/worksheets/sheet1.xml", "Sheet1", &sst, &[], &[], &HashMap::new()).unwrap();

        // col 1,2 (0-based: 0,1) width=20 chars -> px=20*7+5=145 -> pt=145*0.75=108.75
        // col 3 (0-based: 2) width=10 chars -> px=10*7+5=75 -> pt=75*0.75=56.25
        assert_eq!(sheet.col_widths.len(), 3);
        assert!((sheet.col_widths[0] - 108.75).abs() < 0.01);
        assert!((sheet.col_widths[1] - 108.75).abs() < 0.01);
        assert!((sheet.col_widths[2] - 56.25).abs() < 0.01);
    }

    #[test]
    fn test_read_worksheet_parses_row_heights() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData>
    <row r="1" ht="30"><c r="A1" t="n"><v>1</v></c></row>
    <row r="2" ht="15.75"><c r="A2" t="n"><v>2</v></c></row>
  </sheetData>
</worksheet>"#;
        let mut zip = build_zip_with_sheet(xml);
        let sst: Vec<CellValue> = vec![];
        let sheet = read_worksheet(&mut zip, "xl/worksheets/sheet1.xml", "Sheet1", &sst, &[], &[], &HashMap::new()).unwrap();

        // row 1,2 (0-based: 0,1) ht 直接是 pt，不换算
        assert_eq!(sheet.row_heights.len(), 2);
        assert!((sheet.row_heights[0].current() - 30.0).abs() < 0.01);
        assert!((sheet.row_heights[1].current() - 15.75).abs() < 0.01);
    }

    #[test]
    fn test_read_worksheet_no_cols_no_rows_height_is_empty() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData>
    <row r="1"><c r="A1" t="n"><v>1</v></c></row>
  </sheetData>
</worksheet>"#;
        let mut zip = build_zip_with_sheet(xml);
        let sst: Vec<CellValue> = vec![];
        let sheet = read_worksheet(&mut zip, "xl/worksheets/sheet1.xml", "Sheet1", &sst, &[], &[], &HashMap::new()).unwrap();

        // 没有 <cols> -> col_widths 保持空数组，渲染器会 fallback 到默认值。
        // ROOT-082延伸修复后：即使 <row> 没有 ht 属性，也会插入 Auto 占位
        // （current=15.0, is_dirty=true）供后续测量覆盖，避免该行及之后所有行
        // 因 map_to_dense_vec_rowheight 按 max_key 建稠密数组时 max_key 偏小
        // 而集体越界 fallback。因此 row_heights 长度应为 1（对应第1行的占位），
        // 不再是空数组。
        assert_eq!(sheet.col_widths.len(), 0);
        assert_eq!(sheet.row_heights.len(), 1);
        assert!(matches!(
            sheet.row_heights[0],
            roe_model::calc::RowHeight::Auto { current, is_dirty: true } if (current - 15.0).abs() < 0.01
        ));
    }
}

// ─── drawing/chart 辅助 ───────────────────────────────────────────────────────

fn parse_simple_rels(xml: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    loop {
        match reader.read_event() {
            Ok(quick_xml::events::Event::Empty(ref e)) | Ok(quick_xml::events::Event::Start(ref e))
                if e.local_name().as_ref() == b"Relationship" =>
            {
                let mut id = String::new();
                let mut target = String::new();
                for attr in e.attributes().flatten() {
                    match attr.key.as_ref() {
                        b"Id" => id = attr.unescape_value().unwrap_or_default().to_string(),
                        b"Target" => target = attr.unescape_value().unwrap_or_default().to_string(),
                        _ => {}
                    }
                }
                if !id.is_empty() { map.insert(id, target); }
            }
            Ok(quick_xml::events::Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }
    map
}

/// 从 drawing XML 提取图表的 (rId, x_emu, y_emu, cx_emu, cy_emu)
/// 参照 extract_image_rids_from_drawing 的模式：图表用 <xdr:graphicFrame> 作为
/// 边界（图片是 <xdr:pic>），内部同样是 <a:off>/<a:ext> 给出位置和尺寸。
// ─── chartEx(统计图)读取辅助 ─────────────────────────────────────────────

/// 含 chartEx 的 mc:AlternateContent 剔除其中的 mc:Fallback(WPS/Excel 写的
/// "此图表不可用"回退形状),避免被 collect_xlsx_shapes 读成文本框。
/// 与 pptx.rs 同名函数同逻辑;不含 chartex 的 AlternateContent 原样保留。
fn strip_chartex_fallback(xml: &str) -> String {
    const OPEN: &str = "<mc:AlternateContent";
    const CLOSE: &str = "</mc:AlternateContent>";
    let mut out = String::with_capacity(xml.len());
    let mut rest = xml;
    while let Some(s) = rest.find(OPEN) {
        let after = &rest[s..];
        let e = match after.find(CLOSE) { Some(e) => e + CLOSE.len(), None => break };
        let block = &after[..e];
        out.push_str(&rest[..s]);
        if block.contains("drawing/2014/chartex") {
            if let (Some(fs), Some(fe)) = (block.find("<mc:Fallback"), block.find("</mc:Fallback>")) {
                if fs < fe {
                    out.push_str(&block[..fs]);
                    out.push_str(&block[fe + "</mc:Fallback>".len()..]);
                    rest = &after[e..];
                    continue;
                }
            }
        }
        out.push_str(block);
        rest = &after[e..];
    }
    out.push_str(rest);
    out
}

fn chartex_xml_esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// 按区域取值,生成一组 <cx:lvl>。单行多列 → 一个 lvl;否则每列一个 lvl。
fn chartex_dim_lvls(
    range: &roe_model::calc::CellRange,
    is_num: bool,
    get: &dyn Fn(&str, u32, u32) -> CellValue,
) -> Option<String> {
    let (r1, c1, r2, c2) = (range.start.0, range.start.1, range.end.0, range.end.1);
    if r2 < r1 || c2 < c1 { return None; }
    if (r2 - r1 + 1) as u64 * (c2 - c1 + 1) as u64 > 1_000_000 { return None; }
    let mut lines: Vec<Vec<CellValue>> = Vec::new();
    if r1 == r2 && c2 > c1 {
        lines.push((c1..=c2).map(|c| get(&range.sheet, r1, c)).collect());
    } else {
        for c in c1..=c2 {
            lines.push((r1..=r2).map(|r| get(&range.sheet, r, c)).collect());
        }
    }
    let mut out = String::new();
    for line in &lines {
        if is_num {
            out.push_str(&format!("<cx:lvl ptCount=\"{}\" formatCode=\"General\">", line.len()));
            for (i, v) in line.iter().enumerate() {
                let n = match v {
                    CellValue::Number(n) => Some(*n),
                    CellValue::Text(t) => t.trim().parse::<f64>().ok(),
                    _ => None,
                };
                if let Some(n) = n {
                    out.push_str(&format!("<cx:pt idx=\"{i}\">{n}</cx:pt>"));
                }
            }
        } else {
            out.push_str(&format!("<cx:lvl ptCount=\"{}\">", line.len()));
            for (i, v) in line.iter().enumerate() {
                let t = match v {
                    CellValue::Number(n) => format!("{n}"),
                    CellValue::Text(t) => t.clone(),
                    _ => String::new(),
                };
                if !t.is_empty() {
                    out.push_str(&format!("<cx:pt idx=\"{i}\">{}</cx:pt>", chartex_xml_esc(&t)));
                }
            }
        }
        out.push_str("</cx:lvl>");
    }
    Some(out)
}

fn chartex_fill_dim_block(
    block: &str,
    is_num: bool,
    named: &HashMap<String, roe_model::calc::CellRange>,
    get: &dyn Fn(&str, u32, u32) -> CellValue,
) -> Option<String> {
    if block.contains("<cx:lvl") { return None; }
    let f_start = block.find("<cx:f")?;
    let f_gt = f_start + block[f_start..].find('>')?;
    let f_end = f_gt + block[f_gt..].find("</cx:f>")?;
    let name = block[f_gt + 1..f_end].trim();
    let range = named.get(name).cloned().or_else(|| parse_defined_name_ref(name))?;
    let lvls = chartex_dim_lvls(&range, is_num, get)?;
    let ins = f_end + "</cx:f>".len();
    Some(format!("{}{}{}", &block[..ins], lvls, &block[ins..]))
}

/// chartEx 没有数据缓存时(WPS 只写 <cx:f>_xlchart.v1.N</cx:f>),按 definedName
/// 展开成区域并从单元格取值,注入 <cx:lvl> 缓存,让 parse_chartex_xml 能读到数据。
/// 注入的是快照:之后改单元格,图不会自动更新。已有 <cx:lvl> 的维度原样保留。
fn inject_chartex_cache(
    xml: &str,
    named: &HashMap<String, roe_model::calc::CellRange>,
    get: &dyn Fn(&str, u32, u32) -> CellValue,
) -> String {
    let mut out = String::with_capacity(xml.len() + 1024);
    let mut rest = xml;
    loop {
        let s_pos = rest.find("<cx:strDim");
        let n_pos = rest.find("<cx:numDim");
        let (pos, is_num) = match (s_pos, n_pos) {
            (None, None) => break,
            (Some(s), None) => (s, false),
            (None, Some(n)) => (n, true),
            (Some(s), Some(n)) => if s < n { (s, false) } else { (n, true) },
        };
        let gt = match rest[pos..].find('>') { Some(g) => pos + g, None => break };
        if rest[..gt].ends_with('/') {
            out.push_str(&rest[..=gt]);
            rest = &rest[gt + 1..];
            continue;
        }
        let close = if is_num { "</cx:numDim>" } else { "</cx:strDim>" };
        let end = match rest[pos..].find(close) { Some(e) => pos + e + close.len(), None => break };
        let block = &rest[pos..end];
        out.push_str(&rest[..pos]);
        match chartex_fill_dim_block(block, is_num, named, get) {
            Some(b) => out.push_str(&b),
            None => out.push_str(block),
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

fn extract_chart_rids_from_drawing(xml: &str) -> Vec<(String, i64, i64, i64, i64)> {
    let mut results = Vec::new();
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut in_frame = false;
    let mut cur_rid = String::new();
    let mut cur_x: i64 = 0;
    let mut cur_y: i64 = 0;
    let mut cur_cx: i64 = 0;
    let mut cur_cy: i64 = 0;
    loop {
        match reader.read_event() {
            Ok(quick_xml::events::Event::Start(ref e)) => {
                if e.local_name().as_ref() == b"graphicFrame" {
                    in_frame = true;
                    cur_rid.clear();
                    cur_x = 0;
                    cur_y = 0;
                    cur_cx = 0;
                    cur_cy = 0;
                }
            }
            Ok(quick_xml::events::Event::Empty(ref e)) => {
                match e.local_name().as_ref() {
                    b"chart" if in_frame => {
                        for attr in e.attributes().flatten() {
                            let key = std::str::from_utf8(attr.key.as_ref()).unwrap_or("");
                            if key == "r:id" || key.ends_with("}id") {
                                cur_rid = attr.unescape_value().unwrap_or_default().to_string();
                            }
                        }
                    }
                    b"off" if in_frame => {
                        for attr in e.attributes().flatten() {
                            match attr.key.as_ref() {
                                b"x" => cur_x = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0),
                                b"y" => cur_y = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0),
                                _ => {}
                            }
                        }
                    }
                    b"ext" if in_frame => {
                        for attr in e.attributes().flatten() {
                            match attr.key.as_ref() {
                                b"cx" => cur_cx = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0),
                                b"cy" => cur_cy = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0),
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(quick_xml::events::Event::End(ref e)) => {
                if e.local_name().as_ref() == b"graphicFrame" {
                    if !cur_rid.is_empty() {
                        results.push((cur_rid.clone(), cur_x, cur_y, cur_cx, cur_cy));
                    }
                    in_frame = false;
                }
            }
            Ok(quick_xml::events::Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }
    results
}



// ─── drawing/image 辅助 ───────────────────────────────────────────────────────

/// 从 drawing XML 提取图片的 (rId, x_emu, y_emu, cx_emu, cy_emu)
fn extract_image_rids_from_drawing(xml: &str) -> Vec<(String, i64, i64, i64, i64, i64, bool)> {
    let mut results = Vec::new();
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut in_pic = false;
    let mut cur_rid = String::new();
    let mut cur_x: i64 = 0;
    let mut cur_y: i64 = 0;
    let mut cur_cx: i64 = 0;
    let mut cur_cy: i64 = 0;
    // ROOT-051修复：新增rot（旋转角度，来自<xdr:spPr><a:xfrm rot="...">，
    // 单位与RawXlsxShape.rot一致，OOXML原始的60000分之一度）和hidden
    // （来自<xdr:cNvPr hidden="1">）。此前两者在<xdr:pic>解析路径完全
    // 未处理，导致图片旋转恒为0、hidden="1"的图片仍被渲染。
    let mut cur_rot: i64 = 0;
    let mut cur_hidden = false;
    // ROOT-028修复(v129)：<a:blip>/<a:off>/<a:ext>在真实文件里既可能是自闭合
    // 标签（<a:blip r:embed="rId1"/>），也可能带子元素（如<a:blip r:embed="rId1">
    // <a:extLst>...</a:extLst></a:blip>，Office 2010+常见的useLocalDpi扩展），
    // 后一种情况quick-xml会产生Event::Start而非Event::Empty。原代码只在
    // Event::Empty分支处理这三个标签，导致带子元素的<a:blip>属性读取被
    // 完全跳过，cur_rid永远为空，图片在Event::End(pic)时因cur_rid.is_empty()
    // 被静默丢弃。现在Start和Empty共用同一段属性提取逻辑。
    fn handle_pic_child_attrs(
        name: &[u8],
        e: &quick_xml::events::BytesStart,
        in_pic: bool,
        cur_rid: &mut String,
        cur_x: &mut i64,
        cur_y: &mut i64,
        cur_cx: &mut i64,
        cur_cy: &mut i64,
        cur_rot: &mut i64,
        cur_hidden: &mut bool,
    ) {
        if !in_pic { return; }
        match name {
            b"blip" => {
                for attr in e.attributes().flatten() {
                    let key = std::str::from_utf8(attr.key.as_ref()).unwrap_or("");
                    if key == "r:embed" || key.ends_with("}embed") {
                        *cur_rid = attr.unescape_value().unwrap_or_default().to_string();
                    }
                }
            }
            b"off" => {
                for attr in e.attributes().flatten() {
                    match attr.key.as_ref() {
                        b"x" => *cur_x = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0),
                        b"y" => *cur_y = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0),
                        _ => {}
                    }
                }
            }
            b"ext" => {
                for attr in e.attributes().flatten() {
                    match attr.key.as_ref() {
                        b"cx" => *cur_cx = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0),
                        b"cy" => *cur_cy = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0),
                        _ => {}
                    }
                }
            }
            b"xfrm" => {
                for attr in e.attributes().flatten() {
                    if attr.key.as_ref() == b"rot" {
                        *cur_rot = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0);
                    }
                }
            }
            b"cNvPr" => {
                for attr in e.attributes().flatten() {
                    if attr.key.as_ref() == b"hidden" {
                        let v = attr.unescape_value().unwrap_or_default();
                        *cur_hidden = v.as_ref() == "1" || v.as_ref() == "true";
                    }
                }
            }
            _ => {}
        }
    }

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) => {
                let name = e.local_name();
                let name_ref = name.as_ref();
                if name_ref == b"pic" {
                    in_pic = true;
                    cur_rid.clear();
                    cur_rot = 0;
                    cur_hidden = false;
                } else {
                    handle_pic_child_attrs(name_ref, e, in_pic, &mut cur_rid, &mut cur_x, &mut cur_y, &mut cur_cx, &mut cur_cy, &mut cur_rot, &mut cur_hidden);
                }
            }
            Ok(Event::Empty(ref e)) => {
                let name = e.local_name();
                handle_pic_child_attrs(name.as_ref(), e, in_pic, &mut cur_rid, &mut cur_x, &mut cur_y, &mut cur_cx, &mut cur_cy, &mut cur_rot, &mut cur_hidden);
            }
            Ok(Event::End(ref e)) => {
                if e.local_name().as_ref() == b"pic" {
                    if !cur_rid.is_empty() {
                        results.push((cur_rid.clone(), cur_x, cur_y, cur_cx, cur_cy, cur_rot, cur_hidden));
                    }
                    in_pic = false;
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }
    results
}

/// ROOT-053/054修复：从drawing.xml提取<xdr:sp>（浮动形状/文本框）。
/// 此前xlsx drawing解析只处理<xdr:pic>，<xdr:sp>完全没有对应分支，
/// 导致所有形状/文本框类型的浮动对象在解析层就被跳过。
/// 参照extract_image_rids_from_drawing的整体模式（独立、不依赖sheet状态
/// 的纯函数），但因<xdr:sp>结构比<xdr:pic>复杂得多（几何/填充/文字），
/// 内部用简化状态机，只覆盖当前已知样本需要的字段：
///   - a:xfrm: off(x,y)/ext(cx,cy)/rot/flipV
///   - a:prstGeom: prst属性 + a:avLst/a:gd 的adj覆盖值
///   - a:solidFill > a:srgbClr（直接RGB）/ a:schemeClr（仅识别lt1/dk1
///     两个最常见主题槽位，映射到近似的白/黑，因为主题色表在sheet级别，
///     这个纯函数拿不到，属于已知简化——真实主题色替换留作后续任务）
///   - a:txBody > a:p > a:r > a:rPr(sz/b/spc) + a:t（文字内容）
/// 返回值：每个xdr:sp对应一个RawXlsxShape，调用方（collect_xlsx_shapes）
/// 负责EMU→pt换算和组装成最终的FloatingShape。
#[derive(Debug, Default, Clone)]
struct RawXlsxShape {
    prst: Option<String>,
    adj_overrides: Vec<(String, f64)>,
    off_x: i64,
    off_y: i64,
    ext_cx: i64,
    ext_cy: i64,
    rot: i32,
    flip_v: bool,
    fill_rgb: Option<(u8, u8, u8)>,
    /// <xdr:style><a:fontRef><a:schemeClr val="..."/></a:fontRef></xdr:style>
    /// 求出的兜底文字色：当某个 run 自身没有 <a:rPr><a:solidFill> 时使用。
    /// 参照 testShapeAutofit 样本核实：run 的 rPr 完全没写颜色，真实白色
    /// 文字来自 fontRef schemeClr="lt1"，不是渲染层默认黑色的 bug。
    style_font_rgb: Option<(u8, u8, u8)>,
    /// <xdr:style><a:fillRef><a:schemeClr val="..."/></a:fillRef></xdr:style>
    /// 求出的兜底填充色：当 spPr 内没有显式 <a:solidFill> 时使用（参照
    /// testShapeRotationImport 样本核实：spPr 只有 prstGeom，没有
    /// solidFill，真实蓝色填充完全来自 style fillRef 继承，与
    /// style_font_rgb 同一模式）。
    style_fill_rgb: Option<(u8, u8, u8)>,
    /// 按 <a:p> 段落分组的 run 列表：外层 Vec 是段落，内层 Vec 是该段落
    /// 内按顺序出现的 run。(text, size_pt, bold, spc_emu, color_rgb)。
    /// color_rgb 是 run 自身 <a:rPr><a:solidFill> 求出的颜色，None 表示
    /// 该 run 没有显式颜色，组装阶段需要 fallback 到 style_font_rgb，
    /// 再 fallback 到黑色。
    text_runs: Vec<Vec<(String, f32, bool, f32, Option<(u8, u8, u8)>)>>,
    /// ROOT-053/054任务B：<xdr:twoCellAnchor><xdr:from>/<xdr:to> 单元格
    /// 锚点。testShapeDisplacementOnRotationImport样本核实：该样本
    /// a:xfrm off/ext 全为0（不可用），真实位置尺寸只能从 from/to 两个
    /// 单元格锚点反推。(col, col_off_emu, row, row_off_emu)，
    /// col_off/row_off 单位是 EMU（与 xdr:colOff/rowOff 原始单位一致，
    /// 不是像 off_x/off_y 那样已经是 EMU 绝对值，需要在组装阶段结合
    /// 列宽/行高累加）。None 表示该 drawing 是 oneCellAnchor 或其他
    /// 不含 from/to 的锚点类型（本轮不处理，保持 None 即安全跳过
    /// fallback逻辑）。
    anchor_from: Option<(u32, i64, u32, i64)>,
    anchor_to: Option<(u32, i64, u32, i64)>,
}

/// 局部attr辅助函数：与pptx.rs的attr_str同签名同实现，避免跨文件依赖
/// pptx.rs的私有函数（两文件独立编译单元，不共享私有fn）。
fn attr_str_local(e: &quick_xml::events::BytesStart, key: &[u8]) -> Option<String> {
    e.attributes().flatten().find_map(|a| {
        if a.key.local_name().as_ref() == key {
            a.unescape_value().ok().map(|v| v.to_string())
        } else {
            None
        }
    })
}

fn attr_i32_local(e: &quick_xml::events::BytesStart, key: &[u8]) -> Option<i32> {
    attr_str_local(e, key)?.parse().ok()
}

fn extract_shapes_from_drawing(xml: &str) -> Vec<RawXlsxShape> {
    let mut results = Vec::new();
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut in_sp = false;
    let mut in_sppr = false;
    let mut in_av_lst = false;
    let mut in_tx_body = false;
    let mut in_t = false;
    let mut in_sp_fill = false; // 在spPr的solidFill内（区分ln的solidFill）
    let mut in_ln = false;
    let mut in_style = false;      // 在 <xdr:style> 内
    let mut in_font_ref = false;   // 在 <xdr:style><a:fontRef> 内
    let mut in_fill_ref = false;   // 在 <xdr:style><a:fillRef> 内
    let mut in_run_fill = false;   // 在 <a:rPr>/<a:endParaRPr> 的 <a:solidFill> 内
    let mut in_p = false;          // 在 <a:p> 段落内（txBody 内）
    // ROOT-053/054任务B：<xdr:twoCellAnchor><xdr:from>/<xdr:to> 与 <xdr:sp>
    // 是平级兄弟节点（from/to 在 sp 之前出现），不能像其余字段那样直接
    // 写进 cur（此时 in_sp 还是 false，且 <sp> Start 时 cur 会被重置清空）。
    // 用 pending_from/pending_to 在 twoCellAnchor 层级暂存，遇到 <xdr:sp>
    // Start 时再拷贝进新建的 cur。
    let mut in_from = false;
    let mut in_to = false;
    let mut in_anchor_col = false;
    let mut in_anchor_col_off = false;
    let mut in_anchor_row = false;
    let mut in_anchor_row_off = false;
    let mut cur_col: u32 = 0;
    let mut cur_col_off: i64 = 0;
    let mut cur_row: u32 = 0;
    let mut cur_row_off: i64 = 0;
    let mut pending_from: Option<(u32, i64, u32, i64)> = None;
    let mut pending_to: Option<(u32, i64, u32, i64)> = None;

    let mut cur: RawXlsxShape = RawXlsxShape::default();
    let mut cur_text = String::new();
    let mut cur_run_size_pt: f32 = 12.0;
    let mut cur_run_bold = false;
    let mut cur_run_spc: f32 = 0.0;
    let mut cur_run_color: Option<(u8, u8, u8)> = None;
    // 当前段落内已收集的 run 列表，</a:p> 时整体 push 进 cur.text_runs。
    let mut cur_para_runs: Vec<(String, f32, bool, f32, Option<(u8, u8, u8)>)> = Vec::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                let name = e.local_name();
                let name_ref = name.as_ref();
                match name_ref {
                    b"sp" => {
                        in_sp = true;
                        cur = RawXlsxShape::default();
                        cur.anchor_from = pending_from;
                        cur.anchor_to = pending_to;
                    }
                    b"from" => {
                        in_from = true;
                        cur_col = 0; cur_col_off = 0; cur_row = 0; cur_row_off = 0;
                    }
                    b"to" => {
                        in_to = true;
                        cur_col = 0; cur_col_off = 0; cur_row = 0; cur_row_off = 0;
                    }
                    b"col" if in_from || in_to => { in_anchor_col = true; }
                    b"colOff" if in_from || in_to => { in_anchor_col_off = true; }
                    b"row" if in_from || in_to => { in_anchor_row = true; }
                    b"rowOff" if in_from || in_to => { in_anchor_row_off = true; }
                    b"spPr" if in_sp => { in_sppr = true; }
                    b"xfrm" if in_sppr => {
                        if let Some(r) = attr_i32_local(e, b"rot") { cur.rot = r; }
                        if let Some(fv) = attr_str_local(e, b"flipV") {
                            cur.flip_v = fv == "1" || fv.eq_ignore_ascii_case("true");
                        }
                    }
                    b"off" if in_sppr => {
                        for attr in e.attributes().flatten() {
                            match attr.key.as_ref() {
                                b"x" => cur.off_x = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0),
                                b"y" => cur.off_y = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0),
                                _ => {}
                            }
                        }
                    }
                    b"ext" if in_sppr => {
                        for attr in e.attributes().flatten() {
                            match attr.key.as_ref() {
                                b"cx" => cur.ext_cx = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0),
                                b"cy" => cur.ext_cy = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0),
                                _ => {}
                            }
                        }
                    }
                    b"prstGeom" if in_sppr => {
                        cur.prst = attr_str_local(e, b"prst");
                    }
                    b"avLst" if in_sppr => { in_av_lst = true; }
                    b"gd" if in_av_lst => {
                        if let (Some(name_attr), Some(fmla)) =
                            (attr_str_local(e, b"name"), attr_str_local(e, b"fmla"))
                        {
                            if let Some(stripped) = fmla.strip_prefix("val ") {
                                if let Ok(val) = stripped.trim().parse::<f64>() {
                                    cur.adj_overrides.push((name_attr, val));
                                }
                            }
                        }
                    }
                    b"ln" if in_sppr => { in_ln = true; }
                    b"solidFill" if in_sppr && !in_ln => { in_sp_fill = true; }
                    b"solidFill" if in_tx_body => { in_run_fill = true; }
                    b"srgbClr" if in_sp_fill => {
                        if let Some(hex) = attr_str_local(e, b"val") {
                            let hex = hex.trim_start_matches('#');
                            if let Ok(v) = u32::from_str_radix(hex, 16) {
                                cur.fill_rgb = Some((((v >> 16) & 0xff) as u8, ((v >> 8) & 0xff) as u8, (v & 0xff) as u8));
                            }
                        }
                    }
                    b"schemeClr" if in_sp_fill => {
                        if let Some(val) = attr_str_local(e, b"val") {
                            cur.fill_rgb = Some(match val.as_str() {
                                "lt1" | "bg1" | "lt2" | "bg2" => (255, 255, 255),
                                "dk1" | "tx1" | "dk2" | "tx2" => (0, 0, 0),
                                _ => (0x70, 0x70, 0x70),
                            });
                        }
                    }
                    // run 自身颜色：<a:rPr>/<a:endParaRPr> 内的 <a:solidFill>，
                    // 支持 srgbClr（直接RGB）和 sysClr（windowText等系统色，
                    // 取 lastClr 属性作为已解析的兜底RGB，参照 textbox-
                    // CharKerningSpace 样本核实的写法：
                    // <a:sysClr val="windowText" lastClr="000000"/>）。
                    b"srgbClr" if in_run_fill => {
                        if let Some(hex) = attr_str_local(e, b"val") {
                            let hex = hex.trim_start_matches('#');
                            if let Ok(v) = u32::from_str_radix(hex, 16) {
                                cur_run_color = Some((((v >> 16) & 0xff) as u8, ((v >> 8) & 0xff) as u8, (v & 0xff) as u8));
                            }
                        }
                    }
                    b"sysClr" if in_run_fill => {
                        if let Some(hex) = attr_str_local(e, b"lastClr") {
                            let hex = hex.trim_start_matches('#');
                            if let Ok(v) = u32::from_str_radix(hex, 16) {
                                cur_run_color = Some((((v >> 16) & 0xff) as u8, ((v >> 8) & 0xff) as u8, (v & 0xff) as u8));
                            }
                        }
                    }
                    b"schemeClr" if in_run_fill => {
                        if let Some(val) = attr_str_local(e, b"val") {
                            cur_run_color = Some(match val.as_str() {
                                "lt1" | "bg1" | "lt2" | "bg2" => (255, 255, 255),
                                "dk1" | "tx1" | "dk2" | "tx2" => (0, 0, 0),
                                _ => (0x70, 0x70, 0x70),
                            });
                        }
                    }
                    // <xdr:style><a:fontRef><a:schemeClr val="..."/></a:fontRef></xdr:style>：
                    // shape 未显式指定 run 颜色时的兜底文字色来源，见
                    // RawXlsxShape.style_font_rgb 字段文档。
                    b"style" if in_sp => { in_style = true; }
                    b"fontRef" if in_style => { in_font_ref = true; }
                    b"schemeClr" if in_font_ref => {
                        if let Some(val) = attr_str_local(e, b"val") {
                            cur.style_font_rgb = Some(match val.as_str() {
                                "lt1" | "bg1" | "lt2" | "bg2" => (255, 255, 255),
                                "dk1" | "tx1" | "dk2" | "tx2" => (0, 0, 0),
                                _ => (0x70, 0x70, 0x70),
                            });
                        }
                    }
                    b"srgbClr" if in_font_ref => {
                        if let Some(hex) = attr_str_local(e, b"val") {
                            let hex = hex.trim_start_matches('#');
                            if let Ok(v) = u32::from_str_radix(hex, 16) {
                                cur.style_font_rgb = Some((((v >> 16) & 0xff) as u8, ((v >> 8) & 0xff) as u8, (v & 0xff) as u8));
                            }
                        }
                    }
                    // <xdr:style><a:fillRef><a:schemeClr val="..."/></a:fillRef></xdr:style>：
                    // shape spPr 内没有显式 solidFill 时的兜底填充色来源，
                    // 见 RawXlsxShape.style_fill_rgb 字段文档（同 fontRef
                    // 一样的 style 继承模式，testShapeRotationImport 样本
                    // 核实确认）。
                    b"fillRef" if in_style => { in_fill_ref = true; }
                    b"schemeClr" if in_fill_ref => {
                        if let Some(val) = attr_str_local(e, b"val") {
                            cur.style_fill_rgb = Some(match val.as_str() {
                                "lt1" | "bg1" | "lt2" | "bg2" => (255, 255, 255),
                                "dk1" | "tx1" | "dk2" | "tx2" => (0, 0, 0),
                                // accent1-6 等主题强调色：真实主题色表在
                                // workbook 级别，此纯函数拿不到，给一个
                                // 常见的 Office 默认 accent1 蓝色兜底
                                // （0x4472C4，比灰色更接近典型样本观感）。
                                // 与fill_rgb的schemeClr简化处理同一已知
                                // 限制，见 collect_xlsx_shapes 文档。
                                "accent1" => (0x44, 0x72, 0xC4),
                                _ => (0x70, 0x70, 0x70),
                            });
                        }
                    }
                    b"srgbClr" if in_fill_ref => {
                        if let Some(hex) = attr_str_local(e, b"val") {
                            let hex = hex.trim_start_matches('#');
                            if let Ok(v) = u32::from_str_radix(hex, 16) {
                                cur.style_fill_rgb = Some((((v >> 16) & 0xff) as u8, ((v >> 8) & 0xff) as u8, (v & 0xff) as u8));
                            }
                        }
                    }
                    b"txBody" if in_sp => { in_tx_body = true; }
                    b"p" if in_tx_body => {
                        in_p = true;
                        cur_para_runs.clear();
                    }
                    b"rPr" | b"endParaRPr" if in_tx_body => {
                        cur_run_size_pt = attr_str_local(e, b"sz")
                            .and_then(|s| s.parse::<f32>().ok())
                            .map(|v| v / 100.0)
                            .unwrap_or(12.0);
                        cur_run_bold = attr_str_local(e, b"b").map(|v| v == "1").unwrap_or(false);
                        cur_run_spc = attr_str_local(e, b"spc")
                            .and_then(|s| s.parse::<f32>().ok())
                            .unwrap_or(0.0);
                        cur_run_color = None;
                    }
                    b"t" if in_tx_body => {
                        in_t = true;
                        cur_text.clear();
                    }
                    _ => {}
                }
            }
            Ok(Event::End(ref e)) => {
                let name = e.local_name();
                match name.as_ref() {
                    b"sp" => {
                        if in_sp {
                            results.push(cur.clone());
                        }
                        in_sp = false;
                    }
                    b"spPr" => { in_sppr = false; }
                    b"avLst" => { in_av_lst = false; }
                    b"ln" => { in_ln = false; }
                    b"solidFill" => { in_sp_fill = false; in_run_fill = false; }
                    b"style" => { in_style = false; }
                    b"fontRef" => { in_font_ref = false; }
                    b"fillRef" => { in_fill_ref = false; }
                    b"txBody" => { in_tx_body = false; }
                    b"p" if in_tx_body => {
                        in_p = false;
                        if !cur_para_runs.is_empty() {
                            cur.text_runs.push(std::mem::take(&mut cur_para_runs));
                        }
                    }
                    b"t" => {
                        in_t = false;
                        if !cur_text.is_empty() {
                            cur_para_runs.push((cur_text.clone(), cur_run_size_pt, cur_run_bold, cur_run_spc, cur_run_color));
                        }
                    }
                    b"col" => { in_anchor_col = false; }
                    b"colOff" => { in_anchor_col_off = false; }
                    b"row" => { in_anchor_row = false; }
                    b"rowOff" => { in_anchor_row_off = false; }
                    b"from" => {
                        in_from = false;
                        pending_from = Some((cur_col, cur_col_off, cur_row, cur_row_off));
                    }
                    b"to" => {
                        in_to = false;
                        pending_to = Some((cur_col, cur_col_off, cur_row, cur_row_off));
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(ref e)) if in_t => {
                cur_text.push_str(&e.unescape().unwrap_or_default());
            }
            Ok(Event::Text(ref e)) if in_anchor_col || in_anchor_col_off || in_anchor_row || in_anchor_row_off => {
                let txt = e.unescape().unwrap_or_default();
                let txt = txt.trim();
                if in_anchor_col { cur_col = txt.parse().unwrap_or(0); }
                else if in_anchor_col_off { cur_col_off = txt.parse().unwrap_or(0); }
                else if in_anchor_row { cur_row = txt.parse().unwrap_or(0); }
                else if in_anchor_row_off { cur_row_off = txt.parse().unwrap_or(0); }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }
    results
}

/// 从 zip 里读取图片并转成 FloatingImage，追加到 sheet.floating_images
fn collect_xlsx_images<R: Read + std::io::Seek>(
    zip: &mut ZipArchive<R>,
    drawing_xml: &str,
    drawing_rel_map: &HashMap<String, String>,
    sheet: &mut Sheet,
) {
    use roe_model::calc::FloatingImage;
    let image_entries = extract_image_rids_from_drawing(drawing_xml);
    for (rid, x_emu, y_emu, cx_emu, cy_emu, rot_raw, hidden) in image_entries {
        // ROOT-051修复：hidden="1"的图片在解析阶段直接跳过，不进入
        // floating_images。理由：hidden在这个场景语义是"设计器里临时
        // 隐藏/占位用的对象，不应出现在任何渲染结果里"（与行列hidden
        // 的"逻辑存在但视觉隐藏"不同，这里没有"取消隐藏"的交互需求），
        // 在解析层丢弃比在渲染层判断更简单，也避免后续任何消费方
        // 忘记检查hidden字段而意外画出来。
        if hidden {
            continue;
        }
        let img_target = match drawing_rel_map.get(&rid) {
            Some(t) => t.clone(),
            None => continue,
        };
        let img_path = normalize_sheet_path(&img_target);
        // ROOT-028修复(v129)：图片是二进制数据（PNG/JPEG），此前误用
        // read_zip_entry（内部调用read_to_string，要求合法UTF-8）读取，
        // 导致所有图片读取失败（Io(InvalidData): "stream did not contain
        // valid UTF-8"），静默continue丢弃，floating_images恒为空。
        // 改用read_zip_entry_binary，模式照抄docx.rs::collect_docx_images
        // 的现成写法（zip.by_name + read_to_end -> Vec<u8>）。
        let img_bytes = match read_zip_entry_binary(zip, &img_path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        // EMU → pt（1 pt = 12700 EMU）
        let x_pt  = x_emu  as f32 / 12700.0;
        let y_pt  = y_emu  as f32 / 12700.0;
        let w_pt  = cx_emu as f32 / 12700.0;
        let h_pt  = cy_emu as f32 / 12700.0;
        // ROOT-051修复：rot单位从OOXML的60000分之一度换算成度数，
        // 与RawXlsxShape/FloatingShape.rot的换算方式（raw.rot as f32 / 60000.0）
        // 完全一致，保持两条解析路径的角度语义统一。
        let rot_deg = rot_raw as f32 / 60000.0;
        use base64::Engine as _;
        // DEVROOT-002修复：此前未记录图片格式，ODF写出时office:binary-data
        // 缺少mime-type属性，导致真实LibreOffice无法解码、静默降级为占位符
        // 渲染。从字节头嗅探格式（xlsx本身的Content_Types.xml也声明了扩展名，
        // 但字节嗅探更可靠、且与docx.rs现有路径复用同一份判断逻辑，不新增
        // 一套依赖.rels/Content_Types解析的独立方案）。
        let mime = sniff_image_mime(&img_bytes);
        let data_base64 = base64::engine::general_purpose::STANDARD.encode(&img_bytes);
        sheet.floating_images.push(FloatingImage {
            id: 0,
            data_base64,
            mime,
            width_pt:  w_pt,
            height_pt: h_pt,
            x_pt,
            y_pt,
            rot: rot_deg,
            hidden: false,
        });
    }
}

/// 将 extract_shapes_from_drawing 解析出的 RawXlsxShape 组装成 FloatingShape，
/// 追加到 sheet.floating_shapes。不需要 zip/rel_map（shape 不像图片需要
/// 额外读取二进制资源，几何/填充/文字全部内联在 drawing xml 里）。
/// ROOT-053/054任务B：把 <xdr:from>/<xdr:to> 的 (col, colOff_emu, row,
/// rowOff_emu) 锚点换算成 pt 坐标。逻辑照抄 roe-core::tile_geometry 的
/// col_width_at/row_height_at/col_x/row_y 累加算法，但不能跨 crate 依赖
/// roe-core（roe-parser 是纯解析层，Cargo.toml 未声明对 roe-core 的
/// 依赖，属于有意的架构分层，不应打破）。默认列宽/行高常量与
/// roe-core::tile_geometry::CalcRenderConfig::default() 保持一致
/// （48.0pt / 15.0pt），两处如需调整需同步修改。
fn anchor_col_x_pt(sheet: &Sheet, col: u32) -> f32 {
    (0..col).map(|c| anchor_col_width_pt(sheet, c)).sum()
}
fn anchor_col_width_pt(sheet: &Sheet, col: u32) -> f32 {
    if sheet.col_hidden.get(col as usize).copied().unwrap_or(false) {
        return 0.0;
    }
    sheet.col_widths.get(col as usize).copied().filter(|w| *w > 0.0)
        .unwrap_or(if sheet.default_col_width > 0.0 { sheet.default_col_width } else { 48.0 })
}
fn anchor_row_y_pt(sheet: &Sheet, row: u32) -> f32 {
    (0..row).map(|r| anchor_row_height_pt(sheet, r)).sum()
}
fn anchor_row_height_pt(sheet: &Sheet, row: u32) -> f32 {
    if sheet.row_hidden.get(row as usize).copied().unwrap_or(false) {
        return 0.0;
    }
    // ROOT-082: 纯只读场景（公式/图形锚点定位），不触发懒加载测量，
    // 直接读 current()（可能是尚未测量的估算值，与方案A的两阶段
    // 职责分离一致：只有 render_viewport_into 的 Layout 阶段才
    // 触发 &mut Sheet 测量回写）。
    sheet.row_heights.get(row as usize).map(|rh| rh.current()).filter(|h| *h > 0.0)
        .unwrap_or(if sheet.default_row_height > 0.0 { sheet.default_row_height } else { 15.0 })
}
/// (col, colOff_emu, row, rowOff_emu) → (x_pt, y_pt)
fn anchor_to_pt(sheet: &Sheet, anchor: (u32, i64, u32, i64)) -> (f32, f32) {
    let (col, col_off_emu, row, row_off_emu) = anchor;
    let x_pt = anchor_col_x_pt(sheet, col) + col_off_emu as f32 / 12700.0;
    let y_pt = anchor_row_y_pt(sheet, row) + row_off_emu as f32 / 12700.0;
    (x_pt, y_pt)
}

fn collect_xlsx_shapes(drawing_xml: &str, sheet: &mut Sheet) {
    use roe_model::calc::FloatingShape;
    use roe_model::common::{Color, FillStyle, ShapeGeometry};
    use roe_model::writer::{Paragraph, Run};
    use roe_model::preset_geometry::{eval_preset_shape, find_preset_shape};
    use std::collections::HashMap as StdHashMap;

    let raw_shapes = extract_shapes_from_drawing(drawing_xml);

    for raw in raw_shapes {
        // EMU → pt（1 pt = 12700 EMU）
        let mut x_pt = raw.off_x as f32 / 12700.0;
        let mut y_pt = raw.off_y as f32 / 12700.0;
        let mut width_pt = raw.ext_cx as f32 / 12700.0;
        let mut height_pt = raw.ext_cy as f32 / 12700.0;

        // ROOT-053/054任务B修复：testShapeDisplacementOnRotationImport
        // 样本核实，a:xfrm off/ext 全为 0（不可用作真实位置尺寸），但
        // twoCellAnchor 的 from/to 单元格锚点有效。仅在 ext 全零（即
        // xfrm 未提供有效尺寸）时才 fallback，避免影响其余 xfrm 数据
        // 正常的样本（如 testShapeRotationImport 等已验证通过的用例）。
        if raw.ext_cx == 0 && raw.ext_cy == 0 {
            if let (Some(from), Some(to)) = (raw.anchor_from, raw.anchor_to) {
                let (fx, fy) = anchor_to_pt(sheet, from);
                let (tx, ty) = anchor_to_pt(sheet, to);
                // from/to 反推出的是 twoCellAnchor 在 Excel UI 里的「旋转后」
                // 可视框（testShapeDisplacementOnRotationImport 样本实测
                // 确认：直接把这个框当「旋转前」传给 geometry 求值，downArrow
                // 公式内部guide比例严重失真，构造出扁到变形的箭头）。
                // FloatingShape.x_pt/y_pt/width_pt/height_pt 的既定语义是
                // 「旋转前」局部框（与 xfrm off/ext 正常场景语义一致，
                // draw_floating_shape 的 rot 旋转变换正是绕这个框的中心转，
                // 把它转到最终可视位置）——所以这里要做的不是直接采用
                // from/to 的框，而是从「旋转后可视框」反推「旋转前框」。
                // 仅处理整90/270度特例（当前样本集唯一场景）：宽高对调，
                // 且旋转前后中心点不变（旋转本身就是绕中心转，不改变中心）。
                // 任意角度的通用反投影留到后续样本出现时再设计。
                let vis_x = fx;
                let vis_y = fy;
                let vis_w = (tx - fx).max(0.0);
                let vis_h = (ty - fy).max(0.0);
                let rot_deg_normalized = ((raw.rot as f32 / 60000.0) % 360.0 + 360.0) % 360.0;
                let is_quarter_turn = (rot_deg_normalized - 90.0).abs() < 0.01
                    || (rot_deg_normalized - 270.0).abs() < 0.01;
                if is_quarter_turn {
                    let cx = vis_x + vis_w / 2.0;
                    let cy = vis_y + vis_h / 2.0;
                    width_pt = vis_h;
                    height_pt = vis_w;
                    x_pt = cx - width_pt / 2.0;
                    y_pt = cy - height_pt / 2.0;
                } else {
                    x_pt = vis_x;
                    y_pt = vis_y;
                    width_pt = vis_w;
                    height_pt = vis_h;
                }
            }
        }

        // OOXML rot 单位是 1/60000 度，换算成普通角度制
        let rot = raw.rot as f32 / 60000.0;

        // 几何组装：prst 为 None 或 "rect" 时退化为 Rect；
        // 其余 prst 查预设形状公式表求值成 PresetPath；
        // 表里查不到的（如 custGeom，本轮未处理）落入 Custom 兜底。
        let geometry = match raw.prst.as_deref() {
            Some("rect") | None => ShapeGeometry::Rect,
            Some(other) => match find_preset_shape(other) {
                Some(formula) => {
                    // width_pt/height_pt 现在统一是「旋转前」局部框语义
                    // （正常 xfrm ext 场景本来如此；ROOT-053/054任务B的
                    // from/to fallback 分支也已反推成同一语义，见上方
                    // fallback 代码块注释），geometry 求值直接使用即可，
                    // 不需要在这里再单独处理旋转特例。
                    let w = width_pt as f64;
                    let h = height_pt as f64;
                    let adj_map: StdHashMap<&str, f64> = raw
                        .adj_overrides
                        .iter()
                        .map(|(n, v)| (n.as_str(), *v))
                        .collect();
                    let path = eval_preset_shape(formula, w, h, &adj_map);
                    // v181: prst 名字（other）+ adj_overrides 随路径一并打包，
                    // 详见 ShapeGeometry::PresetPathData 文档注释。
                    ShapeGeometry::PresetPath(roe_model::common::PresetPathData {
                        commands: path,
                        preset_name: other.to_string(),
                        adj_overrides: raw.adj_overrides.clone(),
                    })
                }
                None => ShapeGeometry::Custom(other.to_string()),
            },
        };

        // 填充色优先级：spPr 显式 <a:solidFill> > shape style <xdr:style>
        // <a:fillRef> schemeClr 兜底（testShapeRotationImport样本核实：
        // spPr 内完全没有 solidFill，真实填充色靠 style fillRef 继承）。
        // 都没有则 fill 为 None（真正无填充的形状）。
        let fill = raw.fill_rgb.or(raw.style_fill_rgb).map(|(r, g, b)| {
            FillStyle::Solid(Color { r, g, b, a: 255 })
        });

        // 段落组装：extract_shapes_from_drawing 已按 <a:p> 边界分组，
        // text_runs 是 Vec<段落 Vec<run>>，每个内层 Vec 组装成一个
        // Paragraph，内含该段落原有的多个 Run（照 OOXML 原始结构还原，
        // 而非此前"每个run独立成段"的拍平简化）。
        //
        // 颜色优先级（testShapeAutofit样本核实确认）：run自身
        // <a:rPr><a:solidFill> > shape style <xdr:style><a:fontRef>
        // schemeClr 兜底 > 黑色兜底。run没写颜色不代表渲染层该给黑色，
        // PowerPoint/Excel 允许颜色完全由 style fontRef 继承。
        let style_font_rgb = raw.style_font_rgb;
        let text_content: Vec<Paragraph> = raw
            .text_runs
            .into_iter()
            .map(|para_runs| {
                let runs: Vec<Run> = para_runs
                    .into_iter()
                    .map(|(text, size_pt, bold, _spc_emu, run_color)| {
                        let (r, g, b) = run_color.or(style_font_rgb).unwrap_or((0, 0, 0));
                        Run {
                            text,
                            font_size: Some(size_pt),
                            bold: Some(bold),
                            color: Some(roe_model::common::Color { r, g, b, a: 255 }),
                            ..Default::default()
                        }
                    })
                    .collect();
                Paragraph {
                    runs,
                    ..Default::default()
                }
            })
            .collect();

        sheet.floating_shapes.push(FloatingShape { stroke: None,
            id: 0,
            x_pt,
            y_pt,
            width_pt,
            height_pt,
            rot,
            flip_v: raw.flip_v,
            geometry,
            fill,
            text_content,
        });
    }
}

/// ROOT-028新增(v129)：读取zip条目为原始二进制（图片等非文本资源专用），
/// 与read_zip_entry（文本/XML专用，内部read_to_string要求合法UTF-8）区分。
/// 模式照抄docx.rs::collect_docx_images里的现成写法。
fn read_zip_entry_binary<R: Read + std::io::Seek>(
    zip: &mut ZipArchive<R>,
    name: &str,
) -> Result<Vec<u8>, ParseError> {
    let mut entry = zip.by_name(name).map_err(|e| {
        ParseError::Malformed(format!("zip entry '{name}' not found: {e}"))
    })?;
    let mut buf = Vec::new();
    entry.read_to_end(&mut buf)?;
    Ok(buf)
}

// ─── styles.xml 解析 ────────────────────────────────────────────────────────

/// 内置 numFmtId → 格式字符串（Excel 预定义，不在文件里）
fn builtin_num_fmt(id: u32) -> Option<&'static str> {
    match id {
        0  => None,              // General
        1  => Some("0"),
        2  => Some("0.00"),
        3  => Some("#,##0"),
        4  => Some("#,##0.00"),
        9  => Some("0%"),
        10 => Some("0.00%"),
        11 => Some("0.00E+00"),
        12 => Some("# ?/?"),
        13 => Some("# ??/??"),
        14 => Some("m/d/yyyy"),
        15 => Some("d-mmm-yy"),
        16 => Some("d-mmm"),
        17 => Some("mmm-yy"),
        18 => Some("h:mm AM/PM"),
        19 => Some("h:mm:ss AM/PM"),
        20 => Some("h:mm"),
        21 => Some("h:mm:ss"),
        22 => Some("m/d/yyyy h:mm"),
        37 => Some("#,##0 ;(#,##0)"),
        38 => Some("#,##0 ;[Red](#,##0)"),
        39 => Some("#,##0.00;(#,##0.00)"),
        40 => Some("#,##0.00;[Red](#,##0.00)"),
        45 => Some("mm:ss"),
        46 => Some("[h]:mm:ss"),
        47 => Some("mmss.0"),
        48 => Some("##0.0E+0"),
        49 => Some("@"),
        _  => None,
    }
}

/// 解析 xl/styles.xml → (xf格式列表, dxf格式列表)
/// xf: 下标=xf index，值=完整 CellFormat（bold/颜色/数字格式等，由 fontId/fillId/numFmtId 反查 fonts[]/fills[] 组装）；
/// dxf: 下标=dxf index，值=CellFormat（含bg/fg色，条件格式专用，字段来源与 xf 不同，逻辑独立）
fn read_styles<R: Read + std::io::Seek>(
    zip: &mut ZipArchive<R>,
    theme_colors: &HashMap<String, Color>,
) -> Result<(Vec<Option<roe_model::calc::CellFormat>>, Vec<roe_model::calc::CellFormat>), ParseError> {
    let xml = match read_zip_entry(zip, "xl/styles.xml") {
        Ok(s) => s,
        Err(_) => return Ok((Vec::new(), Vec::new())),
    };

    let mut reader = Reader::from_str(&xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();

    // numFmtId → 格式字符串（自定义）
    let mut custom_fmts: HashMap<u32, String> = HashMap::new();
    // (fontId, fillId, borderId, numFmtId) 四元组，下标 = xf index
    let mut xf_entries: Vec<(u32, u32, u32, u32)> = Vec::new();
    // ROOT-076: xf_alignments[i] 对应 xf_entries[i] 的 <alignment> 子元素
    // (horizontal, vertical, wrap_text)；None 表示该 xf 没有 <alignment> 子元素。
    let mut xf_alignments: Vec<Option<(Option<String>, Option<String>, bool)>> = Vec::new();
    let mut cur_xf_alignment_idx: Option<usize> = None;
    let mut dxf_list: Vec<roe_model::calc::CellFormat> = Vec::new();
    // ROOT-077: dxf自己的<numFmt formatCode="..">，与xf走的numFmtId反查
    // custom_fmts表是两条独立路径——dxf内联直接给formatCode字符串，不需要
    // 查表。None表示当前dxf没有<numFmt>子元素（用原cellXfs的数字格式）。
    let mut cur_dxf_num_fmt: Option<String> = None;

    // fonts[]：下标 = fontId，(bold, italic, underline, color, font_size_pt)
    // font_size_pt新增（ROOT-082系数统一修复的延伸bug：原来<sz>标签完全
    // 未解析，导致所有单元格font_size统一走12.0兜底默认值，与xlsx实际
    // 记录的字号无关。此前没有暴露是因为渲染路径font_size也全程硬编码
    // 12.0/1.2系数，两处巧合地"错得一致"看不出问题；本次接入LOK公式后
    // 字号才第一次被实际用于计算，才暴露出解析层从未读取过<sz>的缺口。
    // 见roe-ext_progress_v141.md续 tdf123026 line_count=11 vs Excel实际9行
    // 排查过程）。
    // fonts[]：下标 = fontId，(bold, italic, underline, color, font_size_pt, font_family)
    // font_family新增（与font_size同源缺口的后续修复）：此前<name val=".."/>
    // 完全未解析，所有单元格字体统一走渲染层硬编码的"Carlito"兜底，对本次
    // 验证样本（xlsx本身就是Calibri→fontconfig映射Carlito）碰巧蒙对，换成
    // 任何其他字体（Arial/宋体等）的xlsx会全部渲染错误字体。
    let mut fonts: Vec<(bool, bool, bool, Option<String>, Option<f32>, Option<String>)> = Vec::new();
    // fills[]：下标 = fillId，bg_color（patternFill 的 fgColor，Excel 惯例：
    // patternType="solid" 时视觉背景色实际来自 fgColor，不是 bgColor）
    let mut fills: Vec<Option<String>> = Vec::new();
    // borders[]：下标 = borderId，四边全空则记 None
    let mut borders: Vec<Option<roe_model::calc::CellBorders>> = Vec::new();

    // 解析状态
    let mut in_fonts = false;
    let mut in_cell_xfs = false;
    let mut in_dxfs = false;
    let mut in_dxf = false;
    let mut in_fill = false;
    let mut in_pattern_fill = false;
    let mut in_font = false;
    let mut cur_dxf = roe_model::calc::CellFormat::default();
    let mut cur_fg: Option<String> = None;
    let mut cur_font_color: Option<String> = None;

    // fonts/fills 顶层收集（与 dxf 内 font/fill 解析复用同名标志位会冲突，
    // 用独立标志区分「顶层 <fonts><font>」与「<dxf><font>」两种上下文）
    let mut cur_font_bold = false;
    let mut cur_font_italic = false;
    let mut cur_font_underline = false;
    let mut cur_font_plain_color: Option<String> = None;
    let mut cur_font_size: Option<f32> = None;
    let mut cur_font_family: Option<String> = None;
    let mut in_fills = false;
    let mut cur_fill_fg: Option<String> = None;
    let mut in_borders = false;
    let mut in_border = false;
    let mut cur_border_left: Option<roe_model::calc::BorderLine> = None;
    let mut cur_border_right: Option<roe_model::calc::BorderLine> = None;
    let mut cur_border_top: Option<roe_model::calc::BorderLine> = None;
    let mut cur_border_bottom: Option<roe_model::calc::BorderLine> = None;
    // 当前正在解析的边（left/right/top/bottom），用于把随后出现的 <color> 归属到正确的边；
    // 不依赖 End 事件复位——<left style="thin"/> 这类自闭合标签只有 Empty 事件，没有 End
    let mut cur_border_side: &'static str = "";

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Start(e) | Event::Empty(e) => {
                match e.name().as_ref() {
                // ROOT-079修复：加 !in_dxf 守卫。<dxf>内部同样可能出现<numFmt>
                // 自闭合标签（dxf自带内联numFmt，语义与这里的全局<numFmts>表完全
                // 不同——dxf场景直接用formatCode，不登记进custom_fmts全局表）。
                // 若不加此守卫，Rust match按声明顺序匹配、命中即止，这个更早出现
                // 且无条件匹配的分支会"截胡"所有<numFmt>标签，包括dxf内部的，导致
                // 下方 b"numFmt" if in_dxf => 分支永远拿不到执行机会，dxf的
                // num_format字段永远是None（本次expr条件格式验证发现的问题，
                // 与ROOT-078架构本身无关，是更早的ROOT-077引入dxf numFmt支持时
                // 就存在但未被触发到的独立bug——此前的cfvo/其他验证用例恰好都
                // 没有"dxf含numFmt"这个组合，直到本次expression规则验证才暴露）。
                b"numFmt" if !in_dxf => {
                    let mut fmt_id: u32 = 0;
                    let mut fmt_code = String::new();
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"numFmtId"   => { fmt_id = attr.unescape_value()?.parse().unwrap_or(0); }
                            b"formatCode" => { fmt_code = attr.unescape_value()?.into_owned(); }
                            _ => {}
                        }
                    }
                    if !fmt_code.is_empty() { custom_fmts.insert(fmt_id, fmt_code); }
                }
                b"fonts" => { in_fonts = true; }
                b"font" if in_fonts => {
                    in_font = true;
                    cur_font_bold = false;
                    cur_font_italic = false;
                    cur_font_underline = false;
                    cur_font_plain_color = None;
                    // 此前遗漏cur_font_size的reset（font_size补丁引入时的
                    // 疏漏）：若某个<font>条目没有<sz>子标签（理论上xlsx
                    // 规范要求必有，但防御性修正），会残留上一个font条目的
                    // 尺寸值错误传递给当前font，本次一并修正。
                    cur_font_size = None;
                    cur_font_family = None;
                }
                b"b" if in_font && in_fonts => { cur_font_bold = true; }
                b"i" if in_font && in_fonts => { cur_font_italic = true; }
                b"u" if in_font && in_fonts => { cur_font_underline = true; }
                b"sz" if in_font && in_fonts => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"val" {
                            cur_font_size = attr.unescape_value()?.parse::<f32>().ok();
                        }
                    }
                }
                b"name" if in_font && in_fonts => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"val" {
                            cur_font_family = Some(attr.unescape_value()?.into_owned());
                        }
                    }
                }
                b"color" if in_font && in_fonts => {
                    cur_font_plain_color = parse_color_attr_themed(&e, theme_colors);
                }
                b"fills" => { in_fills = true; }
                b"fill" if in_fills => { in_fill = true; cur_fill_fg = None; }
                b"patternFill" if in_fill && in_fills => { in_pattern_fill = true; }
                b"fgColor" if in_pattern_fill && in_fills => {
                    cur_fill_fg = parse_color_attr_themed(&e, theme_colors);
                }
                b"bgColor" if in_pattern_fill && in_fills => {
                    if cur_fill_fg.is_none() {
                        cur_fill_fg = parse_color_attr_themed(&e, theme_colors);
                    }
                }
                b"borders" => { in_borders = true; }
                b"border" if in_borders => {
                    in_border = true;
                    cur_border_left = None;
                    cur_border_right = None;
                    cur_border_top = None;
                    cur_border_bottom = None;
                    cur_border_side = "";
                }
                b"left" | b"right" | b"top" | b"bottom" if in_border && in_borders => {
                    let side = match e.name().as_ref() {
                        b"left" => "left",
                        b"right" => "right",
                        b"top" => "top",
                        _ => "bottom",
                    };
                    cur_border_side = side;
                    let mut style: Option<String> = None;
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"style" {
                            style = Some(attr.unescape_value()?.into_owned());
                        }
                    }
                    // 先按无颜色结算；若随后出现 <color> 子元素，在 color 分支里原地补写颜色。
                    // style 为空（纯 <left/> 无 style 属性）视为"该边无边框"，记 None。
                    let line = style.map(|s| roe_model::calc::BorderLine { style: s, color: None });
                    match side {
                        "left" => cur_border_left = line,
                        "right" => cur_border_right = line,
                        "top" => cur_border_top = line,
                        _ => cur_border_bottom = line,
                    }
                }
                b"color" if in_border && in_borders && !cur_border_side.is_empty() => {
                    let c = parse_color_attr_themed(&e, theme_colors);
                    let target = match cur_border_side {
                        "left" => &mut cur_border_left,
                        "right" => &mut cur_border_right,
                        "top" => &mut cur_border_top,
                        _ => &mut cur_border_bottom,
                    };
                    if let Some(line) = target {
                        line.color = c;
                    }
                }
                b"cellXfs" => { in_cell_xfs = true; }
                b"xf" if in_cell_xfs => {
                    let mut num_fmt_id: u32 = 0;
                    let mut font_id: u32 = 0;
                    let mut fill_id: u32 = 0;
                    let mut border_id: u32 = 0;
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"numFmtId" => { num_fmt_id = attr.unescape_value()?.parse().unwrap_or(0); }
                            b"fontId"   => { font_id = attr.unescape_value()?.parse().unwrap_or(0); }
                            b"fillId"   => { fill_id = attr.unescape_value()?.parse().unwrap_or(0); }
                            b"borderId" => { border_id = attr.unescape_value()?.parse().unwrap_or(0); }
                            _ => {}
                        }
                    }
                    xf_entries.push((font_id, fill_id, border_id, num_fmt_id));
                    // ROOT-076: 标记"当前正在收集哪一条xf_entries的alignment子元素"，
                    // <alignment>是<xf>的子元素而非属性，quick-xml对自闭合<xf .../>
                    // 触发Event::Empty（此分支同时处理，与Event::Start合并），对带子
                    // 元素的<xf>...</xf>触发Event::Start，子元素<alignment>随后单独
                    // 触发一次Event::Empty/Start，需要靠这个下标状态关联到刚push的
                    // 那一条xf_entries。
                    cur_xf_alignment_idx = Some(xf_entries.len() - 1);
                }
                b"alignment" if in_cell_xfs && cur_xf_alignment_idx.is_some() => {
                    let mut horizontal: Option<String> = None;
                    let mut vertical: Option<String> = None;
                    let mut wrap_text = false;
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"horizontal" => { horizontal = Some(attr.unescape_value()?.to_string()); }
                            b"vertical"   => { vertical = Some(attr.unescape_value()?.to_string()); }
                            b"wrapText"   => {
                                let v = attr.unescape_value()?;
                                wrap_text = v.as_ref() == "1" || v.as_ref() == "true";
                            }
                            _ => {}
                        }
                    }
                    if let Some(idx) = cur_xf_alignment_idx {
                        while xf_alignments.len() <= idx {
                            xf_alignments.push(None);
                        }
                        xf_alignments[idx] = Some((horizontal, vertical, wrap_text));
                    }
                }
                b"dxfs" => { in_dxfs = true; }
                b"dxf" if in_dxfs => {
                    in_dxf = true;
                    cur_dxf = roe_model::calc::CellFormat::default();
                    cur_fg = None;
                    cur_font_color = None;
                    cur_dxf_num_fmt = None;
                }
                b"fill" if in_dxf => { in_fill = true; }
                b"patternFill" if in_fill && in_dxf => { in_pattern_fill = true; }
                b"fgColor" if in_pattern_fill && in_dxf => {
                    cur_fg = parse_color_attr_themed(&e, theme_colors);
                }
                b"bgColor" if in_pattern_fill && in_dxf => {
                    // bgColor 仅在 fgColor 不存在时作为背景色
                    if cur_fg.is_none() {
                        cur_fg = parse_color_attr_themed(&e, theme_colors);
                    }
                }
                b"font" if in_dxf => { in_font = true; }
                b"color" if in_font && in_dxf => {
                    cur_font_color = parse_color_attr_themed(&e, theme_colors);
                }
                // ROOT-077: dxf内联<numFmt formatCode="..">，直接取formatCode属性，
                // 不查custom_fmts表（那张表是给xf/cellXfs的numFmtId反查用的，dxf
                // 自己内联给出完整格式字符串，语义更直接）。
                b"numFmt" if in_dxf => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"formatCode" {
                            let code = attr.unescape_value()?.into_owned();
                            if !code.is_empty() {
                                cur_dxf_num_fmt = Some(code);
                            }
                        }
                    }
                }
                _ => {}
            }},
            Event::End(e) => match e.name().as_ref() {
                b"cellXfs" => { in_cell_xfs = false; }
                b"dxfs"    => { in_dxfs = false; }
                b"dxf" if in_dxfs || in_dxf => {
                    cur_dxf.bg_color = cur_fg.take();
                    cur_dxf.fg_color = cur_font_color.take();
                    cur_dxf.num_format = cur_dxf_num_fmt.take();
                    dxf_list.push(cur_dxf.clone());
                    in_dxf = false;
                }
                b"font" if in_fonts && in_font => {
                    fonts.push((cur_font_bold, cur_font_italic, cur_font_underline, cur_font_plain_color.take(), cur_font_size.take(), cur_font_family.take()));
                    in_font = false;
                }
                b"fonts" => { in_fonts = false; }
                b"fill" if in_fills && in_fill => {
                    fills.push(cur_fill_fg.take());
                    in_fill = false;
                }
                b"fills" => { in_fills = false; }
                b"border" if in_borders && in_border => {
                    let cb = roe_model::calc::CellBorders {
                        left: cur_border_left.take(),
                        right: cur_border_right.take(),
                        top: cur_border_top.take(),
                        bottom: cur_border_bottom.take(),
                    };
                    let is_empty = cb.left.is_none() && cb.right.is_none() && cb.top.is_none() && cb.bottom.is_none();
                    borders.push(if is_empty { None } else { Some(cb) });
                    in_border = false;
                    cur_border_side = "";
                }
                b"borders" => { in_borders = false; }
                b"fill"        => { in_fill = false; }
                b"patternFill" => { in_pattern_fill = false; }
                b"font" if in_dxf => { in_font = false; }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    // 反查 numFmtId → 格式字符串（自定义优先，其次内置表）
    let resolve_num_fmt = |id: u32| -> Option<String> {
        if id == 0 { return None; }
        if let Some(s) = custom_fmts.get(&id) { return Some(s.clone()); }
        builtin_num_fmt(id).map(|s| s.to_string())
    };

    // 用 (fontId, fillId, numFmtId) 三元组反查 fonts[]/fills[]，组装完整 CellFormat。
    // fillId 0/1 是 OOXML 规范强制占位（none/gray125），不代表用户设置了背景色，
    // 与 xlsx_styles_writer.rs::intern_xf 里 `fill_id = fill_local_id + 2` 的编号约定对称。
    let xf_result: Vec<Option<roe_model::calc::CellFormat>> = xf_entries
        .iter()
        .enumerate()
        .map(|(xf_idx, &(font_id, fill_id, border_id, num_fmt_id))| {
            let font = fonts.get(font_id as usize);
            // ROOT-076: 取该 xf 对应的 <alignment> 解析结果（可能没有）
            let (align_h, align_v, wrap_text) = match xf_alignments.get(xf_idx).cloned().flatten() {
                Some((h, v, w)) => (h, v, if w { Some(true) } else { None }),
                None => (None, None, None),
            };
            // fills 数组按 XML 出现顺序收集，含 2 个规范占位 fill（下标0/1: none/gray125），
            // fillId 直接对应 fills 数组下标，与写侧 xlsx_styles_writer.rs 的
            // `fill_id = fill_local_id + 2` 编号是同一套坐标系，读侧无需再额外偏移。
            let bg_color = fills.get(fill_id as usize).cloned().flatten();
            let num_format = resolve_num_fmt(num_fmt_id);
            // borders 数组下标 = borderId，与写侧 intern_border 的编号（0=占位空边框，
            // 1..=自定义边框）是同一套坐标系，读侧无需偏移
            let borders = borders.get(border_id as usize).cloned().flatten();

            let (bold, italic, underline, fg_color, font_size, font_family) = match font {
                Some((b, i, u, c, sz, fam)) => (*b, *i, *u, c.clone(), *sz, fam.clone()),
                None => (false, false, false, None, None, None),
            };
            // 默认字体(fontId 0)的黑色等价于"未设置字色"(Excel 里表格样式的字色优先于它)。
            // 不丢弃的话,所有默认 xf 的单元格都带 fg_color=#000000,表格样式的
            // 表头白字/强调色字永远轮不到。显式设置的黑色通常用 fontId!=0,不受影响。
            let fg_color = if font_id == 0
                && fg_color.as_deref().map_or(false, |c| c.eq_ignore_ascii_case("#000000"))
            {
                None
            } else {
                fg_color
            };

            // 全部字段都是"无样式"时返回 None，与旧行为（下标0/无格式返回None）保持一致，
            // 避免给完全没有自定义样式的单元格也徒增一条空 CellFormat 记录。
            // ROOT-082延伸修复：font_size不参与这个"全空判断"——fontId=0
            // （默认字体）在Excel标准模板里也是显式11pt，不该因为"看起来
            // 无样式"就丢弃掉从<sz>解析出来的真实字号，否则又会回退到
            // 渲染层的12.0兜底默认值，重新引入本次要修的bug。
            if !bold && !italic && !underline && fg_color.is_none() && bg_color.is_none() && num_format.is_none() && borders.is_none() && align_h.is_none() && align_v.is_none() && wrap_text.is_none() && font_size.is_none() && font_family.is_none() {
                None
            } else {
                Some(roe_model::calc::CellFormat {
                    bold: if bold { Some(true) } else { None },
                    italic: if italic { Some(true) } else { None },
                    underline: if underline { Some(true) } else { None },
                    font_size,
                    font_family,
                    fg_color,
                    bg_color,
                    align_h,
                    align_v,
                    wrap_text,
                    num_format,
                    borders,
                })
            }
        })
        .collect();

    Ok((xf_result, dxf_list))
}

/// 从 XML 元素属性解析颜色 → "#RRGGBB"
/// 解析 worksheet 的 <extLst> 中 x14:sparklineGroups 扩展块。
/// xml 参数是整张 sheet 的原始 XML 字符串（独立二次读取同一 zip entry，
/// 不复用 read_worksheet 的流式状态机，避免侵入 900+ 行的主循环）。
/// 标签带 x14:/xm: 命名空间前缀，统一用 local_name() 忽略前缀匹配。
/// ROOT-009问题B：Excel 2010+扩展条件格式（beginsWith/endsWith等旧schema
/// 无法表达的规则类型）实际存放在 <extLst><ext><x14:conditionalFormattings>
/// 里，与顶层 <conditionalFormatting><cfRule> 是完全不同的XML结构，标准解析
/// 路径完全读不到。仿照 parse_sparkline_groups_from_xml 的模式：独立二次
/// 扫描同一份 sheet XML 的 <extLst> 片段，用 quick-xml 状态机 + local_name()
/// 忽略命名空间前缀提取规则。
///
/// 已知范围：x14:dxf 是内联完整定义（不是dxfId索引全局dxfs表，实测样本
/// tdf139394.xlsx确认），本次只解析其中的 <bgColor rgb="..."/>（背景色），
/// 复用现有 parse_color_attr。beginsWith/endsWith 用 LibreOffice 导出风格
/// 表达（两个 <xm:f> 标签，第二个是去除引号后的比较值），只处理这两种
/// type，其余 x14 专属类型（如 dataBar 扩展变体等）不在本次范围，静默跳过。
fn parse_x14_cond_formats_from_xml(xml: &str, theme_colors: &HashMap<String, Color>) -> Vec<roe_model::calc::ConditionalFormatRule> {
    use roe_model::calc::{ConditionalFormatRule, ConditionalFormatKind, CellFormat, CfValue};
    use quick_xml::events::Event;

    // ROOT-066(v128)修复：此前用xml.find找"第一个"<extLst>，但legacy
    // <cfRule>内部会各自嵌套一个只含<x14:id>关联ID的小extLst（不含真正
    // 的x14扩展规则内容），出现位置早于文档末尾真正包含
    // <x14:conditionalFormattings>完整定义的那个extLst。从第一个小extLst
    // 位置截取字符串会切断它所属的<cfRule>开始标签，但后续文本里该cfRule
    // 的</cfRule>结束标签仍完整保留，导致quick-xml解析出"孤儿结束标签"
    // IllFormed(UnmatchedEndTag)错误，静默break整个循环，一个事件都没
    // 处理过（本函数原有的Err(_) => break错误处理会吞掉这个错误，只是
    // 表现为"该sheet的x14规则全部丢失"，没有任何报错提示）。
    // 真正需要解析的<x14:conditionalFormattings>块固定位于sheet.xml末尾
    // （紧邻</worksheet>之前），用rfind定位最后一个<extLst>更符合
    // Excel实际写入布局，且能保证截取片段自身闭合完整。
    let idx = match xml.rfind("<extLst") {
        Some(i) => i,
        None => return Vec::new(),
    };
    let mut reader = Reader::from_str(&xml[idx..]);
    reader.config_mut().trim_text(true);

    let mut rules: Vec<ConditionalFormatRule> = Vec::new();

    let mut cur_type = String::new();
    let mut cur_dxf: Option<CellFormat> = None;
    let mut cur_sqref = String::new();
    let mut f_texts: Vec<String> = Vec::new();

    // ROOT-024: x14:iconSet 专用状态。iconSet 属性名 + 每个 x14:cfvo 的
    // (type属性, xm:f文本) 按出现顺序收集，与 f_texts（beginsWith/endsWith
    // 专用）分开，避免 cfvo 数量>1 时互相干扰。
    let mut cur_icon_style = String::new();
    let mut icon_cfvo_types: Vec<String> = Vec::new();
    let mut icon_cfvo_vals: Vec<String> = Vec::new();
    let mut in_x14_iconset = false;
    let mut in_x14_cfvo = false;
    // ROOT-075: x14:iconSet的custom="1"标记 + 逐档<x14:cfIcon
    // iconSet="X" iconId="N"/>覆盖信息，按出现顺序收集（顺序对应档位，
    // 与icon_cfvo_types/icon_cfvo_vals的档位顺序一致）。非custom规则时
    // cur_icon_custom恒为false，cur_icon_cfIcons恒为空，行为与迁移前
    // 完全一致（tier_overrides最终填None）。
    let mut cur_icon_custom = false;
    let mut cur_icon_cfIcons: Vec<(String, u8)> = Vec::new();

    // ROOT-066(v128): x14:dataBar 专用状态。legacy <dataBar> 的 min/max
    // 是占位废值(val="0"/"0")，真实阈值(autoMin/autoMax/percentile/num/
    // formula)、负值颜色、轴线颜色只存在于这个x14扩展块。
    let mut in_x14_databar = false;
    let mut in_x14_databar_cfvo = false;
    let mut cur_databar_direction = String::new();
    let mut cur_databar_show_value = true;
    let mut cur_databar_border: Option<String> = None;
    let mut cur_databar_axis_position = String::new();
    let mut databar_cfvo_types: Vec<String> = Vec::new();
    let mut databar_cfvo_vals: Vec<String> = Vec::new();
    let mut cur_databar_negative_fill: Option<String> = None;
    let mut cur_databar_gradient: bool = true;
    let mut cur_databar_axis_color: Option<String> = None;

    let mut in_cf_rule = false;
    let mut in_dxf = false;
    let mut in_xm_f = false;
    let mut in_xm_sqref = false;
    let mut buf_text = String::new();

    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                match e.local_name().as_ref() {
                    b"cfRule" => {
                        in_cf_rule = true;
                        cur_type.clear();
                        cur_dxf = None;
                        f_texts.clear();
                        cur_icon_style.clear();
                        icon_cfvo_types.clear();
                        icon_cfvo_vals.clear();
                        for attr in e.attributes().flatten() {
                            if attr.key.local_name().as_ref() == b"type" {
                                cur_type = attr.unescape_value().unwrap_or_default().into_owned();
                            }
                        }
                    }
                    b"dxf" if in_cf_rule => {
                        in_dxf = true;
                        cur_dxf = Some(CellFormat::default());
                    }
                    b"bgColor" if in_dxf => {
                        if let Some(fmt) = cur_dxf.as_mut() {
                            fmt.bg_color = parse_color_attr(e);
                        }
                    }
                    // ROOT-024: x14:iconSet iconSet="3Triangles" 等，属性名即图标样式
                    // ROOT-075: custom="1" 标记该规则每档图标由后续的
                    // <x14:cfIcon>逐档指定，不统一用外层iconSet。
                    b"iconSet" if in_cf_rule => {
                        in_x14_iconset = true;
                        cur_icon_custom = false;
                        cur_icon_cfIcons.clear();
                        for attr in e.attributes().flatten() {
                            match attr.key.local_name().as_ref() {
                                b"iconSet" => {
                                    cur_icon_style = attr.unescape_value().unwrap_or_default().into_owned();
                                }
                                b"custom" => {
                                    let v = attr.unescape_value().unwrap_or_default();
                                    cur_icon_custom = v.as_ref() == "1" || v.as_ref().eq_ignore_ascii_case("true");
                                }
                                _ => {}
                            }
                        }
                    }
                    // ROOT-075: <x14:cfIcon iconSet="X" iconId="N"/>，custom混合
                    // 图标集逐档override，自闭合标签，与x14:cfvo(有文本子节点)
                    // 写法不同，属性直接给出档位应显示的图标来源。
                    b"cfIcon" if in_x14_iconset => {
                        let mut set_name = String::new();
                        let mut id_val: u8 = 0;
                        for attr in e.attributes().flatten() {
                            match attr.key.local_name().as_ref() {
                                b"iconSet" => {
                                    set_name = attr.unescape_value().unwrap_or_default().into_owned();
                                }
                                b"iconId" => {
                                    id_val = attr.unescape_value().unwrap_or_default().parse::<u8>().unwrap_or(0);
                                }
                                _ => {}
                            }
                        }
                        if !set_name.is_empty() {
                            cur_icon_cfIcons.push((set_name, id_val));
                        }
                    }
                    // ROOT-066(v128): x14:dataBar，axisPosition在<x14:dataBar>标签
                    // 自身属性上；两个<x14:cfvo type="..." value="..."/>是自闭合标签
                    // （没有子<xm:f>，与iconSet的cfvo写法不同，value直接是属性）。
                    b"dataBar" if in_cf_rule => {
                        in_x14_databar = true;
                        cur_databar_axis_position.clear();
                        databar_cfvo_types.clear();
                        databar_cfvo_vals.clear();
                        cur_databar_negative_fill = None;
                        cur_databar_axis_color = None;
                        // ROOT-070(v130)：x14:dataBar标签自身的gradient属性此前从未
                        // 读取，导致solid_fill字段（ROOT-066第五轮已改为从x14继承，
                        // 因为legacy dataBar根本没有gradient概念）在x14这条路径上
                        // 自身还是写死的占位符true，等于所有x14 DataBar规则恒定
                        // 实心，没有gradient="0"的用例（本该保持默认渐变）被误判
                        // 成实心，是回归。默认值与legacy同源逻辑一致：无该属性时
                        // 视为gradient=true（渐变）。
                        cur_databar_gradient = true;
                        cur_databar_direction.clear();
                        cur_databar_show_value = true;
                        cur_databar_border = None;
                        for attr in e.attributes().flatten() {
                            match attr.key.local_name().as_ref() {
                                b"axisPosition" => {
                                    cur_databar_axis_position = attr.unescape_value().unwrap_or_default().into_owned();
                                }
                                b"gradient" => {
                                    cur_databar_gradient = attr.unescape_value()
                                        .map(|v| v != "0" && v != "false")
                                        .unwrap_or(true);
                                }
                                b"direction" => {
                                    cur_databar_direction = attr.unescape_value().unwrap_or_default().into_owned();
                                }
                                b"showValue" => {
                                    cur_databar_show_value = attr.unescape_value().map(|v| v != "0" && v != "false").unwrap_or(true);
                                }
                                _ => {}
                            }
                        }
                    }
                    b"cfvo" if in_x14_databar => {
                        let mut t = String::new();
                        let mut v = String::new();
                        for attr in e.attributes().flatten() {
                            match attr.key.local_name().as_ref() {
                                b"type" => t = attr.unescape_value().unwrap_or_default().into_owned(),
                                b"val" | b"value" => v = attr.unescape_value().unwrap_or_default().into_owned(),
                                _ => {}
                            }
                        }
                        databar_cfvo_types.push(t);
                        databar_cfvo_vals.push(v);
                        in_x14_databar_cfvo = true;
                    }
                    b"f" if in_x14_databar && in_x14_databar_cfvo => {
                        in_xm_f = true;
                        buf_text.clear();
                    }
                    b"borderColor" if in_x14_databar => {
                        cur_databar_border = parse_color_attr_themed(e, theme_colors);
                    }
                    b"negativeFillColor" if in_x14_databar => {
                        // ROOT-068(v130)：negativeFillColor/axisColor此前只调用
                        // parse_color_attr（只认rgb属性），x14扩展里这两个颜色
                        // 经常写成theme+tint组合（如theme="0" tint="-0.15"），
                        // 之前完全解析不出来、返回None，渲染时又走到写死的红色
                        // 兜底，导致"负值应显示灰色却渲染成红色"。改用
                        // parse_color_attr_themed，rgb优先、没有rgb再尝试theme+tint。
                        cur_databar_negative_fill = parse_color_attr_themed(e, theme_colors);
                    }
                    b"axisColor" if in_x14_databar => {
                        cur_databar_axis_color = parse_color_attr_themed(e, theme_colors);
                    }
                    b"cfvo" if in_x14_iconset => {
                        in_x14_cfvo = true;
                        let mut t = String::new();
                        for attr in e.attributes().flatten() {
                            if attr.key.local_name().as_ref() == b"type" {
                                t = attr.unescape_value().unwrap_or_default().into_owned();
                            }
                        }
                        icon_cfvo_types.push(t);
                    }
                    b"f" if in_x14_cfvo => {
                        in_xm_f = true;
                        buf_text.clear();
                    }
                    b"f" if in_cf_rule => {
                        in_xm_f = true;
                        buf_text.clear();
                    }
                    b"sqref" => {
                        in_xm_sqref = true;
                        buf_text.clear();
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(ref t)) => {
                if in_xm_f || in_xm_sqref {
                    if let Ok(s) = t.unescape() {
                        buf_text.push_str(&s);
                    }
                }
            }
            Ok(Event::End(ref e)) => {
                match e.local_name().as_ref() {
                    b"f" if in_xm_f && in_x14_databar_cfvo => {
                        in_xm_f = false;
                        if let Some(last) = databar_cfvo_vals.last_mut() {
                            *last = buf_text.clone();
                        }
                    }
                    b"cfvo" if in_x14_databar_cfvo => {
                        in_x14_databar_cfvo = false;
                    }
                    b"f" if in_xm_f && in_x14_cfvo => {
                        in_xm_f = false;
                        icon_cfvo_vals.push(buf_text.clone());
                    }
                    b"f" if in_xm_f => {
                        in_xm_f = false;
                        f_texts.push(buf_text.clone());
                    }
                    b"cfvo" if in_x14_cfvo => {
                        in_x14_cfvo = false;
                    }
                    b"iconSet" if in_x14_iconset => {
                        in_x14_iconset = false;
                    }
                    b"dataBar" if in_x14_databar => {
                        in_x14_databar = false;
                    }
                    b"sqref" if in_xm_sqref => {
                        in_xm_sqref = false;
                        cur_sqref = buf_text.clone();
                    }
                    b"dxf" => { in_dxf = false; }
                    b"cfRule" => {
                        in_cf_rule = false;
                    }
                    b"conditionalFormatting" => {
                        // 修复：<xm:sqref> 在 XML 里是 </x14:cfRule> 结束之后、
                        // </x14:conditionalFormatting> 之前才出现的平级兄弟节点，
                        // 不在 cfRule 内部。之前在 cfRule 的 End 事件里结算规则，
                        // 此时 cur_sqref 还是上一条规则残留的值（或首条为空导致
                        // parse_cell_range 失败、整条丢失）。现在挪到
                        // conditionalFormatting 的 End 事件里结算，此时本条的
                        // sqref 已经读到。
                        // f_texts[1]（第二个<xm:f>）是去引号后的比较值，
                        // LibreOffice导出风格：LEFT(A1,LEN("+"))="+" 配合 "+"
                        let condition = match cur_type.as_str() {
                            "beginsWith" => f_texts.get(1).map(|v| {
                                format!("begins_with:{}", v.trim().trim_matches('"'))
                            }),
                            "endsWith" => f_texts.get(1).map(|v| {
                                format!("ends_with:{}", v.trim().trim_matches('"'))
                            }),
                            _ => None,
                        };
                        if let (Some(condition), Some(fmt)) = (condition, cur_dxf.clone()) {
                            for sq in cur_sqref.split_whitespace() {
                                if let Some(range) = parse_cell_range(sq) {
                                    rules.push(ConditionalFormatRule {
                                        range,
                                        kind: ConditionalFormatKind::CellFormat {
                                            condition: condition.clone(),
                                            format: fmt.clone(),
                                        },
                                        stop_if_true: false,
                                    });
                                }
                            }
                        }
                        // ROOT-024: x14扩展iconSet规则（Excel 2010+新增图标集，
                        // 如3Triangles/3Stars/5Boxes，标准<iconSet>枚举无法表达，
                        // 只能走这条x14扩展路径）。thresholds顺序对应
                        // icon_cfvo_types/icon_cfvo_vals收集顺序，percent/reverse/
                        // icons_only暂用默认值（x14:iconSet本身可带这些属性，
                        // 当前样本未使用，后续遇到再补充解析）。
                        // ROOT-066(v128): x14扩展dataBar，携带legacy <dataBar>
                        // 无法表达的真实阈值(autoMin/autoMax/percentile/num/formula)
                        // + negativeFillColor + axisColor + axisPosition。
                        // bar_color/border_color/direction/bar_only/solid_fill
                        // 这里先填占位默认值，调用方（read_worksheet之后的合并步骤）
                        // 会用同range的legacy DataBar规则里的这些字段回填，
                        // 因为x14这个结构里根本不包含正色等信息（正色只在legacy
                        // <dataBar><color>里）。
                        if cur_type == "dataBar" && !databar_cfvo_types.is_empty() {
                            let min = CfValue {
                                kind: databar_cfvo_types.get(0).cloned().unwrap_or_else(|| "min".to_string()),
                                val: databar_cfvo_vals.get(0).cloned().unwrap_or_default(),
                                gte: true,
                            };
                            let max = CfValue {
                                kind: databar_cfvo_types.get(1).cloned().unwrap_or_else(|| "max".to_string()),
                                val: databar_cfvo_vals.get(1).cloned().unwrap_or_default(),
                                gte: true,
                            };
                            let axis_position = if cur_databar_axis_position.is_empty() {
                                "automatic".to_string()
                            } else {
                                cur_databar_axis_position.clone()
                            };
                            for sq in cur_sqref.split_whitespace() {
                                if let Some(range) = parse_cell_range(sq) {
                                    rules.push(ConditionalFormatRule {
                                        range,
                                        kind: ConditionalFormatKind::DataBar {
                                            min: min.clone(),
                                            max: max.clone(),
                                            bar_color: "#638EC6".to_string(), // 占位，调用方会用legacy规则的真实值回填
                                            border_color: cur_databar_border.clone(),
                                            direction: match cur_databar_direction.as_str() {
                                                "leftToRight" => roe_model::calc::BarDirection::LeftToRight,
                                                "rightToLeft" => roe_model::calc::BarDirection::RightToLeft,
                                                _ => roe_model::calc::BarDirection::Auto,
                                            },
                                            bar_only: !cur_databar_show_value,
                                            solid_fill: !cur_databar_gradient,
                                            negative_fill_color: cur_databar_negative_fill.clone(),
                                            axis_color: cur_databar_axis_color.clone(),
                                            axis_position: axis_position.clone(),
                                        },
                                        stop_if_true: false,
                                    });
                                }
                            }
                        }
                        if cur_type == "iconSet" && !cur_icon_style.is_empty() {
                            let thresholds: Vec<CfValue> = icon_cfvo_types.iter().zip(icon_cfvo_vals.iter())
                                .map(|(t, v)| CfValue { kind: t.clone(), val: v.clone(), gte: true })
                                .collect();
                            // ROOT-075: custom="1"且确实收集到<x14:cfIcon>时，才构造
                            // tier_overrides；否则None，保持非custom规则行为不变。
                            // cfIcon顺序与cfvo顺序一一对应，数量不一致（理论不应
                            // 发生，防御性处理）时按较短的一方截断，不panic。
                            let tier_overrides: Option<Vec<Option<(String, u8)>>> =
                                if cur_icon_custom && !cur_icon_cfIcons.is_empty() {
                                    Some(
                                        cur_icon_cfIcons.iter()
                                            .map(|(set, id)| Some((set.clone(), *id)))
                                            .collect()
                                    )
                                } else {
                                    None
                                };
                            if !thresholds.is_empty() {
                                for sq in cur_sqref.split_whitespace() {
                                    if let Some(range) = parse_cell_range(sq) {
                                        rules.push(ConditionalFormatRule {
                                            range,
                                            kind: ConditionalFormatKind::IconSet {
                                                icon_style: cur_icon_style.clone(),
                                                thresholds: thresholds.clone(),
                                                percent: true,
                                                reverse: false,
                                                icons_only: false,
                                                tier_overrides: tier_overrides.clone(),
                                            },
                                            stop_if_true: false,
                                        });
                                    }
                                }
                            }
                        }
                        cur_sqref.clear();
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    rules
}

fn parse_sparkline_groups_from_xml(xml: &str) -> Vec<roe_model::calc::SparklineGroup> {
    use roe_model::calc::{SparklineGroup, Sparkline, SparklineType, DisplayEmptyCellsAs, SparklineAxisType};
    use quick_xml::events::Event;

    let idx = match xml.rfind("<extLst") {
        Some(i) => i,
        None => return Vec::new(),
    };
    let mut reader = Reader::from_str(&xml[idx..]);
    reader.config_mut().trim_text(true);

    let mut groups: Vec<SparklineGroup> = Vec::new();
    let mut cur_group: Option<SparklineGroup> = None;
    let mut in_sparklines = false;
    let mut cur_sqref: Option<String> = None;
    let mut cur_f = String::new();
    let mut in_f = false;
    let mut in_sqref = false;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                match e.local_name().as_ref() {
                    b"sparklineGroup" => {
                        let mut g = SparklineGroup::default();
                        for attr in e.attributes().flatten() {
                            let key = attr.key.local_name();
                            let key = key.as_ref();
                            let val = match attr.unescape_value() { Ok(v) => v.into_owned(), Err(_) => continue };
                            match key {
                                b"type" => {
                                    g.sparkline_type = match val.as_str() {
                                        "column" => SparklineType::Column,
                                        "stacked" => SparklineType::Stacked,
                                        _ => SparklineType::Line,
                                    };
                                }
                                b"lineWeight" => { g.line_weight = val.parse().unwrap_or(0.75); }
                                b"manualMax" => { g.manual_max = val.parse().ok(); }
                                b"manualMin" => { g.manual_min = val.parse().ok(); }
                                b"dateAxis" => { g.date_axis = val == "1" || val == "true"; }
                                b"displayEmptyCellsAs" => {
                                    g.display_empty_cells_as = match val.as_str() {
                                        "span" => DisplayEmptyCellsAs::Span,
                                        "zero" => DisplayEmptyCellsAs::Zero,
                                        _ => DisplayEmptyCellsAs::Gap,
                                    };
                                }
                                b"markers" => { g.markers = val == "1" || val == "true"; }
                                b"high" => { g.high = val == "1" || val == "true"; }
                                b"low" => { g.low = val == "1" || val == "true"; }
                                b"first" => { g.first = val == "1" || val == "true"; }
                                b"last" => { g.last = val == "1" || val == "true"; }
                                b"negative" => { g.negative = val == "1" || val == "true"; }
                                b"displayXAxis" => { g.display_x_axis = val == "1" || val == "true"; }
                                b"displayHidden" => { g.display_hidden = val == "1" || val == "true"; }
                                b"minAxisType" => {
                                    g.min_axis_type = match val.as_str() {
                                        "group" => SparklineAxisType::Group,
                                        "custom" => SparklineAxisType::Custom,
                                        _ => SparklineAxisType::Individual,
                                    };
                                }
                                b"maxAxisType" => {
                                    g.max_axis_type = match val.as_str() {
                                        "group" => SparklineAxisType::Group,
                                        "custom" => SparklineAxisType::Custom,
                                        _ => SparklineAxisType::Individual,
                                    };
                                }
                                b"rightToLeft" => { g.right_to_left = val == "1" || val == "true"; }
                                _ => {}
                            }
                        }
                        cur_group = Some(g);
                    }
                    b"colorSeries"   => { if let Some(g) = cur_group.as_mut() { g.color_series   = parse_color_attr(e); } }
                    b"colorNegative" => { if let Some(g) = cur_group.as_mut() { g.color_negative = parse_color_attr(e); } }
                    b"colorAxis"     => { if let Some(g) = cur_group.as_mut() { g.color_axis     = parse_color_attr(e); } }
                    b"colorMarkers"  => { if let Some(g) = cur_group.as_mut() { g.color_markers  = parse_color_attr(e); } }
                    b"colorFirst"    => { if let Some(g) = cur_group.as_mut() { g.color_first    = parse_color_attr(e); } }
                    b"colorLast"     => { if let Some(g) = cur_group.as_mut() { g.color_last     = parse_color_attr(e); } }
                    b"colorHigh"     => { if let Some(g) = cur_group.as_mut() { g.color_high     = parse_color_attr(e); } }
                    b"colorLow"      => { if let Some(g) = cur_group.as_mut() { g.color_low      = parse_color_attr(e); } }
                    b"sparklines" => { in_sparklines = true; }
                    b"sparkline" if in_sparklines => {
                        cur_sqref = None;
                        cur_f.clear();
                    }
                    b"f" => { in_f = true; cur_f.clear(); }
                    b"sqref" => { in_sqref = true; }
                    _ => {}
                }
            }
            Ok(Event::Text(t)) => {
                if in_f {
                    if let Ok(txt) = t.unescape() { cur_f.push_str(&txt); }
                } else if in_sqref {
                    if let Ok(txt) = t.unescape() { cur_sqref = Some(txt.into_owned()); }
                }
            }
            Ok(Event::End(ref e)) => {
                match e.local_name().as_ref() {
                    b"f" => { in_f = false; }
                    b"sqref" => { in_sqref = false; }
                    b"sparkline" if in_sparklines => {
                        if let Some(g) = cur_group.as_mut() {
                            if let Some(sqref) = cur_sqref.take() {
                                if let Some((row, col)) = parse_cell_ref(&sqref) {
                                    let range = cur_f.rsplit('!').next().unwrap_or(&cur_f);
                                    let input_range = parse_cell_range(range).into_iter().collect();
                                    g.sparklines.push(Sparkline { cell: (row, col), input_range });
                                }
                            }
                        }
                        cur_f.clear();
                    }
                    b"sparklines" => { in_sparklines = false; }
                    b"sparklineGroup" => {
                        if let Some(g) = cur_group.take() { groups.push(g); }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    groups
}

/// 支持 rgb="FFRRGGBB"（含alpha）和 rgb="RRGGBB"（不含alpha）
// ROOT-022/025修复(v133)：xlsx主题色(theme color)解析。
// xlsx条件格式(dataBar/colorScale)的<color>标签除了直接rgb="RRGGBB"外，
// 还大量使用<color theme="N" tint="F"/>引用xl/theme/theme1.xml里的
// 12个主题色槽位，此前代码完全未处理theme/tint属性，导致颜色槽位留空，
// 渲染时回退到硬编码默认色（红/绿），与sc_data_xlsx__cond_format_theme_color2/3
// 等样本文件名直接印证的"主题色"现象吻合。
//
// theme索引→槽位映射经LOK sc/source/filter/oox/stylesbuffer.cxx::
// XlsColor::setTheme()验证，注意0/1是lt1/dk1（不是dk1/lt1，容易搞反）：
// 0=lt1 1=dk1 2=lt2 3=dk2 4=accent1 5=accent2 6=accent3
// 7=accent4 8=accent5 9=accent6 10=hlink 11=folHlink
const XLSX_THEME_SLOT_ORDER: [&str; 12] = [
    "lt1", "dk1", "lt2", "dk2",
    "accent1", "accent2", "accent3", "accent4", "accent5", "accent6",
    "hlink", "folHlink",
];

/// 读取 xl/theme/theme1.xml，提取12个主题色槽位的RGB值。
/// 找不到文件或解析失败时返回Office默认主题色（照抄OnlyOffice
/// FillWithDefaults同款默认值，与roe-parser/src/ooxml/pptx.rs的
/// parse_theme_colors()默认值表保持一致）。
fn read_xlsx_theme_colors<R: Read + std::io::Seek>(
    zip: &mut ZipArchive<R>,
) -> HashMap<String, Color> {
    match read_zip_entry(zip, "xl/theme/theme1.xml") {
        Ok(xml) => parse_ooxml_theme_colors(&xml),
        Err(_) => parse_ooxml_theme_colors(""),
    }
}

/// 解析6位十六进制颜色字符串（不带#前缀）→ [u8;3]，失败返回黑色
fn parse_rgb_hex_3(s: &str) -> [u8; 3] {
    let s = s.trim();
    if s.len() >= 6 {
        let r = u8::from_str_radix(&s[0..2], 16).unwrap_or(0);
        let g = u8::from_str_radix(&s[2..4], 16).unwrap_or(0);
        let b = u8::from_str_radix(&s[4..6], 16).unwrap_or(0);
        [r, g, b]
    } else {
        [0, 0, 0]
    }
}

/// RGB([u8;3], 0-255) → HSL(h: 0-360, s: 0-1, l: 0-1)
fn rgb_to_hsl(rgb: [u8; 3]) -> (f64, f64, f64) {
    let r = rgb[0] as f64 / 255.0;
    let g = rgb[1] as f64 / 255.0;
    let b = rgb[2] as f64 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if (max - min).abs() < 1e-9 {
        return (0.0, 0.0, l);
    }
    let d = max - min;
    let s = if l > 0.5 { d / (2.0 - max - min) } else { d / (max + min) };
    let h = if (max - r).abs() < 1e-9 {
        ((g - b) / d) % 6.0 + if g < b { 6.0 } else { 0.0 }
    } else if (max - g).abs() < 1e-9 {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    let h = h * 60.0;
    let h = if h < 0.0 { h + 360.0 } else { h };
    (h, s, l)
}

/// HSL(h: 0-360, s: 0-1, l: 0-1) → RGB([u8;3], 0-255)
fn hsl_to_rgb(h: f64, s: f64, l: f64) -> [u8; 3] {
    if s.abs() < 1e-9 {
        let v = (l.clamp(0.0, 1.0) * 255.0).round() as u8;
        return [v, v, v];
    }
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r1, g1, b1) = if hp < 1.0 { (c, x, 0.0) }
        else if hp < 2.0 { (x, c, 0.0) }
        else if hp < 3.0 { (0.0, c, x) }
        else if hp < 4.0 { (0.0, x, c) }
        else if hp < 5.0 { (x, 0.0, c) }
        else { (c, 0.0, x) };
    let m = l - c / 2.0;
    let r = ((r1 + m).clamp(0.0, 1.0) * 255.0).round() as u8;
    let g = ((g1 + m).clamp(0.0, 1.0) * 255.0).round() as u8;
    let b = ((b1 + m).clamp(0.0, 1.0) * 255.0).round() as u8;
    [r, g, b]
}

/// ROOT-022/025修复(v133)：将theme索引+tint换算成"#RRGGBB"字符串。
/// 算法照抄LOK oox/source/drawingml/color.cxx::Color::addExcelTintTransformation()
/// + HSL模式下lumMod/lumOff的应用逻辑（Color::getFinalColor路径的HSL分支），
/// 不是简单的RGB线性插值：
///   tint > 0（变亮）：lumMod = 1-tint，lumOff = tint，newLum = oldLum*lumMod + lumOff
///   tint < 0（变暗）：lumMod = 1-|tint|，lumOff = 0，newLum = oldLum*lumMod
///   tint == 0：颜色不变
fn resolve_theme_color(theme_idx: u32, tint: f64, theme_colors: &HashMap<String, Color>) -> Option<String> {
    let slot = XLSX_THEME_SLOT_ORDER.get(theme_idx as usize)?;
    let c = *theme_colors.get(*slot)?;
    let base = [c.r, c.g, c.b];
    if tint.abs() < 1e-9 {
        return Some(format!("#{:02X}{:02X}{:02X}", base[0], base[1], base[2]));
    }
    let (h, s, l) = rgb_to_hsl(base);
    let abs_tint = tint.abs().clamp(0.0, 1.0);
    let new_l = if tint > 0.0 {
        l * (1.0 - abs_tint) + abs_tint
    } else {
        l * (1.0 - abs_tint)
    };
    let rgb = hsl_to_rgb(h, s, new_l.clamp(0.0, 1.0));
    Some(format!("#{:02X}{:02X}{:02X}", rgb[0], rgb[1], rgb[2]))
}

fn parse_color_attr(e: &quick_xml::events::BytesStart) -> Option<String> {
    for attr in e.attributes().flatten() {
        if attr.key.as_ref() == b"rgb" {
            let val = attr.unescape_value().ok()?;
            let s = val.trim();
            return if s.len() == 8 {
                // AARRGGBB → #RRGGBB
                Some(format!("#{}", &s[2..]))
            } else if s.len() == 6 {
                Some(format!("#{}", s))
            } else {
                None
            };
        }
    }
    None
}

/// parse_color_attr 的主题色感知版本（ROOT CalcThemeTest修复）。
/// styles.xml 的 <color>/<fgColor>/<bgColor> 除了直接给 rgb 属性外，还大量
/// 使用 theme="N" tint="F" 引用 xl/theme/theme1.xml 里的12个主题色槽位
/// （与ROOT-022/025修复(v133)条件格式dataBar/colorScale场景的theme+tint
/// 解析同源，逻辑照抄第841行附近已验证过的分支，这里复用同一个
/// resolve_theme_color 辅助函数，只是调用方从条件格式换成了普通单元格样式）。
/// rgb 属性存在时优先用 rgb（不变），不存在时才尝试 theme+tint，都没有则 None。
fn parse_color_attr_themed(
    e: &quick_xml::events::BytesStart,
    theme_colors: &HashMap<String, Color>,
) -> Option<String> {
    if let Some(hex) = parse_color_attr(e) {
        return Some(hex);
    }
    let mut theme_idx: Option<u32> = None;
    let mut tint: f64 = 0.0;
    for attr in e.attributes().flatten() {
        match attr.key.as_ref() {
            b"theme" => {
                theme_idx = attr.unescape_value().ok().and_then(|v| v.parse::<u32>().ok());
            }
            b"tint" => {
                tint = attr.unescape_value().ok().and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
            }
            _ => {}
        }
    }
    let idx = theme_idx?;
    resolve_theme_color(idx, tint, theme_colors)
}

#[cfg(test)]
mod test_a2a {
    use super::*;

    #[test]
    fn test_parse_merge_ref() {
        let mc = parse_merge_ref("A1:C3").unwrap();
        assert_eq!(mc.r1, 0); assert_eq!(mc.c1, 0);
        assert_eq!(mc.r2, 2); assert_eq!(mc.c2, 2);

        let mc2 = parse_merge_ref("B2:D4").unwrap();
        assert_eq!(mc2.r1, 1); assert_eq!(mc2.c1, 1);
        assert_eq!(mc2.r2, 3); assert_eq!(mc2.c2, 3);

        assert!(parse_merge_ref("A1").is_none()); // 无冒号
        println!("parse_merge_ref ✓");
    }

    #[test]
    fn test_xlsx_merge_cells_parsed() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData>
    <row r="1"><c r="A1" t="inlineStr"><is><t>merged</t></is></c></row>
  </sheetData>
  <mergeCells count="1">
    <mergeCell ref="A1:C2"/>
  </mergeCells>
</worksheet>"#;
        // 直接测 parse_merge_ref，worksheet 解析需要完整 zip
        let mc = parse_merge_ref("A1:C2").unwrap();
        assert_eq!(mc.r1, 0); assert_eq!(mc.c1, 0);
        assert_eq!(mc.r2, 1); assert_eq!(mc.c2, 2);
        println!("xlsx merge cells ✓");
    }
}

#[cfg(test)]
mod test_sparkline {
    use super::*;

    #[test]
    fn test_parse_sparklines_real_file() {
        let data = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../test_data/Sparklines.xlsx"))
            .expect("test_data/Sparklines.xlsx should exist");
        let wb = XlsxParser::parse(&data).expect("parse should succeed");

        assert_eq!(wb.sheets.len(), 3);

        // Sheet1: 2 groups (line + column), 1 sparkline each
        let sheet1 = &wb.sheets[0];
        assert_eq!(sheet1.sparkline_groups.len(), 2, "sheet1 should have 2 sparkline groups");

        let g0 = &sheet1.sparkline_groups[0];
        assert_eq!(g0.sparkline_type, roe_model::calc::SparklineType::Line);
        assert!((g0.line_weight - 1.0).abs() < 1e-6);
        assert!(g0.markers);
        assert!(g0.high && g0.low && g0.first && g0.last && g0.negative);
        assert!(g0.display_x_axis);
        assert_eq!(g0.color_series.as_deref(), Some("#376092"));
        assert_eq!(g0.color_negative.as_deref(), Some("#00B050"));
        assert_eq!(g0.color_first.as_deref(), Some("#7030A0"));
        assert_eq!(g0.sparklines.len(), 1);
        assert_eq!(g0.sparklines[0].cell, (1, 0)); // A2 → row=1,col=0
        assert_eq!(g0.sparklines[0].input_range.len(), 1);

        let g1 = &sheet1.sparkline_groups[1];
        assert_eq!(g1.sparkline_type, roe_model::calc::SparklineType::Column);
        assert!(!g1.markers); // markers 属性未出现，应为 false
        assert_eq!(g1.sparklines[0].cell, (2, 0)); // A3 → row=2,col=0
        eprintln!("[COLOR-DEBUG] g1 (Column) colors: series={:?} negative={:?} axis={:?} markers={:?} first={:?} last={:?} high={:?} low={:?}",
            g1.color_series, g1.color_negative, g1.color_axis, g1.color_markers,
            g1.color_first, g1.color_last, g1.color_high, g1.color_low);
        eprintln!("[COLOR-DEBUG] g0 (Line) colors: series={:?} negative={:?} axis={:?} markers={:?} first={:?} last={:?} high={:?} low={:?}",
            g0.color_series, g0.color_negative, g0.color_axis, g0.color_markers,
            g0.color_first, g0.color_last, g0.color_high, g0.color_low);
        eprintln!("[COLOR-DEBUG] g1 (Column) colors: series={:?} negative={:?} axis={:?} markers={:?} first={:?} last={:?} high={:?} low={:?}",
            g1.color_series, g1.color_negative, g1.color_axis, g1.color_markers,
            g1.color_first, g1.color_last, g1.color_high, g1.color_low);
        eprintln!("[COLOR-DEBUG] g0 (Line) colors: series={:?} negative={:?} axis={:?} markers={:?} first={:?} last={:?} high={:?} low={:?}",
            g0.color_series, g0.color_negative, g0.color_axis, g0.color_markers,
            g0.color_first, g0.color_last, g0.color_high, g0.color_low);

        // Sheet3: stacked(win-loss) 类型 + 一个 group 内多条 sparkline
        let sheet3 = &wb.sheets[2];
        assert!(sheet3.sparkline_groups.len() >= 2, "sheet3 should have multiple groups");
        let stacked_group = sheet3.sparkline_groups.iter()
            .find(|g| g.sparkline_type == roe_model::calc::SparklineType::Stacked)
            .expect("should find a stacked-type group");
        assert_eq!(stacked_group.sparklines.len(), 10, "stacked group should have 10 sparklines (N1..N10)");

        println!("sparkline real file parse ✓ sheet1={} groups, sheet3 stacked has {} sparklines",
            sheet1.sparkline_groups.len(), stacked_group.sparklines.len());
    }
}

#[cfg(test)]
mod test_a2b {
    use super::*;

    #[test]
    fn test_builtin_num_fmt() {
        assert_eq!(builtin_num_fmt(0), None);          // General
        assert_eq!(builtin_num_fmt(2), Some("0.00"));
        assert_eq!(builtin_num_fmt(14), Some("m/d/yyyy"));
        assert_eq!(builtin_num_fmt(9), Some("0%"));
        assert_eq!(builtin_num_fmt(49), Some("@"));
        println!("builtin_num_fmt ✓");
    }

    #[test]
    fn test_read_styles_custom_fmt() {
        // 模拟一个最小 styles.xml：一个自定义格式 + cellXfs
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <numFmts count="1">
    <numFmt numFmtId="164" formatCode="yyyy-mm-dd"/>
  </numFmts>
  <cellXfs count="3">
    <xf numFmtId="0"/>
    <xf numFmtId="14"/>
    <xf numFmtId="164"/>
  </cellXfs>
</styleSheet>"#;

        // 用内存 zip 包装测试
        use std::io::{Cursor, Write};
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(Cursor::new(&mut buf));
            let opts = zip::write::FileOptions::<()>::default();
            zip.start_file("xl/styles.xml", opts).unwrap();
            zip.write_all(xml.as_bytes()).unwrap();
            zip.finish().unwrap();
        }
        let mut zip = zip::ZipArchive::new(Cursor::new(&buf)).unwrap();
        let (fmts, _dxfs) = read_styles(&mut zip, &HashMap::new()).unwrap();

        assert_eq!(fmts.len(), 3);
        assert!(fmts[0].is_none());                          // numFmtId=0 → General，无样式
        assert_eq!(
            fmts[1].as_ref().and_then(|f| f.num_format.clone()),
            Some("m/d/yyyy".to_string())
        ); // numFmtId=14 内置
        assert_eq!(
            fmts[2].as_ref().and_then(|f| f.num_format.clone()),
            Some("yyyy-mm-dd".to_string())
        ); // numFmtId=164 自定义
        println!("read_styles ✓");
    }

    #[test]
    fn test_read_worksheet_conditional_format_databar_colorscale_iconset() {
        // 覆盖 dataBar / colorScale(2色+3色都测) / iconSet 三种新增解析路径
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
 <sheetData>
  <row r="1">
   <c r="A1"><v>1</v></c>
   <c r="B1"><v>2</v></c>
   <c r="C1"><v>3</v></c>
  </row>
 </sheetData>
 <conditionalFormatting sqref="A1">
  <cfRule type="dataBar" priority="1">
   <dataBar>
    <cfvo type="min"/>
    <cfvo type="max"/>
    <color rgb="FF638EC6"/>
   </dataBar>
  </cfRule>
 </conditionalFormatting>
 <conditionalFormatting sqref="B1">
  <cfRule type="colorScale" priority="2">
   <colorScale>
    <cfvo type="min"/>
    <cfvo type="percentile" val="50"/>
    <cfvo type="max"/>
    <color rgb="FFF8696B"/>
    <color rgb="FFFFEB84"/>
    <color rgb="FF63BE7B"/>
   </colorScale>
  </cfRule>
 </conditionalFormatting>
 <conditionalFormatting sqref="C1">
  <cfRule type="iconSet" priority="3">
   <iconSet iconSet="3TrafficLights1" reverse="1" showValue="0">
    <cfvo type="percent" val="0"/>
    <cfvo type="percent" val="33"/>
    <cfvo type="percent" val="67"/>
   </iconSet>
  </cfRule>
 </conditionalFormatting>
</worksheet>"#;

        use std::io::{Cursor, Write};
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(Cursor::new(&mut buf));
            let opts = zip::write::FileOptions::<()>::default();
            zip.start_file("xl/worksheets/sheet1.xml", opts).unwrap();
            zip.write_all(xml.as_bytes()).unwrap();
            zip.finish().unwrap();
        }
        let mut zip = zip::ZipArchive::new(Cursor::new(&buf)).unwrap();
        let sst: Vec<CellValue> = Vec::new();
        let style_formats: Vec<Option<roe_model::calc::CellFormat>> = Vec::new();
        let dxf_formats: Vec<roe_model::calc::CellFormat> = Vec::new();
        let sheet = read_worksheet(&mut zip, "xl/worksheets/sheet1.xml", "Sheet1", &sst, &style_formats, &dxf_formats, &HashMap::new()).unwrap();

        assert_eq!(sheet.cond_formats.len(), 3, "应解析出3条条件格式规则");

        // dataBar
        match &sheet.cond_formats[0].kind {
            roe_model::calc::ConditionalFormatKind::DataBar { min, max, bar_color, .. } => {
                assert_eq!(min.kind, "min");
                assert_eq!(max.kind, "max");
                assert_eq!(bar_color, "#638EC6");
            }
            other => panic!("expected DataBar, got {:?}", other),
        }

        // colorScale (3色)
        match &sheet.cond_formats[1].kind {
            roe_model::calc::ConditionalFormatKind::ColorScale { min_type, min_color, mid, max_type, max_color, .. } => {
                assert_eq!(min_type, "min");
                assert_eq!(min_color, "#F8696B");
                assert_eq!(max_type, "max");
                assert_eq!(max_color, "#63BE7B");
                let (mid_type, mid_val, mid_color) = mid.as_ref().expect("3色应有mid");
                assert_eq!(mid_type, "percentile");
                assert_eq!(mid_val, "50");
                assert_eq!(mid_color, "#FFEB84");
            }
            other => panic!("expected ColorScale, got {:?}", other),
        }

        // iconSet
        match &sheet.cond_formats[2].kind {
            roe_model::calc::ConditionalFormatKind::IconSet { icon_style, thresholds, percent, reverse, icons_only, .. } => {
                assert_eq!(icon_style, "3TrafficLights1");
                assert_eq!(thresholds.len(), 3);
                assert!(*percent);
                assert!(*reverse);
                assert!(*icons_only);
            }
            other => panic!("expected IconSet, got {:?}", other),
        }

        println!("cf databar/colorscale/iconset parse ✓");
    }

    #[test]
    fn test_read_worksheet_data_validation_list_and_whole() {
        // 覆盖 list（下拉列表）+ whole（整数范围 between）两种最常见类型
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
 <sheetData>
  <row r="1">
   <c r="A1"><v>1</v></c>
   <c r="B1"><v>2</v></c>
  </row>
 </sheetData>
 <dataValidations count="2">
  <dataValidation type="list" allowBlank="1" showErrorMessage="1" sqref="A1:A5">
   <formula1>"苹果,香蕉,橙子"</formula1>
  </dataValidation>
  <dataValidation type="whole" operator="between" allowBlank="0" showErrorMessage="1" error="必须在1到100之间" sqref="B1:B10">
   <formula1>1</formula1>
   <formula2>100</formula2>
  </dataValidation>
 </dataValidations>
</worksheet>"#;

        use std::io::{Cursor, Write};
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(Cursor::new(&mut buf));
            let opts = zip::write::FileOptions::<()>::default();
            zip.start_file("xl/worksheets/sheet1.xml", opts).unwrap();
            zip.write_all(xml.as_bytes()).unwrap();
            zip.finish().unwrap();
        }
        let mut zip = zip::ZipArchive::new(Cursor::new(&buf)).unwrap();
        let sst: Vec<CellValue> = Vec::new();
        let style_formats: Vec<Option<roe_model::calc::CellFormat>> = Vec::new();
        let dxf_formats: Vec<roe_model::calc::CellFormat> = Vec::new();
        let sheet = read_worksheet(&mut zip, "xl/worksheets/sheet1.xml", "Sheet1", &sst, &style_formats, &dxf_formats, &HashMap::new()).unwrap();

        assert_eq!(sheet.data_validations.len(), 2, "应解析出2条数据验证规则");

        let dv0 = &sheet.data_validations[0];
        assert_eq!(dv0.kind, "list");
        assert_eq!(dv0.operator, "");
        assert_eq!(dv0.formula1, r#""苹果,香蕉,橙子""#);
        assert_eq!(dv0.formula2, None);
        assert!(dv0.allow_blank);
        assert!(dv0.show_error);
        assert_eq!(dv0.range.start, (0, 0)); // A1
        assert_eq!(dv0.range.end, (4, 0));   // A5

        let dv1 = &sheet.data_validations[1];
        assert_eq!(dv1.kind, "whole");
        assert_eq!(dv1.operator, "between");
        assert_eq!(dv1.formula1, "1");
        assert_eq!(dv1.formula2, Some("100".to_string()));
        assert!(!dv1.allow_blank);
        assert!(dv1.show_error);
        assert_eq!(dv1.error_msg, Some("必须在1到100之间".to_string()));
        assert_eq!(dv1.range.start, (0, 1)); // B1
        assert_eq!(dv1.range.end, (9, 1));   // B10

        println!("data validation list/whole parse ✓");
    }

    #[test]
    fn test_parse_defined_name_ref_rejects_non_plain_ranges() {
        // 纯区域:接受
        assert!(parse_defined_name_ref("Sheet1!$A$1").is_some());
        assert!(parse_defined_name_ref("Sheet1!$A$1:$B$10").is_some());
        let r = parse_defined_name_ref("'It''s'!$C$1").unwrap();
        assert_eq!(r.sheet, "It's");
        // 非纯区域:一律 None,走 named_formulas 原样保留
        assert!(parse_defined_name_ref("OFFSET([1]Tabelle1!$G$3,0,0,COUNTA([1]Tabelle1!$G:$G)-1)").is_none());
        assert!(parse_defined_name_ref("Sheet1!$A:$A").is_none());
        assert!(parse_defined_name_ref("Sheet1!$1:$1").is_none());
        assert!(parse_defined_name_ref("Munka1!$A:$A,Munka1!$1:$1").is_none());
        assert!(parse_defined_name_ref("[1]Sheet1!$A$1").is_none());
        assert!(parse_defined_name_ref("Sheet1!$B$10:$A$1").is_none());
        assert!(parse_defined_name_ref("{1,2,3;4,5,6}").is_none());
    }

    #[test]
    fn test_read_defined_names_basic_and_cross_sheet() {
        // 覆盖：普通命名区域 + 带空格的 sheet 名（单引号包裹）+ 单格命名区域（非 range）。
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheets>
    <sheet name="Sheet1" sheetId="1" r:id="rId1"/>
  </sheets>
  <definedNames>
    <definedName name="SalesTotal">Sheet1!$A$1:$B$10</definedName>
    <definedName name="TaxRate">'My Sheet'!$C$1</definedName>
  </definedNames>
</workbook>"#;
        use std::io::{Cursor, Write};
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(Cursor::new(&mut buf));
            let opts = zip::write::FileOptions::<()>::default();
            zip.start_file("xl/workbook.xml", opts).unwrap();
            zip.write_all(xml.as_bytes()).unwrap();
            zip.finish().unwrap();
        }
        let mut zip = zip::ZipArchive::new(Cursor::new(&buf)).unwrap();
        let (named_ranges, _named_formulas) = read_defined_names(&mut zip).unwrap();

        assert_eq!(named_ranges.len(), 2);

        let sales = named_ranges.get("SalesTotal").expect("SalesTotal should be parsed");
        assert_eq!(sales.sheet, "Sheet1");
        assert_eq!(sales.start, (0, 0)); // A1 -> row0 col0
        assert_eq!(sales.end, (9, 1));   // B10 -> row9 col1

        let tax = named_ranges.get("TaxRate").expect("TaxRate should be parsed");
        assert_eq!(tax.sheet, "My Sheet", "带空格的 sheet 名应正确去掉单引号");
        assert_eq!(tax.start, (0, 2)); // C1 -> row0 col2
        assert_eq!(tax.end, (0, 2));   // 单格命名区域 start==end

        println!("read_defined_names ✓");
    }
}


#[cfg(test)]
mod chartex_cache_tests {
    use super::*;

    const STAT_XML: &str = r##"<?xml version="1.0"?><cx:chartSpace xmlns:cx="http://schemas.microsoft.com/office/drawing/2014/chartex"><cx:chartData><cx:data id="0"><cx:strDim type="cat"><cx:f>_xlchart.v1.0</cx:f></cx:strDim><cx:numDim type="val"><cx:f>_xlchart.v1.1</cx:f></cx:numDim></cx:data></cx:chartData><cx:chart><cx:plotArea><cx:plotAreaRegion><cx:series layoutId="funnel" uniqueId="{a}" formatIdx="0"><cx:dataId val="0"/></cx:series></cx:plotAreaRegion></cx:plotArea></cx:chart></cx:chartSpace>"##;

    #[test]
    fn chartex_cache_injects_from_defined_names() {
        let mut named = HashMap::new();
        named.insert("_xlchart.v1.0".to_string(), parse_defined_name_ref("Sheet1!$A$2:$A$6").unwrap());
        named.insert("_xlchart.v1.1".to_string(), parse_defined_name_ref("Sheet1!$B$2:$B$6").unwrap());
        let get = |sn: &str, r: u32, c: u32| -> CellValue {
            assert_eq!(sn, "Sheet1");
            if c == 0 { CellValue::Text(format!("r{r}")) } else { CellValue::Number(10.0 + r as f64) }
        };
        let filled = inject_chartex_cache(STAT_XML, &named, &get);
        let m = crate::ooxml::chartex::parse_chartex_xml(&filled).expect("parse");
        assert_eq!(m.data[0].categories[0], vec!["r1", "r2", "r3", "r4", "r5"]);
        let vals: Vec<f64> = m.data[0].values.iter().map(|v| v.unwrap()).collect();
        assert_eq!(vals, vec![11.0, 12.0, 13.0, 14.0, 15.0]);
    }

    #[test]
    fn chartex_cache_keeps_existing_lvl() {
        let xml = r##"<cx:data id="0"><cx:strDim type="cat"><cx:f>X</cx:f><cx:lvl ptCount="1"><cx:pt idx="0">a</cx:pt></cx:lvl></cx:strDim></cx:data>"##;
        let get = |_: &str, _: u32, _: u32| -> CellValue { panic!("不应取值") };
        assert_eq!(inject_chartex_cache(xml, &HashMap::new(), &get), xml);
    }

    #[test]
    fn strip_fallback_removes_wps_placeholder_only_for_chartex() {
        let drawing = r##"<xdr:twoCellAnchor><mc:AlternateContent xmlns:mc="m"><mc:Choice Requires="cx1"><xdr:graphicFrame><a:graphic><a:graphicData uri="http://schemas.microsoft.com/office/drawing/2014/chartex"><cx:chart r:id="rId1"/></a:graphicData></a:graphic></xdr:graphicFrame></mc:Choice><mc:Fallback><xdr:sp><a:t>This chart is not available</a:t></xdr:sp></mc:Fallback></mc:AlternateContent></xdr:twoCellAnchor>"##;
        let clean = strip_chartex_fallback(drawing);
        assert!(!clean.contains("not available"));
        assert!(clean.contains("graphicFrame"));
        assert_eq!(extract_chart_rids_from_drawing(&clean).len(), 1);
        let other = r##"<mc:AlternateContent><mc:Choice Requires="x"><a/></mc:Choice><mc:Fallback><b/></mc:Fallback></mc:AlternateContent>"##;
        assert_eq!(strip_chartex_fallback(other), other);
    }

    /// ROE_XLSX_STAT_FILE=<xlsx> cargo test -p roe-parser --lib real_file_stat_chart_read -- --ignored --nocapture
    #[test]
    #[ignore]
    fn real_file_stat_chart_read() {
        let p = match std::env::var("ROE_XLSX_STAT_FILE") { Ok(p) => p, Err(_) => return };
        let data = std::fs::read(&p).unwrap();
        let wb = XlsxParser::parse(&data).unwrap();
        for (si, sh) in wb.sheets.iter().enumerate() {
            println!("sheet {si} {}: floating_charts={} stat={}", sh.name, sh.floating_charts.len(), sh.floating_stat_charts.len());
            for c in &sh.floating_stat_charts {
                println!("  id={} kind={:?} rect=({:.1},{:.1},{:.1},{:.1}) series={} cats={:?} vals={:?}",
                    c.id, c.model.kind, c.x_pt, c.y_pt, c.width_pt, c.height_pt, c.model.series.len(),
                    c.model.data.first().map(|d| d.categories.clone()),
                    c.model.data.first().map(|d| d.values.clone()));
            }
        }
    }
}
