//! Calc 能力自省:list_functions。数据来自 calc_api_meta.rs(scripts/gen_calc_api_meta.py 生成)。
use crate::calc_api_meta::CALC_API_META;
use crate::calc_df_meta::DF_API_META;
use rhai::Engine;

pub fn register_list_functions(engine: &mut Engine, readonly_env: bool) {
    // (名称, 参数, 返回, 只读, 说明)。df_* 来自 calc_df_meta;只读环境下隐藏桩函数,
    // df 的"只读"标记 = 只读环境可用。
    let mut items: Vec<(&'static str, &'static str, &'static str, bool, &'static str)> = CALC_API_META
        .iter()
        .filter(|m| !readonly_env || m.3)
        .map(|m| (m.0, m.1, m.2, m.3, ""))
        .collect();
    items.extend(
        DF_API_META
            .iter()
            .filter(|m| !readonly_env || m.4)
            .map(|m| (m.0, m.1, m.2, m.4, m.3)),
    );
    engine.register_fn("list_functions", move |filter: String| -> String {
        let terms: Vec<String> = filter.split_whitespace().map(|t| t.to_lowercase()).collect();
        // 按当前绑定的文档类型分流:只读环境恒为 Calc;Writer/Impress 没有读写标记,不带 [r]/[w]。
        if !readonly_env {
            if crate::impress_api::has_presentation() {
                let rows: Vec<(&str, &str, &str)> = crate::impress_api_meta::IMPRESS_API_META.iter().map(|m| (m.0, m.1, m.2)).collect();
                return list_simple("Impress 演示文稿", &rows, &terms, &filter, crate::api_notes::IMPRESS_NOTES, &|n| crate::impress_api::READ_FN_NAMES.contains(&n));
            }
            if crate::writer_api::has_document() {
                let rows: Vec<(&str, &str, &str)> = crate::writer_api_meta::WRITER_API_META.to_vec();
                return list_simple("Writer 文档", &rows, &terms, &filter, crate::api_notes::WRITER_NOTES, &|n| crate::writer_api::READ_FN_NAMES.contains(&n));
            }
        }
        if terms.is_empty() {
            let mut s = format!(
                "共 {} 个函数([r]只读 [w]可写)。用 list_functions(\"关键词\") 查看参数;关键词匹配函数名,多个词用空格分隔,任一命中即列出。sheet/row/col 均从 0 起。df_ 开头的是 DataFrame 数据分析函数(分组/SQL/透视/连接等),用 list_functions(\"df_\") 查看。\n",
                items.len()
            );
            for (n, _, _, ro, _) in &items {
                s.push_str(n);
                s.push_str(if *ro { "[r] " } else { "[w] " });
            }
            return s;
        }
        let mut out = String::new();
        for (n, p, r, ro, desc) in &items {
            let low = n.to_lowercase();
            if terms.iter().any(|t| low.contains(t.as_str())) {
                out.push_str(&format!("{}({}) -> {}  [{}]\n", n, p, r, if *ro { "只读" } else { "可写" }));
                if !desc.is_empty() {
                    out.push_str(&format!("  说明: {}\n", desc));
                }
                if let Some(ex) = example_for(n) {
                    out.push_str(&format!("  示例: {}\n", ex));
                }
            }
        }
        if out.is_empty() {
            format!("没有函数名包含 {:?};用 list_functions(\"\") 查看全部名称", filter)
        } else {
            out
        }
    });
}

