//! Writer/Impress 的 list_functions 补充说明(手写,不是脚本生成,gen_*_api_meta.py 不会覆盖)。
//! 只收录实测踩过坑、或签名看不出语义的函数。改名/删函数后,下面的测试会提醒同步。

/// Writer:函数名 -> 说明。
pub(crate) const WRITER_NOTES: &[(&str, &str)] = &[
    ("paragraph_count", r#"返回 body 元素总数(表格、图表、形状、分节符也各占一个),不是纯段落数。所有 para/idx/start/end 参数都是 body 下标,从 0 起。"#),
    ("get_paragraph", r#"按 body 下标取文本;该位置不是普通段落(表格/图表/形状)时返回空串。"#),
    ("get_paragraph_style", r#"参数是样式名(如 Heading1),不是段落序号。查某一段的格式用 get_paragraph_format(body 下标)。"#),
    ("get_paragraph_format", r#"参数是 body 下标,返回该段的样式名、对齐等格式。查样式定义本身用 get_paragraph_style(样式名)。"#),
    ("sort_paragraphs", r#"start/end 是 body 下标,且 end 包含(排 4 段、无表格图表时是 0,3 而不是 0,4);end 必须大于 start。kind 0=文字(汉字按拼音,字母忽略大小写,字母排在汉字前),1=数字(取段内第一个数字比较,无数字的排最前,数字相同再按文字);descending 非 0 为降序。区间内必须全是普通段落,含脚注/尾注引用的段落不能排。失败返回 err: 且文档不变。"#),
    ("set_char_format_range", r#"start_offset/end_offset 是段内 UTF-8 字节偏移(一个汉字 3 字节),半开区间 [start,end)。"#),
    ("find_text", r#"返回 {"total","truncated","matches":[{"para","start","end","text"}]}。para 是 body 下标,start/end 是段内 UTF-8 字节偏移。只搜 body 直属段落(不含表格、文本框),不跨段;最多 200 条,超出 truncated=true。"#),
    ("insert_comment", r#"(pa,oa) 到 (pb,ob) 是批注锚定的起止位置,pa/pb 为 body 下标,oa/ob 为段内 UTF-8 字节偏移。author 请显式给名字;text 里的换行会分段。成功返回 {"id":N}。"#),
    ("insert_footnote", r#"para 为 body 下标,offset 为段内 UTF-8 字节偏移;kind 0=脚注,1=尾注。成功返回 {"id":N}。"#),
    ("delete_note", r#"按 (para body 下标, offset 字节偏移, kind) 定位,offset 必须恰好落在引用的起点,找不到会报错。"#),
    ("insert_writer_chart", r#"para 为 body 下标,offset 为段内 UTF-8 字节偏移(落在字符中间会报错);图表自己也占一个 body 位置。json 最小示例:{"kind":"Bar","title":"标题","x_labels":["一月","二月"],"series":[{"name":"销量","data":[1,2]}]};kind 区分大小写,可选 Bar/HorizontalBar/Line/Pie/Scatter/Radar/Area/Doughnut。width/height 单位 pt;is_stat=true 表示统计图。"#),
    ("insert_writer_formula", r#"offset 为段内 UTF-8 字节偏移。成功返回 {"para_index":N,"run_idx":M},之后用 get_writer_formula/set_writer_formula(para, run_idx) 读改。"#),
    ("insert_writer_shape", r#"at 是新形状插入处的 body 下标(插在该位置之前);x/y 相对正文区,x/y/width/height 单位 pt;形状是浮动的,不带文字环绕。geometry 目前只认 rect,其他值报错。"#),
    ("change_case", r#"段内 [start,end) 改大小写,para 为 body 下标,偏移为 UTF-8 字节。"#),
    ("set_paragraph", r#"idx 是 body 下标(表格/图表/形状也占位)。整段替换文字,只保留段落格式。返回 String:"ok" 成功,以 err: 开头为失败(下标越界、该位置不是段落)。"#),
    ("insert_paragraph", r#"在 body 下标 idx 处插入新段落,idx 超出末尾会收敛到末尾追加,负数按 0。返回 "ok"。"#),
    ("append_paragraph", r#"在文档末尾追加段落。返回 "ok"。"#),
    ("apply_paragraph_style", r#"para_idx 是 body 下标。不校验样式名是否存在。返回 String:"ok" 成功,以 err: 开头为失败(下标越界、该位置不是段落)。"#),
    ("set_char_format", r#"para_idx 是 body 下标,run_idx 是段内 run 序号。font_size<=0 表示不改字号。返回 String:"ok" 成功,以 err: 开头为失败(下标越界、不是段落、run 越界;空段落没有 run,先 set_paragraph 写入文字)。"#),
    ("clear_char_format", r#"清除指定 run 的全部字符格式。返回 String:"ok" 成功,以 err: 开头为失败(下标越界、不是段落、run 越界)。"#),
    ("set_char_color", r#"para_idx 是 body 下标,run_idx 是段内 run 序号;r/g/b 须在 0~255,超出报错(不再回绕)。返回 String:"ok" 或以 err: 开头的失败原因。"#),
    ("set_char_font", r#"font_family 为中日韩字体名,font_family_ascii 为西文字体名,传空串表示不改该项。返回 String:"ok" 或 err:(下标越界、不是段落、run 越界)。"#),
    ("set_paragraph_format", r#"para_idx 是 body 下标。align 可选 left/center/right/justify,空串表示不改,非法值报错且不改原对齐;数值参数传负数表示不改该项。返回 String:"ok" 或 err:。"#),
    ("insert_hyperlink", r#"在 para_idx 段的 run_idx 位置插入一个带链接的新 run,run_idx 超出末尾会收敛到末尾。返回 String:"ok" 或 err:(下标越界、不是段落)。"#),
    ("set_list_format", r#"list_style 如 bullet/decimal,空串清除列表;level 为 0~8,超过 8 收敛到 8,负数报错。list_style 不校验取值。返回 String:"ok" 或 err:。"#),
    ("insert_image", r#"image_base64 为图片的 base64(可含换行),按 PNG 记录 MIME。width_pt/height_pt 须大于 0。para_idx 超出末尾会收敛到末尾。返回 String:"ok" 或 err:(base64 无效、尺寸非法)。"#),
    ("insert_table", r#"在 body 下标 para_idx 处插入 rows×cols 的空表格,para_idx 超出末尾收敛到末尾,负数按 0;rows/cols 小于 1 按 1,上限 200 行 50 列。默认每列 72pt。返回 String:"ok" 或 err:。新表格的 body 下标即实际插入位置。"#),
    ("table_insert_row", r#"在表格 body_idx 的第 row_idx 行之前(after=false)或之后(after=true)插入一行,新行格式克隆 row_idx 行,单元格为空。返回 String:"ok" 或 err:(body_idx 非表格/越界、row_idx 越界)。"#),
    ("table_delete_row", r#"删除表格 body_idx 的第 row_idx 行。只剩 1 行时拒绝删除并返回 err:。返回 String:"ok" 或 err:。"#),
    ("table_insert_column", r#"在表格 body_idx 的第 col_idx 列之前(after=false)或之后(after=true)插入一列,新列宽度与格式克隆 col_idx 列,单元格为空。返回 String:"ok" 或 err:(body_idx 非表格/越界、col_idx 越界)。"#),
    ("table_delete_column", r#"删除表格 body_idx 的第 col_idx 列。只剩 1 列时拒绝删除并返回 err:。返回 String:"ok" 或 err:。"#),
    ("set_table_col_width", r#"设置表格 body_idx 第 col_idx 列的宽度(pt)。width_pt 必须大于 0,否则返回 err:(不再静默忽略)。返回 String:"ok" 或 err:。"#),
    ("set_table_row_height", r#"设置表格 body_idx 第 row_idx 行的固定行高(pt)。height_pt 必须大于 0,否则返回 err:。返回 String:"ok" 或 err:。"#),
    ("clear_table_row_height", r#"清除表格 body_idx 第 row_idx 行的固定行高,恢复按内容自动测量。返回 String:"ok" 或 err:。"#),
    ("set_table_style_id", r#"设置表格 body_idx 的样式 id(不校验样式是否存在)。返回 String:"ok" 或 err:(body_idx 非表格/越界)。"#),
    ("clear_table_style_id", r#"清除表格 body_idx 的样式 id。返回 String:"ok" 或 err:(body_idx 非表格/越界)。"#),
    ("set_table_first_row", r#"设置表格 body_idx 的样式开关:首行(value=true 开启)。返回 String:"ok" 或 err:(body_idx 非表格/越界)。"#),
    ("set_table_first_col", r#"设置表格 body_idx 的样式开关:首列(value=true 开启)。返回 String:"ok" 或 err:(body_idx 非表格/越界)。"#),
    ("set_table_last_row", r#"设置表格 body_idx 的样式开关:末行(value=true 开启)。返回 String:"ok" 或 err:(body_idx 非表格/越界)。"#),
    ("set_table_last_col", r#"设置表格 body_idx 的样式开关:末列(value=true 开启)。返回 String:"ok" 或 err:(body_idx 非表格/越界)。"#),
    ("set_table_band_row", r#"设置表格 body_idx 的样式开关:行条纹(value=true 开启)。返回 String:"ok" 或 err:(body_idx 非表格/越界)。"#),
    ("set_table_band_col", r#"设置表格 body_idx 的样式开关:列条纹(value=true 开启)。返回 String:"ok" 或 err:(body_idx 非表格/越界)。"#),
    ("set_table_default_border_color", r#"设置表格 body_idx 的默认边框颜色,r/g/b 须在 0~255,越界返回 err:。返回 String:"ok" 或 err:(body_idx 非表格/越界)。"#),
    ("clear_table_default_border_color", r#"清除表格 body_idx 的默认边框颜色。返回 String:"ok" 或 err:(body_idx 非表格/越界)。"#),
    ("set_table_default_border_width", r#"设置表格 body_idx 的默认边框宽度(pt),width_pt 必须大于 0,否则返回 err:(不再静默忽略)。返回 String:"ok" 或 err:(body_idx 非表格/越界)。"#),
    ("clear_table_default_border_width", r#"清除表格 body_idx 的默认边框宽度。返回 String:"ok" 或 err:(body_idx 非表格/越界)。"#),
    ("set_table_inside_h_border_color", r#"设置表格 body_idx 的内部横线颜色,r/g/b 须在 0~255,越界返回 err:。返回 String:"ok" 或 err:(body_idx 非表格/越界)。"#),
    ("clear_table_inside_h_border_color", r#"清除表格 body_idx 的内部横线颜色。返回 String:"ok" 或 err:(body_idx 非表格/越界)。"#),
    ("set_table_inside_h_border_width", r#"设置表格 body_idx 的内部横线宽度(pt),width_pt 必须大于 0,否则返回 err:(不再静默忽略)。返回 String:"ok" 或 err:(body_idx 非表格/越界)。"#),
    ("clear_table_inside_h_border_width", r#"清除表格 body_idx 的内部横线宽度。返回 String:"ok" 或 err:(body_idx 非表格/越界)。"#),
    ("set_table_inside_v_border_color", r#"设置表格 body_idx 的内部竖线颜色,r/g/b 须在 0~255,越界返回 err:。返回 String:"ok" 或 err:(body_idx 非表格/越界)。"#),
    ("clear_table_inside_v_border_color", r#"清除表格 body_idx 的内部竖线颜色。返回 String:"ok" 或 err:(body_idx 非表格/越界)。"#),
    ("set_table_inside_v_border_width", r#"设置表格 body_idx 的内部竖线宽度(pt),width_pt 必须大于 0,否则返回 err:(不再静默忽略)。返回 String:"ok" 或 err:(body_idx 非表格/越界)。"#),
    ("clear_table_inside_v_border_width", r#"清除表格 body_idx 的内部竖线宽度。返回 String:"ok" 或 err:(body_idx 非表格/越界)。"#),
    ("clear_char_color", r#"清除第 para_idx 段第 run_idx 个 run 的字体颜色。返回 String:"ok" 或 err:(para_idx/run_idx 越界或非段落)。"#),
    ("set_char_background", r#"设置 run 的背景/高亮色,r/g/b 须在 0~255。返回 String:"ok" 或 err:(para_idx/run_idx 越界或非段落)。"#),
    ("clear_char_background", r#"清除 run 的背景色。返回 String:"ok" 或 err:(para_idx/run_idx 越界或非段落)。"#),
    ("set_char_strikethrough", r#"设置 run 是否有删除线(enabled)。返回 String:"ok" 或 err:(para_idx/run_idx 越界或非段落)。"#),
    ("set_char_script", r#"设置上下标,mode 为 normal/superscript/subscript,其他值返回 err:(不再落默认)。返回 String:"ok" 或 err:(para_idx/run_idx 越界或非段落)。"#),
    ("set_char_underline_style", r#"设置下划线样式 single/double/wave/none,其他值返回 err:;不改 underline 开关本身。返回 String:"ok" 或 err:(para_idx/run_idx 越界或非段落)。"#),
    ("set_line_height", r#"设置第 para_idx 段行距倍数,value 必须大于 0,否则返回 err:(不再静默忽略)。返回 String:"ok" 或 err:。"#),
    ("clear_line_height", r#"清除第 para_idx 段的行距设置。返回 String:"ok" 或 err:(para_idx 越界或非段落)。"#),
    ("set_wrap_type", r#"设置图文框/形状的环绕方式,wrap_type 为 none/square/tight/through/topAndBottom,其他值(含空串)返回 err:(不再静默忽略)。返回 String:"ok" 或 err:(body_idx 负数/越界/不是图文框或形状)。"#),
    ("set_frame_anchor", r#"设置图文框/形状的水平、垂直锚点,空串表示不改该方向,非空原样写入。返回 String:"ok" 或 err:(body_idx 负数/越界/不是图文框或形状)。"#),
    ("set_frame_align", r#"设置图文框/形状的水平、垂直对齐,空串表示不改该方向,非空原样写入。返回 String:"ok" 或 err:(body_idx 负数/越界/不是图文框或形状)。"#),
    ("set_wrap_distance", r#"设置环绕间距上下左右(pt),负数表示不改该方向。返回 String:"ok" 或 err:(body_idx 负数/越界/不是图文框或形状)。"#),
    ("set_page_layout", r#"设置页面宽高与四边距(pt);宽高须同时大于 0 才改页面尺寸,边距非正数表示不改该项。返回 String:"ok"。"#),
    ("set_header_footer", r#"设置文档默认页眉页脚文本,空串清除该项。返回 String:"ok"。"#),
    ("insert_section_break", r#"在 body 下标 body_idx 处插入分节符,超出末尾收敛到末尾,负数返回 err:。返回 String:"ok" 或 err:。"#),
    ("set_section_page_layout", r#"设置第 idx 节的页面宽高与四边距(pt),宽高须同时大于 0 才改页面尺寸,边距非正数表示不改该项。返回 String:"ok" 或 err:(idx 负数/越界,共 N 个节)。"#),
    ("set_section_header_footer", r#"设置第 idx 节的 default 页眉页脚文本,空串清除该项。返回 String:"ok" 或 err:(idx 负数/越界,共 N 个节)。"#),
    ("set_section_title_page", r#"设置第 idx 节首页页眉页脚是否独立。返回 String:"ok" 或 err:(idx 负数/越界,共 N 个节)。"#),
    ("set_section_type", r#"设置第 idx 节类型 nextPage/evenPage/oddPage/continuous,空串清除,其他值返回 err:(不再原样存入)。返回 String:"ok" 或 err:(idx 负数/越界,共 N 个节)。"#),
    ("set_section_columns", r#"设置第 idx 节的统一栏数,num_cols 必须大于 0,否则返回 err:(不再静默忽略)。返回 String:"ok" 或 err:(idx 负数/越界,共 N 个节)。"#),
    ("add_paragraph_style", r#"新增或覆盖(upsert)段落样式,style_json 须含非空 name;alignment 有值时须为 left/center/right/justify。返回 String:"ok" 或 err:(JSON 不合法/缺 name/字段非法)。"#),
    ("remove_paragraph_style", r#"删除段落样式,不存在返回 err:。返回 String:"ok" 或 err:。"#),
    ("add_char_style", r#"新增或覆盖(upsert)字符样式,style_json 须含非空 name;script 有值时须为 normal/superscript/subscript。返回 String:"ok" 或 err:。"#),
    ("remove_char_style", r#"删除字符样式,不存在返回 err:。返回 String:"ok" 或 err:。"#),
    ("generate_toc", r#"按样式名含 heading/h1/h2/h3 的段落在文档开头生成目录;没有标题返回 err:。注意非幂等,重复调用会重复插入目录。返回 String:"ok" 或 err:。"#),
    ("delete_range", r#"(pa,oa) 到 (pb,ob) 删除区间,pa/pb 为 body 下标,偏移为 UTF-8 字节;区间内须全是普通段落,删除后 pa 与 pb 合并,其后 body 下标前移。"#),
];

/// Impress:函数名 -> 说明。
pub(crate) const IMPRESS_NOTES: &[(&str, &str)] = &[
    ("get_last_error", r#"无参,返回最近一次创建类函数(insert_shape/insert_connector 等返回 id 的函数)失败的原因;没有失败返回空串。创建函数返回 -1 时调用它看原因;每次脚本开始时清空,所以必须在同一个脚本里、失败之后立刻读。"#),
    ("insert_slide", r#"建的是不带版式的裸页(没有占位符)。index 是新页的目标位置:插入后新页下标等于 index,原第 index 页及其后各页后移;index=当前页数表示追加到末尾;负数或超过页数会被收敛到 0..页数,不报错。注意与 insert_slide_with_layout 的 after 不同(那个是插在第 after 页之后)。master_idx=-1 表示不挂母版,否则须在 0..母版数内,越界失败(先 list_masters)。返回 i64:成功为新页下标(即 index 收敛后的值,可直接作后续函数的 slide 参数),失败 -1,原因在同一个脚本里紧接着用 get_last_error() 读取。新建页优先用 insert_slide_with_layout,先 list_layouts 看可选版式。"#),
    ("delete_slide", r#"返回 String:"ok" 成功,以 err: 开头为失败(index 越界)。删除后其后各页下标整体前移,连续删多页要从后往前删。"#),
    ("duplicate_slide", r#"返回 String:"ok" 成功,以 err: 开头为失败(index 越界)。副本插在原页之后(下标 index+1),页内 shape_id 与原页相同(shape_id 只在页内唯一)。"#),
    ("set_slide_master", r#"返回 String:"ok" 成功,以 err: 开头为失败。master_idx 为 -1 表示卸载母版,否则必须在 list_masters 返回的范围内,越界报错。"#),
    ("delete_shape", r#"返回 String:"ok" 成功,以 err: 开头为失败。第二个参数是 shape_id,不是页内下标;页越界或 id 不存在都会报错。"#),
    ("set_connector_arrows", r#"返回 String:"ok" 成功,以 err: 开头为失败(页越界或 shape_id 不存在)。head_arrow/tail_arrow 取 "arrow"/"triangle"/"oval"/"none",其它字符串按 none 处理,不报错。"#),
    ("insert_slide_with_layout", r#"after 是前一页的下标,layout_idx 见 list_layouts。返回 String,以 err: 开头为失败。"#),
    ("get_shape_text", r#"shape 是形状在页内的下标,不是 shape_id。参数名为 shape 的函数用页内下标,参数名为 shape_id 的函数用形状 id,二者不一定相等。"#),
    ("set_shape_text", r#"shape 是形状在页内的下标,不是 shape_id。返回 String:"ok" 成功,以 err: 开头为失败(页/形状下标越界)。会把形状内容替换为单段落单 run。"#),
    ("set_shape_char_format", r#"shape 是页内下标。返回 String:"ok" 成功,以 err: 开头为失败(下标越界、非文本形状、para_idx/run_idx 越界)。空占位符没有 run,会返回 err:,先 set_shape_text 写入文字再设格式。"#),
    ("set_shape_paragraph_format", r#"shape 是页内下标。返回 String:"ok" 成功,以 err: 开头为失败(下标越界、非文本形状、para_idx 越界、align 非法;align 可选 left/center/right/justify,空串表示不改)。数值参数传负数表示不改该项。"#),
    ("insert_textbox", r#"返回 i64:新形状的 shape_id(非负整数,可直接用于后续函数的 shape_id 参数),失败返回 -1,原因在同一个脚本里立刻调用 get_last_error() 读取。text 至少写一个字符(空文本框插不进去);新文本框追加在最上层,只需下移一层用 shape_move_backward,压到最底层用 shape_send_to_back。"#),
    ("insert_slide_table", r#"返回 i64:新表格的 shape_id,失败 -1(原因用 get_last_error() 读)。行数 1..=100,列数 1..=50;x/y/w/h 单位 pt,必须写小数,w、h 必须大于 0。"#),
    ("insert_slide_image", r#"返回 i64:新图片的 shape_id,失败 -1(原因用 get_last_error() 读)。path 是本地 PNG/JPEG 文件,不超过 20MB;x/y/w/h 单位 pt,必须写小数。"#),
    ("insert_slide_chart", r#"返回 i64:新图表的 shape_id,失败 -1(原因用 get_last_error() 读)。chart_json 必填 kind/x_labels/series。"#),
    ("get_slide_chart_json", r#"只支持普通图表;统计图和非图表返回 err:,不要对统计图调用。"#),
    ("set_slide_chart_json", r#"整体替换图表,保留原 id、位置、尺寸。只支持普通图表,不要对统计图调用。"#),
    ("insert_slide_stat_chart", r#"StatChartModel 没有默认值,所有字段(含 data、series 内的字段)必须写全。成功返回 i64 shape_id,失败 -1(原因用 get_last_error() 读)。"#),
    ("set_shape_fill", r#"shape_id 是形状 id。color 只认 #rrggbb 或 none,不认颜色名,不支持渐变/图片填充(没有对应接口,不要尝试 gradient: 之类写法)。none = 显式无填充(透明)。注意:新建的普通形状没有填充记录,画面上会显示为主题色半透明块;需要透明时要主动 set_shape_fill(slide, id, "none")。"#),
    ("set_shape_stroke", r#"shape_id 是形状 id。color 只认 #rrggbb;空字符串表示不改颜色,none 清除描边(此时 width 被忽略);不认颜色名;width 是 f64,传 -1.0 表示不改宽度,要写成小数(如 3.0)。设完可用 get_shape_style 读回 stroke、stroke_w。"#),
    ("set_shape_text_style", r#"shape_id 是形状 id。cmd 是 "键:值" 字符串,一次一个键:bold/italic/underline/strike 的值为 1、0 或 toggle(不写值等于 toggle);script 的值为 super 或 sub;size 的值为数字,带 + 或 - 前缀表示相对调整;color 的值为 #rrggbb 或 auto;background 的值为 #rrggbb 或 none;font 的值为字体名;clear 清除全部文字样式。例:"color:#1F3864"、"size:60"、"bold:1"。颜色不认颜色名。形状没有文字时返回 err:。不确定写法时不要在已有内容的形状上试,先在临时形状上试。读回用 get_shape_char_format_by_id。"#),
    ("set_shape_text_style_range", r#"shape_id 是形状 id,不是页内下标。cmd 语法同 set_shape_text_style。(p1,o1)-(p2,o2) 为起止位置:段落号从 0 起,偏移是段内 UTF-8 字节数(一个汉字占 3 字节,先用 get_shape_text_by_id 看文字再数)。起点大于终点会自动对调;空选区、越界、偏移落在汉字中间都返回 err: 且不改动。成功返回 ok。"#),
    ("get_shape_char_format", r#"shape 是页内下标,不含颜色、背景等信息。需要读颜色、字体、背景、删除线等用 get_shape_char_format_by_id(slide, shape_id, para_idx, run_idx)。"#),
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn writer_notes_names_exist_in_meta() {
        let names: BTreeSet<&str> = crate::writer_api_meta::WRITER_API_META.iter().map(|m| m.0).collect();
        let mut seen = BTreeSet::new();
        for (n, d) in WRITER_NOTES {
            assert!(names.contains(n), "WRITER_NOTES 里的 {n} 不在 WRITER_API_META 里(改名了?)");
            assert!(seen.insert(*n), "WRITER_NOTES 里 {n} 重复");
            assert!(!d.is_empty(), "{n} 的说明为空");
        }
    }

    #[test]
    fn impress_notes_names_exist_in_meta() {
        let names: BTreeSet<&str> = crate::impress_api_meta::IMPRESS_API_META.iter().map(|m| m.0).collect();
        let mut seen = BTreeSet::new();
        for (n, d) in IMPRESS_NOTES {
            assert!(names.contains(n), "IMPRESS_NOTES 里的 {n} 不在 IMPRESS_API_META 里(改名了?)");
            assert!(seen.insert(*n), "IMPRESS_NOTES 里 {n} 重复");
            assert!(!d.is_empty(), "{n} 的说明为空");
        }
    }

    #[test]
    fn writer_list_functions_shows_notes_and_header_hint() {
        let eng = crate::engine::ScriptEngine::new_readonly_writer();
        let mut doc = roe_model::writer::WriterDocument::default();
        let one = eng.run_writer("list_functions(\"get_paragraph\")", &mut doc).unwrap();
        let one = one.as_str().unwrap().to_string();
        assert!(one.contains("说明:") && one.contains("body 下标"), "应带出说明: {one}");
        let all = eng.run_writer("list_functions(\"\")", &mut doc).unwrap();
        let all = all.as_str().unwrap().to_string();
        assert!(all.contains("UTF-8 字节") && all.contains("body 下标"), "全表开头应有总提示: {all}");
        assert!(all.contains("get_paragraph*"), "有说明的函数名后应带 *: {all}");
    }
}