/// Writer/Impress 的 list_functions 输出(无读写标记、无示例)。
fn list_simple(kind: &str, rows: &[(&str, &str, &str)], terms: &[String], filter: &str, notes: &[(&str, &str)], is_read: &dyn Fn(&str) -> bool) -> String {
    let tag = |n: &str| if is_read(n) { "[r]" } else { "[w]" };
    if terms.is_empty() {
        let mut s = format!(
            "当前是 {} ,共 {} 个函数。用 list_functions(\"关键词\") 查看参数;关键词匹配函数名,多个词用空格分隔,任一命中即列出。索引均从 0 起。名字后 [r] 只读、[w] 可写(只读查询环境不能直接调用 [w] 函数,需走 execute_script_preview)。\n",
            kind, rows.len());
        if kind.starts_with("Writer") {
            s.push_str("注意:Writer 函数里的 para/idx/start/end 都是 body 下标(表格、图表、形状也各占一个位置,paragraph_count() 返回的是 body 元素总数);带偏移的参数是段内 UTF-8 字节(一个汉字 3 字节)。用户说的「第 N 段」要先扫描 body 换算成下标。部分函数另有说明,用 list_functions(\"函数名\") 查看。\n");
        }
        if !notes.is_empty() {
            s.push_str("名字后带 * 的函数有额外说明(参数语义、返回格式、易错点),使用前先 list_functions(\"函数名\") 查看。\n");
        }
        for (n, _, _) in rows {
            s.push_str(n);
            if notes.iter().any(|e| e.0 == *n) { s.push('*'); }
            s.push_str(tag(n));
            s.push(' ');
        }
        return s;
    }
    let mut out = String::new();
    for (n, p, r) in rows {
        let low = n.to_lowercase();
        if terms.iter().any(|t| low.contains(t.as_str())) {
            out.push_str(&format!("{}{}({}) -> {}\n", n, tag(n), p, r));
            if let Some(e) = notes.iter().find(|e| e.0 == *n) {
                out.push_str(&format!("  说明: {}\n", e.1));
            }
        }
    }
    if out.is_empty() {
        format!("没有函数名包含 {:?};用 list_functions(\"\") 查看全部名称", filter)
    } else {
        out
    }
}

/// 只读 Writer 引擎用的 list_functions:只列 writer_api::READ_FN_NAMES 里的纯查询函数。
pub(crate) fn register_writer_readonly_list(engine: &mut Engine) {
    engine.register_fn("list_functions", move |filter: String| -> String {
        let terms: Vec<String> = filter.split_whitespace().map(|t| t.to_lowercase()).collect();
        let rows: Vec<(&str, &str, &str)> = crate::writer_api_meta::WRITER_API_META
            .iter()
            .map(|m| (m.0, m.1, m.2))
            .collect();
        list_simple("Writer 文档(只读查询环境,写函数请用 execute_script_preview)", &rows, &terms, &filter, crate::api_notes::WRITER_NOTES, &|n| crate::writer_api::READ_FN_NAMES.contains(&n))
    });
}

/// 只读 Impress 引擎用的 list_functions:只列 impress_api::READ_FN_NAMES 里的纯查询函数。
pub(crate) fn register_impress_readonly_list(engine: &mut Engine) {
    engine.register_fn("list_functions", move |filter: String| -> String {
        let terms: Vec<String> = filter.split_whitespace().map(|t| t.to_lowercase()).collect();
        let rows: Vec<(&str, &str, &str)> = crate::impress_api_meta::IMPRESS_API_META
            .iter()
            .map(|m| (m.0, m.1, m.2))
            .collect();
        list_simple("Impress 演示文稿(只读查询环境,写函数请用 execute_script_preview)", &rows, &terms, &filter, crate::api_notes::IMPRESS_NOTES, &|n| crate::impress_api::READ_FN_NAMES.contains(&n))
    });
}

/// 登记了示例的函数名(测试会校验它们都在 meta 里、且 example_for 返回 Some)。
pub(crate) const EXAMPLE_FUNCS: &[&str] = &[
    "sort_range", "insert_chart", "set_cell_format", "apply_filter",
    "apply_auto_filter_rows", "set_conditional_format", "add_cf_rule",
    "set_data_validation", "update_data_validation",
    "set_chart_rect", "move_chart_to_cell", "resize_chart_to_range", "list_charts",
    "set_chart_style", "get_chart_config",
    "calc_list_objects", "calc_get_object", "calc_set_object_rect", "calc_delete_object", "calc_insert_image",
    "calc_insert_shape", "calc_insert_textbox", "calc_set_object_text",
    "calc_set_object_fill", "calc_set_object_stroke",
    "calc_move_object_to_cell", "calc_resize_object_to_range",
    "get_pivot_config", "pivot_set_config", "pivot_set_filter", "pivot_set_sort", "pivot_create",
];

/// 按函数名返回示例文本。示例直接引用 calc_api.rs 的常量,避免两处漂移。
fn example_for(name: &str) -> Option<String> {
    use crate::calc_api::*;
    Some(match name {
        "sort_range" => format!(
            "range_json={} (sheet 键必填,行列从 0 起,闭区间);key_col 是相对范围起始列的偏移", CF_RANGE_EXAMPLE),
        "insert_chart" => format!(
            "chart_json={};kind 区分大小写,可选: {};data 必须是数字(不要加引号),x_labels 必须是字符串。{}",
            CHART_EXAMPLE, CHART_KINDS.join("/"), CHART_OPTIONAL_HELP),
        "set_cell_format" => format!(
            "fmt_json={};注意是整体替换该格格式,不是合并,只想加粗时要带上原有字段", CELL_FORMAT_EXAMPLE),
        "apply_filter" | "apply_auto_filter_rows" => format!(
            "filter_json={};键是相对 header_col 的列偏移,值是字符串数组(数字也要加引号)", AF_EXAMPLE),
        "set_conditional_format" => format!(
            "rule_json={};{}", CF_RULE_EXAMPLE, COND_HELP),
        "add_cf_rule" => format!(
            "range_json={};kind_json={}", CF_RANGE_EXAMPLE, CF_KIND_EXAMPLE),
        "set_data_validation" | "update_data_validation" => format!(
            "rule_json={};operator 可选: {}", DV_EXAMPLE, DV_OPERATORS.join("/")),
        "set_chart_style" => format!(
            "style_json={};局部修改,只写要改的键(kind/title/show_legend/show_grid/x_axis_label/y_axis_label/border_color/border_width/grouping/show_val/gap_width_pct/overlap_pct/hole_size_pct);title 与 *_pct 传 null 表示清除;统计图只支持 title/show_legend/show_grid;id 先用 list_charts 查",
            CHART_STYLE_EXAMPLE),
        "list_charts" => "返回 JSON 数组:id/kind/title/x_pt,y_pt,width_pt,height_pt/range(覆盖的单元格,如 B3:D6)/overlaps(与之重叠的图表 id)/at_origin;放新图或移动前先看一眼,避免互相盖住或盖住数据".to_string(),
        "get_chart_config" => "返回 JSON:普通图含 kind/title/show_legend/show_grid/x_axis_label/y_axis_label/border_color/border_width/grouping/show_val/gap_width_pct/overlap_pct/hole_size_pct 加 id/stat/x_pt,y_pt,width_pt,height_pt;这些格式键可整体或改几项后喂回 set_chart_style(位置尺寸不在其内,改用 set_chart_rect);统计图(stat=true)只有 kind/title/show_legend/show_grid,且只能改 title/show_legend/show_grid;id 先用 list_charts 查".to_string(),
        "set_chart_rect" => "set_chart_rect(0, 1, 48.0, 30.0, 300.0, 200.0);x/y/w/h 单位 pt,必须写成小数(48.0 不能写 48);id 先用 list_charts 查".to_string(),
        "move_chart_to_cell" => "move_chart_to_cell(0, 1, 2, 3) 把图表左上角移到 D3,尺寸不变;行列从 0 起".to_string(),
        "resize_chart_to_range" => "resize_chart_to_range(0, 1, 2, 1, 5, 3) 让图表正好盖住 B3:D6(闭区间,从 0 起);会同时改位置".to_string(),
        "calc_list_objects" => "calc_list_objects(0) 返回 JSON 数组,一项一个浮动对象(图表/统计图/图片/图形共用 id 空间):id/kind(chart|stat_chart|image|shape)/sub(图表种类或图形预设名)/x_pt,y_pt,width_pt,height_pt/rot/hidden,另有 title(图表)/text(图形)/mime(图片);改位置、删除前先看一眼,id 不要缓存,每次重新查".to_string(),
        "calc_get_object" => "calc_get_object(0, 1) 返回单个浮动对象的 JSON,字段同 calc_list_objects 的一项;id 必须是正整数,不存在会抛错,先用 calc_list_objects 查".to_string(),
        "calc_set_object_rect" => "calc_set_object_rect(0, 1, 48.0, 30.0, 300.0, 200.0);x/y/w/h 单位 pt,必须写成小数(48.0 不能写 48);对图表/统计图/图片/图形都有效,旋转角不变;id 先用 calc_list_objects 查".to_string(),
        "calc_set_object_fill" => "calc_set_object_fill(0, 2, \"#ff0000\");fill 是 #rrggbb,空串或 none 表示无填充;只对图形/文本框有效,图表和图片会报错;id 先用 calc_list_objects 查".to_string(),
        "calc_set_object_stroke" => "calc_set_object_stroke(0, 2, \"#333333\", 1.5);color 是 #rrggbb,空串或 none 表示无边框;width_pt 单位 pt,范围 (0,100],必须写成小数;只对图形/文本框有效;id 先用 calc_list_objects 查".to_string(),
        "calc_delete_object" => "calc_delete_object(0, 1) 删除图表/统计图/图片/图形,成功返回 true,id 不存在返回 false;删除后该 id 不会被复用;id 先用 calc_list_objects 查,每次删除后重新查,不要缓存".to_string(),
        "calc_insert_image" => "calc_insert_image(0, \"base64:iVBOR...\", 48.0, 30.0, 0.0, 0.0) 返回新对象 id;source 以 base64: 或 data:image/ 开头按 base64,否则按本地路径,只认 PNG/JPEG,上限 10 MB;w/h 传 0.0 按原比例(都为 0.0 时宽 200pt);x/y/w/h 单位 pt,必须写成小数".to_string(),
        "calc_insert_shape" => "calc_insert_shape(0, \"ellipse\", 48.0, 30.0, 120.0, 80.0, \"#ff0000\") 返回新对象 id;geometry 可用 rect/ellipse/line 或 OOXML 预设名(如 heart),写错会报错;fill 是 #rrggbb,空串表示无填充;文字另用 calc_set_object_text;x/y/w/h 单位 pt,必须写成小数".to_string(),
        "calc_insert_textbox" => "calc_insert_textbox(0, 48.0, 30.0, 200.0, 60.0, \"第一行\\n第二行\") 返回新对象 id;无填充矩形,\\n 分段;x/y/w/h 单位 pt,必须写成小数".to_string(),
        "calc_set_object_text" => "calc_set_object_text(0, 3, \"新文字\\n第二段\") 整体替换图形/文本框的文字,保留首段首个 run 的格式;图表和图片没有文字,会报错;id 先用 calc_list_objects 查".to_string(),
        "calc_move_object_to_cell" => "calc_move_object_to_cell(0, 3, 2, 3) 把浮动对象(图表/统计图/图片/图形)左上角移到 D3,尺寸不变;行列从 0 起,参数全是整数;id 先用 calc_list_objects 查".to_string(),
        "calc_resize_object_to_range" => "calc_resize_object_to_range(0, 3, 2, 1, 5, 3) 让浮动对象正好盖住 B3:D6(闭区间,从 0 起);会同时改位置;参数全是整数;id 先用 calc_list_objects 查".to_string(),
        "get_pivot_config" => "get_pivot_config(1) 返回透视表当前配置 JSON:rows/cols/values(源列名)/filters(页字段名)/source_idx(数据源 sheet 下标)/fields(数据源全部列名);非透视 sheet 返回 err:;改透视表前先看一眼".to_string(),
        "pivot_set_config" => "pivot_set_config(1, `{\"rows\":[\"Region\"],\"cols\":[\"Product\"],\"values\":[\"Sales\"],\"filters\":[]}`) 就地整体设置字段布局,返回 ok 或 err:...;四个键都是字段名数组,缺省=空,键名写错会报错;字段必须是数据源列名且不能跨区域重复;保留原有筛选和排序(字段不再使用时自动清理);不能改聚合方式".to_string(),
        "pivot_set_filter" => "pivot_set_filter(1, \"Region\", `[\"East\",\"West\"]`) 设置某字段的筛选(只显示这些值),\"[]\" 清除;字段须是页/行/列字段,值字段不行;返回 ok 或 err:...".to_string(),
        "pivot_set_sort" => "pivot_set_sort(1, \"Product\", false) 行/列字段降序,true 升序(恢复默认);页字段、值字段会报错;返回 ok 或 err:...".to_string(),
        "pivot_create" => "pivot_create(0, `{\"rows\":[\"Region\"],\"cols\":[\"Product\"],\"values\":[\"Sales\"],\"agg\":\"sum\",\"filters\":{}}`, \"透视1\") 以 sheet 0 为数据源新建透视表 sheet,成功返回 ok:新sheet下标;agg 只能是 sum/mean/count/min/max;注意这里 filters 是对象 {\"字段\":[允许值]},空数组=仅作页字段不限制行,和 pivot_set_config 的数组形状不同;数据源不能是透视 sheet;sheet 名规则同 add_sheet".to_string(),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn registered(e: &Engine) -> BTreeSet<String> {
        e.gen_fn_signatures(false)
            .into_iter()
            .map(|s| s.split('(').next().unwrap_or("").trim().to_string())
            .collect()
    }
    fn meta_names(readonly: bool) -> BTreeSet<String> {
        CALC_API_META.iter().filter(|m| !readonly || m.3).map(|m| m.0.to_string()).collect()
    }

    #[test]
    fn meta_matches_full_engine() {
        let mut e = Engine::new();
        crate::calc_api::register(&mut e);
        let (reg, meta) = (registered(&e), meta_names(false));
        assert_eq!(reg, meta, "仅注册有: {:?}; 仅清单有: {:?}",
            reg.difference(&meta).collect::<Vec<_>>(), meta.difference(&reg).collect::<Vec<_>>());
    }

    #[test]
    fn meta_matches_readonly_engine() {
        let mut e = Engine::new();
        crate::calc_api::register_readonly(&mut e);
        let (reg, meta) = (registered(&e), meta_names(true));
        assert_eq!(reg, meta, "仅注册有: {:?}; 仅清单有: {:?}",
            reg.difference(&meta).collect::<Vec<_>>(), meta.difference(&reg).collect::<Vec<_>>());
    }

    #[test]
    fn example_funcs_exist_in_meta_and_have_examples() {
        let names = meta_names(false);
        for f in EXAMPLE_FUNCS {
            assert!(names.contains(*f), "{f} 不在 meta 里");
            assert!(example_for(f).is_some(), "{f} 没有示例");
        }
    }

    #[test]
    fn list_functions_appends_example_for_insert_chart() {
        let mut e = Engine::new();
        register_list_functions(&mut e, false);
        let out = e.eval::<String>(r#"list_functions("insert_chart")"#).unwrap();
        assert!(out.contains("示例:") && out.contains("HorizontalBar") && out.contains("\"series\""), "got: {out}");
    }

    #[test]
    fn list_functions_insert_chart_lists_optional_fields() {
        let mut e = Engine::new();
        register_list_functions(&mut e, false);
        let out = e.eval::<String>(r#"list_functions("insert_chart")"#).unwrap();
        for k in crate::calc_api::CHART_OPTIONAL_KEYS {
            assert!(out.contains(k), "示例里缺少可选字段 {k}: {out}");
        }
    }

    #[test]
    fn df_meta_names_unique() {
        let set: BTreeSet<&str> = DF_API_META.iter().map(|m| m.0).collect();
        assert_eq!(set.len(), DF_API_META.len(), "DF_API_META 有重名");
    }

    #[test]
    fn df_meta_matches_full_registration() {
        let mut e = Engine::new();
        crate::calc_df_api::register(&mut e);
        let reg: BTreeSet<String> = registered(&e).into_iter().filter(|n| n.starts_with("df_")).collect();
        let meta: BTreeSet<String> = DF_API_META.iter().map(|m| m.0.to_string()).collect();
        assert_eq!(reg, meta, "仅注册有: {:?}; 仅清单有: {:?}",
            reg.difference(&meta).collect::<Vec<_>>(), meta.difference(&reg).collect::<Vec<_>>());
    }

    #[test]
    fn df_meta_stubs_behave_as_stubs_in_readonly_engine() {
        // register_readonly = 全量注册 + 同名桩覆盖。按 meta 的参数列表生成占位实参,
        // 逐个调用:标"不可用"的必须返回桩值(String 以 err: 开头,i64 为 -1);
        // 标"可用"的不得返回桩的提示语。
        let mut e = Engine::new();
        crate::calc_df_api::register_readonly(&mut e);
        let args_for = |params: &str| -> String {
            params.split(',').map(|x| x.trim()).filter(|x| !x.is_empty()).map(|x| {
                match x.split(':').nth(1).map(|t| t.trim()).unwrap_or("") {
                    "String" => "\"\"",
                    "i64" => "0",
                    "bool" => "false",
                    other => panic!("未处理的参数类型 {other}"),
                }
            }).collect::<Vec<_>>().join(", ")
        };
        for (name, params, ret, _desc, available) in DF_API_META {
            if *available { continue; }
            let call = format!("{}({})", name, args_for(params));
            match *ret {
                "i64" => {
                    let v = e.eval::<i64>(&call).unwrap_or_else(|er| panic!("{call}: {er}"));
                    assert_eq!(v, -1, "{name} 在只读环境应返回 -1");
                }
                "String" => {
                    let v = e.eval::<String>(&call).unwrap_or_else(|er| panic!("{call}: {er}"));
                    assert!(v.starts_with("err:") && v.contains("只读环境不可用"), "{name}: {v}");
                }
                other => panic!("{name}: 未处理的返回类型 {other}"),
            }
        }
        // 反向:df_info 标可用,不应是桩
        let v = e.eval::<String>("df_info()").unwrap();
        assert!(!v.contains("只读环境不可用"), "df_info 不应是桩: {v}");
    }

    #[test]
    fn list_functions_lists_df_and_hides_stubs_in_readonly() {
        let mut full = Engine::new();
        register_list_functions(&mut full, false);
        let out = full.eval::<String>(r#"list_functions("df_groupby_agg df_load_csv")"#).unwrap();
        assert!(out.contains("df_groupby_agg(") && out.contains("df_load_csv(") && out.contains("\"fn\":\"sum\""), "got: {out}");

        let mut ro = Engine::new();
        register_list_functions(&mut ro, true);
        let out = ro.eval::<String>(r#"list_functions("df_groupby_agg df_load_csv")"#).unwrap();
        assert!(out.contains("df_groupby_agg(") && !out.contains("df_load_csv("), "got: {out}");
        let all = ro.eval::<String>(r#"list_functions("")"#).unwrap();
        let body = all.split_once('\n').map(|x| x.1).unwrap_or("");
        assert!(all.contains("df_sql[r]") && !all.contains("df_to_sheet"), "got: {all}");
        assert!(!body.contains("[w]"), "只读引擎清单不应出现可写标记: {all}");
    }

    #[test]
    fn list_functions_no_example_for_plain_functions() {
        let mut e = Engine::new();
        register_list_functions(&mut e, false);
        let out = e.eval::<String>(r#"list_functions("get_cell")"#).unwrap();
        assert!(out.contains("get_cell(") && !out.contains("示例:"), "got: {out}");
    }

    #[test]
    fn json_examples_are_valid_json() {
        use crate::calc_api::*;
        for (n, j) in [("AF", AF_EXAMPLE), ("CF_RULE", CF_RULE_EXAMPLE), ("CELL_FORMAT", CELL_FORMAT_EXAMPLE),
                       ("CHART", CHART_EXAMPLE), ("CF_KIND", CF_KIND_EXAMPLE), ("CF_RANGE", CF_RANGE_EXAMPLE)] {
            assert!(serde_json::from_str::<serde_json::Value>(j).is_ok(), "{n} 示例不是合法 JSON");
        }
    }
}

#[cfg(test)]
mod json_contract_tests {
    use super::*;
    use roe_model::calc::{Sheet, Workbook};
    use std::collections::HashMap;

    fn empty_wb() -> Workbook {
        Workbook {
            sheets: vec![Sheet { name: "Sheet1".into(), ..Default::default() }],
            named_ranges: HashMap::new(),
            named_formulas: HashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        }
    }

    /// 按元数据参数串构造一次"坏 JSON"调用;没有 _json 参数返回 None。
    fn build_call(name: &str, params: &str) -> Option<String> {
        let mut has_json = false;
        let mut args: Vec<String> = Vec::new();
        for p in params.split(',') {
            let p = p.trim();
            if p.is_empty() { continue; }
            let (pn, ty) = p.split_once(':')?;
            let (pn, ty) = (pn.trim(), ty.trim());
            let a = if pn.ends_with("_json") {
                has_json = true;
                "`not json`".to_string()
            } else {
                match ty {
                    "i64" => "0".to_string(),
                    "f64" => "0.0".to_string(),
                    "bool" => "true".to_string(),
                    _ => "\"\"".to_string(),
                }
            };
            args.push(a);
        }
        if has_json { Some(format!("{}({})", name, args.join(", "))) } else { None }
    }

    #[test]
    fn json_contract_report() {
        let (mut loud, mut silent, mut missing) = (0, 0, 0);
        for m in CALC_API_META.iter().filter(|m| !m.3) {
            let Some(script) = build_call(m.0, m.1) else { continue };
            let mut wb = empty_wb();
            let mut e = Engine::new();
            crate::calc_api::register(&mut e);
            let mut res: Option<Result<String, String>> = None;
            let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                crate::calc_api::with_workbook(&mut wb, || {
                    res = Some(
                        e.eval::<rhai::Dynamic>(&script)
                            .map(|v| v.to_string())
                            .map_err(|er| er.to_string()),
                    );
                });
            }));
            if caught.is_err() {
                loud += 1;
                eprintln!("PANIC   {}", m.0);
                continue;
            }
            match res {
                Some(Err(s)) if s.contains("Function not found") => {
                    missing += 1;
                    eprintln!("MISSING {}", m.0);
                }
                Some(Err(_)) => {
                    loud += 1;
                    eprintln!("LOUD    {}", m.0);
                }
                Some(Ok(v)) => {
                    silent += 1;
                    let v: String = v.chars().take(60).collect();
                    eprintln!("SILENT  {}  -> 返回 {:?}", m.0, v);
                }
                None => eprintln!("NORUN   {}", m.0),
            }
        }
        eprintln!("SUMMARY loud={} silent={} missing={}", loud, silent, missing);
    }
}
