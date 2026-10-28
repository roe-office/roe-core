//! Writer API：段落/字符格式操作

use rhai::Engine;
use roe_model::common::{StyleRef, CellContent};
#[cfg(test)]
use roe_model::common::{ImageData, ShapeGeometry, ShapeStyle};
use roe_model::style::Alignment;
use roe_model::common::Color;
use roe_model::writer::{BodyElement, InlineImage, Paragraph, Run, ScriptPosition, Table, TableCell, TableRow, UnderlineStyle, WriterDocument};
use roe_writer::char_format::CharFormatPatch;
#[cfg(test)]
use roe_model::writer::{FloatFrame, Shape};
use std::cell::RefCell;
use base64::Engine as _;

#[path = "writer_api_ext.rs"]
mod ext;

thread_local! {
    static DOCUMENT: RefCell<Option<*mut WriterDocument>> = const { RefCell::new(None) };
}

pub fn with_document<F>(doc: &mut WriterDocument, f: F)
where F: FnOnce() {
    DOCUMENT.with(|cell| { *cell.borrow_mut() = Some(doc as *mut WriterDocument); });
    f();
    DOCUMENT.with(|cell| { *cell.borrow_mut() = None; });
}

fn with_doc<R, F: FnOnce(&mut WriterDocument) -> R>(f: F) -> Option<R> {
    DOCUMENT.with(|cell| {
        let ptr = (*cell.borrow())?;
        Some(f(unsafe { &mut *ptr }))
    })
}

/// 写函数统一回报:Some(Ok)->"ok";Some(Err(m))->m(已带 err: 前缀);None(未绑定文档)->err。
fn write_ret(r: Option<Result<(), String>>) -> String {
    match r {
        Some(Ok(())) => "ok".to_string(),
        Some(Err(m)) => m,
        None => "err:当前没有绑定 Writer 文档".to_string(),
    }
}

fn para_mut(doc: &mut WriterDocument, idx: i64) -> Result<&mut Paragraph, String> {
    let n = doc.body.len();
    if idx < 0 {
        return Err(format!("err:body 下标={} 不能为负数", idx));
    }
    match doc.body.get_mut(idx as usize) {
        Some(BodyElement::Paragraph(p)) => Ok(p),
        Some(_) => Err(format!("err:body[{}] 不是段落(是表格/图表/形状)", idx)),
        None => Err(format!("err:body 下标={} 越界(共 {} 个 body 元素)", idx, n)),
    }
}

fn run_mut(p: &mut Paragraph, idx: i64) -> Result<&mut Run, String> {
    let nr = p.runs.len();
    if idx < 0 {
        return Err(format!("err:run_idx={} 不能为负数", idx));
    }
    p.runs.get_mut(idx as usize)
        .ok_or_else(|| format!("err:run_idx={} 越界(该段共 {} 个 run;空段落请先 set_paragraph 写入文字)", idx, nr))
}

/// 取 body[idx] 的表格可变引用;失败时在当前闭包内 return Err("err:...")。
/// 只能用在返回 Result<_, String> 的闭包里。
macro_rules! table_of {
    ($doc:expr, $idx:expr) => {{
        let n = $doc.body.len();
        if $idx < 0 {
            return Err(format!("err:body 下标={} 不能为负数", $idx));
        }
        match $doc.body.get_mut($idx as usize) {
            Some(BodyElement::Table(t)) => t,
            Some(_) => return Err(format!("err:body[{}] 不是表格(是段落/图表/形状)", $idx)),
            None => return Err(format!("err:body 下标={} 越界(共 {} 个 body 元素)", $idx, n)),
        }
    }};
}

/// 校验 body[idx] 是图文框或形状;失败返回已带 err: 前缀的原因。
fn check_frame(doc: &WriterDocument, idx: i64) -> Result<(), String> {
    let n = doc.body.len();
    if idx < 0 {
        return Err(format!("err:body 下标={} 不能为负数", idx));
    }
    match doc.body.get(idx as usize) {
        Some(BodyElement::Frame(_)) | Some(BodyElement::Shape(_)) => Ok(()),
        Some(_) => Err(format!("err:body[{}] 不是图文框/形状", idx)),
        None => Err(format!("err:body 下标={} 越界(共 {} 个 body 元素)", idx, n)),
    }
}

/// 取 doc.sections[idx] 的可变引用;失败返回已带 err: 前缀的原因。
fn section_mut(doc: &mut WriterDocument, idx: i64) -> Result<&mut roe_model::writer::Section, String> {
    let n = doc.sections.len();
    if idx < 0 {
        return Err(format!("err:节下标 idx={} 不能为负数", idx));
    }
    doc.sections.get_mut(idx as usize)
        .ok_or_else(|| format!("err:节下标 idx={} 越界(共 {} 个节)", idx, n))
}

/// 当前线程是否绑定了 Writer 文档(run_writer 执行期间为 true)。供 list_functions 按文档类型分流。
#[allow(dead_code)]
pub(crate) fn has_document() -> bool {
    DOCUMENT.with(|cell| cell.borrow().is_some())
}

pub fn register(engine: &mut Engine) {
    ext::register(engine);
    register_read_fns(engine);
    // ── 基础段落操作 ────────────────────────────────────────────



    engine.register_fn("set_paragraph", |idx: i64, text: String| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            let n = doc.body.len();
            match doc.body.get_mut(idx as usize) {
                Some(BodyElement::Paragraph(p)) => {
                    p.runs = vec![Run { text: text.clone(), ..Default::default() }];
                    Ok(())
                }
                Some(_) => Err(format!("err:body[{}] 不是段落(是表格/图表/形状)", idx)),
                None => Err(format!("err:body 下标={} 越界(共 {} 个 body 元素)", idx, n)),
            }
        }))
    });

    engine.register_fn("insert_paragraph", |idx: i64, text: String| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            let para = Paragraph {
                style: StyleRef("Default".into()),
                runs: vec![Run { text: text.clone(), ..Default::default() }],
                ..Default::default()
            };
            let pos = (idx.max(0) as usize).min(doc.body.len());
            doc.body.insert(pos, BodyElement::Paragraph(para));
            Ok(())
        }))
    });

    engine.register_fn("append_paragraph", |text: String| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            let para = Paragraph {
                style: StyleRef("Default".into()),
                runs: vec![Run { text: text.clone(), ..Default::default() }],
                ..Default::default()
            };
            doc.body.push(BodyElement::Paragraph(para));
            Ok(())
        }))
    });

    // ── Step 23 新增 ────────────────────────────────────────────

    // apply_paragraph_style(para_idx, style_name)
    engine.register_fn("apply_paragraph_style", |para_idx: i64, style_name: String| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            let n = doc.body.len();
            match doc.body.get_mut(para_idx as usize) {
                Some(BodyElement::Paragraph(p)) => {
                    p.style = StyleRef(style_name.clone());
                    Ok(())
                }
                Some(_) => Err(format!("err:body[{}] 不是段落(是表格/图表/形状)", para_idx)),
                None => Err(format!("err:body 下标={} 越界(共 {} 个 body 元素)", para_idx, n)),
            }
        }))
    });

    // ── 样式定义增删改查：StyleRegistry.paragraph_styles / char_styles ──
    // 输入输出统一走JSON字符串（照抄get_paragraph_format已有的json!()+
    // to_string()惯例），原因：ParagraphStyle有13个字段、CharStyle有10个，
    // 远超本文件其余函数3-6个位置参数的规模，JSON方式避免"新建时用什么
    // 哨兵值表示某字段不设置"的设计难题——JSON里省略的字段直接落None/
    // 默认值即可。add_*是upsert语义：同名已存在则整条覆盖，不存在则新建，
    // 与HashMap本身"整条记录替换"的语义天然吻合，不做"部分字段更新"。

    fn parse_alignment(s: &str) -> Option<Alignment> {
        match s {
            "left"    => Some(Alignment::Left),
            "center"  => Some(Alignment::Center),
            "right"   => Some(Alignment::Right),
            "justify" => Some(Alignment::Justify),
            _         => None,
        }
    }


    fn parse_script_position(s: &str) -> Option<ScriptPosition> {
        match s {
            "superscript" => Some(ScriptPosition::Superscript),
            "subscript"   => Some(ScriptPosition::Subscript),
            "normal"      => Some(ScriptPosition::Normal),
            _             => None,
        }
    }



    fn parse_color(v: &serde_json::Value) -> Option<Color> {
        if v.is_null() { return None; }
        Some(Color {
            r: v.get("r").and_then(|x| x.as_u64()).unwrap_or(0) as u8,
            g: v.get("g").and_then(|x| x.as_u64()).unwrap_or(0) as u8,
            b: v.get("b").and_then(|x| x.as_u64()).unwrap_or(0) as u8,
            a: v.get("a").and_then(|x| x.as_u64()).unwrap_or(255) as u8,
        })
    }

    // add_paragraph_style(style_json)：style_json里"name"字段必填，作为
    // HashMap的key；其余字段缺省时落None/false。解析失败（非法JSON或
    // 缺name）时静默忽略，不panic、不报错给调用方（与本文件其余函数
    // "越界/无效输入静默忽略"的一贯风格一致）。
    engine.register_fn("add_paragraph_style", |style_json: String| -> String {
        let v = match serde_json::from_str::<serde_json::Value>(&style_json) {
            Ok(v) => v,
            Err(e) => return format!("err:style_json 不是合法 JSON({})", e),
        };
        let Some(name) = v.get("name").and_then(|x| x.as_str()) else {
            return "err:style_json 缺少字符串字段 name".to_string();
        };
        if name.is_empty() { return "err:name 不能为空".to_string(); }
        if let Some(a) = v.get("alignment") {
            if !a.is_null() && a.as_str().and_then(parse_alignment).is_none() {
                return format!("err:alignment 非法值 {}(可选 left/center/right/justify)", a);
            }
        }
        let style = roe_model::style::ParagraphStyle {
            name: name.to_string(),
            based_on: v.get("based_on").and_then(|x| x.as_str()).map(|s| s.to_string()),
            font_size: v.get("font_size").and_then(|x| x.as_f64()).map(|f| f as f32),
            font_family: v.get("font_family").and_then(|x| x.as_str()).map(|s| s.to_string()),
            font_family_ascii: v.get("font_family_ascii").and_then(|x| x.as_str()).map(|s| s.to_string()),
            bold: v.get("bold").and_then(|x| x.as_bool()).unwrap_or(false),
            italic: v.get("italic").and_then(|x| x.as_bool()).unwrap_or(false),
            alignment: v.get("alignment").and_then(|x| x.as_str()).and_then(parse_alignment),
            line_height: v.get("line_height").and_then(|x| x.as_f64()).map(|f| f as f32),
            space_before: v.get("space_before").and_then(|x| x.as_f64()).map(|f| f as f32),
            space_after: v.get("space_after").and_then(|x| x.as_f64()).map(|f| f as f32),
            indent_left: v.get("indent_left").and_then(|x| x.as_f64()).map(|f| f as f32),
            indent_first: v.get("indent_first").and_then(|x| x.as_f64()).map(|f| f as f32),
            color: v.get("color").map(parse_color).unwrap_or(None),
            list_style_name: v.get("list_style_name").and_then(|x| x.as_str()).map(|s| s.to_string()),
            list_level: v.get("list_level").and_then(|x| x.as_u64()).map(|n| n as u8),
            shading: None,
            border_left: None,
        };
        write_ret(with_doc(|doc| -> Result<(), String> {
            doc.styles.paragraph_styles.insert(name.to_string(), style);
            Ok(())
        }))
    });

    // remove_paragraph_style(name)
    engine.register_fn("remove_paragraph_style", |name: String| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            if doc.styles.paragraph_styles.remove(&name).is_none() {
                return Err(format!("err:段落样式 \"{}\" 不存在", name));
            }
            Ok(())
        }))
    });

    // get_paragraph_style(name) -> JSON字符串，不存在返回"{}"

    // list_paragraph_styles() -> JSON数组字符串，元素为所有已定义的样式名

    // add_char_style(style_json)：语义同add_paragraph_style，upsert。
    engine.register_fn("add_char_style", |style_json: String| -> String {
        let v = match serde_json::from_str::<serde_json::Value>(&style_json) {
            Ok(v) => v,
            Err(e) => return format!("err:style_json 不是合法 JSON({})", e),
        };
        let Some(name) = v.get("name").and_then(|x| x.as_str()) else {
            return "err:style_json 缺少字符串字段 name".to_string();
        };
        if name.is_empty() { return "err:name 不能为空".to_string(); }
        if let Some(a) = v.get("script") {
            if !a.is_null() && a.as_str().and_then(parse_script_position).is_none() {
                return format!("err:script 非法值 {}(可选 normal/superscript/subscript)", a);
            }
        }
        let style = roe_model::style::CharStyle {
            name: name.to_string(),
            based_on: v.get("based_on").and_then(|x| x.as_str()).map(|s| s.to_string()),
            font_size: v.get("font_size").and_then(|x| x.as_f64()).map(|f| f as f32),
            font_family: v.get("font_family").and_then(|x| x.as_str()).map(|s| s.to_string()),
            font_family_ascii: v.get("font_family_ascii").and_then(|x| x.as_str()).map(|s| s.to_string()),
            bold: v.get("bold").and_then(|x| x.as_bool()).unwrap_or(false),
            italic: v.get("italic").and_then(|x| x.as_bool()).unwrap_or(false),
            underline: v.get("underline").and_then(|x| x.as_bool()).unwrap_or(false),
            color: v.get("color").map(parse_color).unwrap_or(None),
            strikethrough: v.get("strikethrough").and_then(|x| x.as_bool()),
            background: v.get("background").map(parse_color).unwrap_or(None),
            script: v.get("script").and_then(|x| x.as_str()).and_then(parse_script_position),
            // WROOT-107: 字符基线垂直位移（pt，正上负下）
            vertical_position: v.get("vertical_position").and_then(|x| x.as_f64()).map(|f| f as f32),
        };
        write_ret(with_doc(|doc| -> Result<(), String> {
            doc.styles.char_styles.insert(name.to_string(), style);
            Ok(())
        }))
    });

    // remove_char_style(name)
    engine.register_fn("remove_char_style", |name: String| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            if doc.styles.char_styles.remove(&name).is_none() {
                return Err(format!("err:字符样式 \"{}\" 不存在", name));
            }
            Ok(())
        }))
    });

    // get_char_style(name) -> JSON字符串，不存在返回"{}"

    // list_char_styles() -> JSON数组字符串

    // set_char_format(para_idx, run_idx, bold, italic, underline, font_size)
    // font_size <= 0 表示不修改
    engine.register_fn("set_char_format",
        |para_idx: i64, run_idx: i64, bold: bool, italic: bool, underline: bool, font_size: f64| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            let n = doc.body.len();
            let p = match doc.body.get_mut(para_idx as usize) {
                Some(BodyElement::Paragraph(p)) => p,
                Some(_) => return Err(format!("err:body[{}] 不是段落(是表格/图表/形状)", para_idx)),
                None => return Err(format!("err:body 下标={} 越界(共 {} 个 body 元素)", para_idx, n)),
            };
            let nr = p.runs.len();
            let run = p.runs.get_mut(run_idx as usize)
                .ok_or_else(|| format!("err:run_idx={} 越界(该段共 {} 个 run;空段落请先 set_paragraph 写入文字)", run_idx, nr))?;
            run.bold      = Some(bold);
            run.italic    = Some(italic);
            run.underline = Some(underline);
            if font_size > 0.0 {
                run.font_size = Some(font_size as f32);
            }
            Ok(())
        }))
    });

    // get_char_format(para_idx, run_idx) -> JSON

    // clear_char_format(para_idx, run_idx)
    engine.register_fn("clear_char_format", |para_idx: i64, run_idx: i64| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            let n = doc.body.len();
            let p = match doc.body.get_mut(para_idx as usize) {
                Some(BodyElement::Paragraph(p)) => p,
                Some(_) => return Err(format!("err:body[{}] 不是段落(是表格/图表/形状)", para_idx)),
                None => return Err(format!("err:body 下标={} 越界(共 {} 个 body 元素)", para_idx, n)),
            };
            let nr = p.runs.len();
            let run = p.runs.get_mut(run_idx as usize)
                .ok_or_else(|| format!("err:run_idx={} 越界(该段共 {} 个 run)", run_idx, nr))?;
            run.bold             = None;
            run.italic           = None;
            run.underline        = None;
            run.underline_style  = None;
            run.font_size        = None;
            run.font_family      = None;
            run.font_family_ascii = None;
            run.color            = None;
            run.background       = None;
            run.strikethrough    = None;
            run.script           = None;
            run.char_style       = None;
            Ok(())
        }))
    });

    // ── v193新增：字符格式跨run范围操作（Rhai层range版本）──
    // 核心逻辑（跨run拆分/三类查询语义）已下沉到 roe_writer::char_format，
    // 本函数与 roe-api 的12个C-ABI函数共享同一套核心逻辑，仅编码层不同：
    // 混合值用 "mixed" 字符串表达（JSON自然语义），不借用C-ABI的数字哨兵。

    // set_char_format_range(para_idx, start_offset, end_offset,
    //                        bold, italic, underline, font_size, font_family, underline_style)
    // start_offset/end_offset：字节偏移，[start_offset, end_offset) 半开区间
    // bold/italic/underline：三态整数
    //   -1 = 不修改（保留原值）
    //    0 = 设为 false
    //    1 = 设为 true
    //    2 = 清除内联覆盖，回退到段落/字符样式默认值
    // font_size：<= 0.0 表示不修改；> 0.0 表示设为该值（pt）
    // font_family：空字符串表示不修改；非空表示设为该值
    // underline_style："" 表示不修改；"single"/"double"/"wave"/"none" 设为对应样式；
    //                   "clear" 表示清除内联覆盖回退默认值
    // 返回：成功返回空字符串 ""，失败返回错误信息（如区间越界/边界不在字符边界上）
    engine.register_fn("set_char_format_range",
        |para_idx: i64, start_offset: i64, end_offset: i64,
         bold: i64, italic: i64, underline: i64,
         font_size: f64, font_family: String, font_family_ascii: String,
         underline_style: String| -> String {
        with_doc(|doc| {
            if let Some(BodyElement::Paragraph(p)) = doc.body.get_mut(para_idx as usize) {
                fn tri_to_patch(v: i64) -> Option<Option<bool>> {
                    match v {
                        0 => Some(Some(false)),
                        1 => Some(Some(true)),
                        2 => Some(None),
                        _ => None, // -1 或其它非法值：不修改
                    }
                }

                let patch = CharFormatPatch {
                    color: None,
                    background: None,
                    bold: tri_to_patch(bold),
                    italic: tri_to_patch(italic),
                    underline: tri_to_patch(underline),
                    font_size: if font_size > 0.0 {
                        Some(Some(font_size as f32))
                    } else {
                        None
                    },
                    font_family: if font_family.is_empty() {
                        None
                    } else {
                        Some(Some(font_family.clone()))
                    },
                    font_family_ascii: if font_family_ascii.is_empty() {
                        None
                    } else {
                        Some(Some(font_family_ascii.clone()))
                    },
                    underline_style: match underline_style.to_lowercase().as_str() {
                        "" => None,
                        "clear" => Some(None),
                        "double" => Some(Some(UnderlineStyle::Double)),
                        "wave"   => Some(Some(UnderlineStyle::Wave)),
                        "none"   => Some(Some(UnderlineStyle::None)),
                        "single" => Some(Some(UnderlineStyle::Single)),
                        other => {
                            return format!("set_char_format_range: 未知 underline_style 值 \"{other}\"，应为 single/double/wave/none/clear/空字符串");
                        }
                    },
                    strikethrough: None,
                    script: None,
                };

                match roe_writer::char_format::set_char_format_in_range(
                    p, start_offset as usize, end_offset as usize, &patch,
                ) {
                    Ok(()) => String::new(),
                    Err(e) => format!("set_char_format_range: {e}"),
                }
            } else {
                format!("set_char_format_range: 段落索引 {para_idx} 不存在")
            }
        })
        .unwrap_or_else(|| "set_char_format_range: 未打开文档".to_string())
    });

    // get_char_format_range(para_idx, start_offset, end_offset) -> JSON
    // 一次性返回区间内全部字符格式属性的统计结果，风格照抄 get_char_format：
    //   - bold/italic/underline：一致时给出 true/false，混合时给出字符串 "mixed"
    //   - font_size：一致时给出具体数值，混合或区间内全部未设置时给出 null
    //   - font_family：一致时给出具体字符串，混合或全部未设置时给出 null
    //   - underline_style：一致时给出具体样式字符串（"single"/"double"/"wave"/"none"），
    //                       混合时给出 "mixed"；区间内全部未设置内联覆盖时给出 "single"
    //                       （对齐 get_char_format 里"无覆盖时按single显示"的既有惯例）
    // 出错（如区间越界）时返回 {"error": "..."}

    // replace_text(pattern, replacement, case_sensitive, start_para, end_para) -> JSON
    // Gemini缺口清单第2条：Writer 全局查找替换（ROE_Progress_v194.md §2.2）。
    // 复用 roe-writer::find_replace 核心逻辑（单段落内匹配，不跨段落，
    // 与Word/LibreOffice Writer内置查找替换的既有行为一致）。
    //   - pattern：正则表达式源串（regex crate 语法）。
    //     注意：pattern 是作为 Rhai 脚本字符串字面量写入的，若正则里
    //     用到 \d/\w/\s 等反斜杠转义元字符，Rhai 自身的字符串字面量
    //     语法不认识这些转义序列会解析报错（"MalformedEscapeSequence"），
    //     调用方需要用等价的字符类写法（如 [0-9] 代替 \d、[A-Za-z0-9_]
    //     代替 \w）绕开双重转义问题，或改用 Rhai 的原始字符串字面量
    //     `#"..."#`（Rhai 1.x 支持，内部反斜杠不转义）。
    //   - replacement：替换成的新文本，可以是空字符串（等价纯删除）。
    //   - case_sensitive：true=区分大小写，false=不区分。
    //   - start_para/end_para：段落范围 [start_para, end_para)，按 body
    //     下标计算。start_para<0 或 end_para<0 时视为"不限定范围"，
    //     对整个文档生效（与C-ABI层的u32::MAX哨兵是两套独立约定，
    //     Rhai层用负数哨兵更符合脚本语言习惯，不需要对齐C-ABI的
    //     unsigned语义，同 char_format.rs 的 "__MIXED__" vs "mixed"
    //     分层惯例）。
    // 返回：JSON `{"total_count": N, "affected_para_indices": [...]}`；
    // 正则编译失败或异常时返回 `{"error": "..."}`。
    engine.register_fn("replace_text",
        |pattern: String, replacement: String, case_sensitive: bool,
         start_para: i64, end_para: i64| -> String {
        let regex = match roe_writer::find_replace::compile_pattern(&pattern, case_sensitive) {
            Ok(re) => re,
            Err(e) => return serde_json::json!({
                "error": format!("replace_text: {e}")
            }).to_string(),
        };

        let para_range = if start_para < 0 || end_para < 0 {
            None
        } else {
            Some((start_para as usize, end_para as usize))
        };

        let result = with_doc(|doc| {
            roe_writer::find_replace::replace_text_in_document(doc, &regex, &replacement, para_range)
        });

        match result {
            Some(r) => serde_json::json!({
                "total_count": r.total_count,
                "affected_para_indices": r.affected_para_indices,
            }).to_string(),
            None => serde_json::json!({
                "error": "replace_text: 未打开文档"
            }).to_string(),
        }
    });

    // ── Writer接口开发v1 新增：字符格式补全（颜色/背景/删除线/上下标/字体名/下划线样式）──

    // set_char_color(para_idx, run_idx, r, g, b)：字体颜色，a固定255（不透明）
    engine.register_fn("set_char_color",
        |para_idx: i64, run_idx: i64, r: i64, g: i64, b: i64| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            for (name, v) in [("r", r), ("g", g), ("b", b)] {
                if !(0..=255).contains(&v) {
                    return Err(format!("err:颜色分量 {}={} 超出范围(须在 0~255)", name, v));
                }
            }
            let p = para_mut(doc, para_idx)?;
            let run = run_mut(p, run_idx)?;
            run.color = Some(Color { r: r as u8, g: g as u8, b: b as u8, a: 255 });
            Ok(())
        }))
    });

    // clear_char_color(para_idx, run_idx)
    engine.register_fn("clear_char_color", |para_idx: i64, run_idx: i64| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            let p = para_mut(doc, para_idx)?;
            let run = run_mut(p, run_idx)?;
            run.color = None;
            Ok(())
        }))
    });

    // set_char_background(para_idx, run_idx, r, g, b)：高亮/背景色，a固定255
    engine.register_fn("set_char_background", |para_idx: i64, run_idx: i64, r: i64, g: i64, b: i64| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            for (name, v) in [("r", r), ("g", g), ("b", b)] {
                if !(0..=255).contains(&v) {
                    return Err(format!("err:颜色分量 {}={} 超出范围(须在 0~255)", name, v));
                }
            }
            let p = para_mut(doc, para_idx)?;
            let run = run_mut(p, run_idx)?;
            run.background = Some(Color { r: r as u8, g: g as u8, b: b as u8, a: 255 });
            Ok(())
        }))
    });

    // clear_char_background(para_idx, run_idx)
    engine.register_fn("clear_char_background", |para_idx: i64, run_idx: i64| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            let p = para_mut(doc, para_idx)?;
            let run = run_mut(p, run_idx)?;
            run.background = None;
            Ok(())
        }))
    });

    // set_char_strikethrough(para_idx, run_idx, enabled)
    engine.register_fn("set_char_strikethrough", |para_idx: i64, run_idx: i64, enabled: bool| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            let p = para_mut(doc, para_idx)?;
            let run = run_mut(p, run_idx)?;
            run.strikethrough = Some(enabled);
            Ok(())
        }))
    });

    // set_char_script(para_idx, run_idx, mode)：mode = "normal"/"superscript"/"subscript"
    engine.register_fn("set_char_script", |para_idx: i64, run_idx: i64, mode: String| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            let v = match mode.to_lowercase().as_str() {
                "superscript" => ScriptPosition::Superscript,
                "subscript"   => ScriptPosition::Subscript,
                "normal"      => ScriptPosition::Normal,
                _ => return Err(format!("err:mode 非法值 \"{}\"(可选 normal/superscript/subscript)", mode)),
            };
            let p = para_mut(doc, para_idx)?;
            let run = run_mut(p, run_idx)?;
            run.script = Some(v);
            Ok(())
        }))
    });

    // set_char_font(para_idx, run_idx, font_family, font_family_ascii)
    // 东亚字体 font_family + 西文字体 font_family_ascii，空字符串 = 不修改该项
    engine.register_fn("set_char_font",
        |para_idx: i64, run_idx: i64, font_family: String, font_family_ascii: String| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            let p = para_mut(doc, para_idx)?;
            let run = run_mut(p, run_idx)?;
            if !font_family.is_empty() {
                run.font_family = Some(font_family.clone());
            }
            if !font_family_ascii.is_empty() {
                run.font_family_ascii = Some(font_family_ascii.clone());
            }
            Ok(())
        }))
    });

    // set_char_underline_style(para_idx, run_idx, style)
    // style = "single"/"double"/"wave"/"none"
    // 注：仅设置样式，不改 underline 开关本身；style为None时（未调用本函数）
    // 若 underline=true 渲染按 Single 处理，见 UnderlineStyle 枚举注释
    engine.register_fn("set_char_underline_style", |para_idx: i64, run_idx: i64, style: String| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            let v = match style.to_lowercase().as_str() {
                "single" => UnderlineStyle::Single,
                "double" => UnderlineStyle::Double,
                "wave"   => UnderlineStyle::Wave,
                "none"   => UnderlineStyle::None,
                _ => return Err(format!("err:style 非法值 \"{}\"(可选 single/double/wave/none)", style)),
            };
            let p = para_mut(doc, para_idx)?;
            let run = run_mut(p, run_idx)?;
            run.underline_style = Some(v);
            Ok(())
        }))
    });

    // set_paragraph_format(para_idx, align, indent_left, indent_first, spacing_before, spacing_after)
    // align: "left"/"center"/"right"/"justify"/""（空字符串=不修改）
    // 负数 = 不修改对应数值字段
    engine.register_fn("set_paragraph_format",
        |para_idx: i64, align: String,
         indent_left: f64, indent_first: f64,
         spacing_before: f64, spacing_after: f64| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            let p = para_mut(doc, para_idx)?;
            // 先校验对齐值再改,非法值不再清掉原对齐
            let new_align = if align.is_empty() {
                None
            } else {
                Some(match align.to_lowercase().as_str() {
                    "left"    => Alignment::Left,
                    "center"  => Alignment::Center,
                    "right"   => Alignment::Right,
                    "justify" => Alignment::Justify,
                    _ => return Err(format!("err:align 非法值 \"{}\"(可选 left/center/right/justify;空串表示不改)", align)),
                })
            };
            if let Some(a) = new_align { p.alignment = Some(a); }
            if indent_left   >= 0.0 { p.indent_left  = Some(indent_left as f32); }
            if indent_first  >= 0.0 { p.indent_first = Some(indent_first as f32); }
            if spacing_before >= 0.0 { p.space_before = Some(spacing_before as f32); }
            if spacing_after  >= 0.0 { p.space_after  = Some(spacing_after as f32); }
            Ok(())
        }))
    });

    // get_paragraph_format(para_idx) -> JSON

    // set_line_height(para_idx, value)：行间距，Writer接口开发v1 新增
    // value <= 0 表示不修改（与 set_paragraph_format 的"负数不修改"约定一致）
    engine.register_fn("set_line_height", |para_idx: i64, value: f64| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            if !(value > 0.0) {
                return Err(format!("err:value={} 必须大于 0", value));
            }
            let p = para_mut(doc, para_idx)?;
            p.line_height = Some(value as f32);
            Ok(())
        }))
    });

    // clear_line_height(para_idx)
    engine.register_fn("clear_line_height", |para_idx: i64| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            let p = para_mut(doc, para_idx)?;
            p.line_height = None;
            Ok(())
        }))
    });

    // ── Writer接口开发v1 新增：图文框环绕能力（FloatFrame/Shape 共用）──
    // body_idx 定位 doc.body 中的元素；命中 Frame 或 Shape 都处理，其余类型忽略。
    // wrap_type 等字段模型层现为 Option<String>（非枚举，见 roe-model/src/writer.rs
    // 字段注释列出的合法值集合），本层做输入校验：非法/空字符串一律视为"不修改"，
    // 不写入非法值，避免脏数据流入渲染层。

    fn normalize_wrap_type(s: &str) -> Option<String> {
        match s {
            "none" | "square" | "tight" | "through" | "topAndBottom" => Some(s.to_string()),
            _ => None,
        }
    }

    // set_wrap_type(body_idx, wrap_type)
    // wrap_type: "none"/"square"/"tight"/"through"/"topAndBottom"，其余值忽略不修改
    engine.register_fn("set_wrap_type", |body_idx: i64, wrap_type: String| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            check_frame(doc, body_idx)?;
            let Some(n) = normalize_wrap_type(&wrap_type) else {
                return Err(format!("err:wrap_type 非法值 \"{}\"(可选 none/square/tight/through/topAndBottom)", wrap_type));
            };
            match doc.body.get_mut(body_idx as usize) {
                Some(BodyElement::Frame(f)) => { f.wrap_type = Some(n); }
                Some(BodyElement::Shape(s)) => { s.wrap_type = Some(n); }
                _ => {}
            }
            Ok(())
        }))
    });

    // set_frame_anchor(body_idx, anchor_h, anchor_v)
    // 空字符串 = 不修改该方向；非空则原样写入（如 "page"/"margin"/"column"/"paragraph"/"line"）
    engine.register_fn("set_frame_anchor",
        |body_idx: i64, anchor_h: String, anchor_v: String| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            check_frame(doc, body_idx)?;
            match doc.body.get_mut(body_idx as usize) {
                Some(BodyElement::Frame(f)) => {
                    if !anchor_h.is_empty() { f.anchor_type_h = Some(anchor_h.clone()); }
                    if !anchor_v.is_empty() { f.anchor_type_v = Some(anchor_v.clone()); }
                }
                Some(BodyElement::Shape(s)) => {
                    if !anchor_h.is_empty() { s.anchor_type_h = Some(anchor_h.clone()); }
                    if !anchor_v.is_empty() { s.anchor_type_v = Some(anchor_v.clone()); }
                }
                _ => {}
            }
            Ok(())
        }))
    });

    // set_frame_align(body_idx, align_h, align_v)
    // 空字符串 = 不修改该方向；非空则原样写入（如 "left"/"center"/"right"/"inside"/"outside"）
    engine.register_fn("set_frame_align",
        |body_idx: i64, align_h: String, align_v: String| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            check_frame(doc, body_idx)?;
            match doc.body.get_mut(body_idx as usize) {
                Some(BodyElement::Frame(f)) => {
                    if !align_h.is_empty() { f.align_h = Some(align_h.clone()); }
                    if !align_v.is_empty() { f.align_v = Some(align_v.clone()); }
                }
                Some(BodyElement::Shape(s)) => {
                    if !align_h.is_empty() { s.align_h = Some(align_h.clone()); }
                    if !align_v.is_empty() { s.align_v = Some(align_v.clone()); }
                }
                _ => {}
            }
            Ok(())
        }))
    });

    // set_wrap_distance(body_idx, dist_t, dist_b, dist_l, dist_r)：环绕间距四方向（pt）
    // 负数 = 不修改对应方向（约定与 set_paragraph_format 一致）
    engine.register_fn("set_wrap_distance",
        |body_idx: i64, dist_t: f64, dist_b: f64, dist_l: f64, dist_r: f64| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            check_frame(doc, body_idx)?;
            match doc.body.get_mut(body_idx as usize) {
                Some(BodyElement::Frame(f)) => {
                    if dist_t >= 0.0 { f.dist_t_pt = dist_t as f32; }
                    if dist_b >= 0.0 { f.dist_b_pt = dist_b as f32; }
                    if dist_l >= 0.0 { f.dist_l_pt = dist_l as f32; }
                    if dist_r >= 0.0 { f.dist_r_pt = dist_r as f32; }
                }
                Some(BodyElement::Shape(s)) => {
                    if dist_t >= 0.0 { s.dist_t_pt = dist_t as f32; }
                    if dist_b >= 0.0 { s.dist_b_pt = dist_b as f32; }
                    if dist_l >= 0.0 { s.dist_l_pt = dist_l as f32; }
                    if dist_r >= 0.0 { s.dist_r_pt = dist_r as f32; }
                }
                _ => {}
            }
            Ok(())
        }))
    });

    // get_frame_wrap(body_idx) -> JSON：读取当前环绕配置，命中 Frame/Shape 之外返回 "{}"

    // set_page_layout(width_pt, height_pt, margin_top, margin_bottom, margin_left, margin_right)
    // margin 参数暂存 page_size，margin 字段后续 roe-model 扩展时补充
    engine.register_fn("set_page_layout",
        |width_pt: f64, height_pt: f64,
         margin_top: f64, margin_bottom: f64,
         margin_left: f64, margin_right: f64| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            if width_pt > 0.0 && height_pt > 0.0 {
                doc.page_size = (width_pt as f32, height_pt as f32);
            }
            if margin_top    > 0.0 { doc.margin_top    = margin_top    as f32; }
            if margin_bottom > 0.0 { doc.margin_bottom = margin_bottom as f32; }
            if margin_left   > 0.0 { doc.margin_left   = margin_left   as f32; }
            if margin_right  > 0.0 { doc.margin_right  = margin_right  as f32; }
            Ok(())
        }))
    });

    // ── Step 24 新增 ────────────────────────────────────────────

    // insert_hyperlink(para_idx, run_idx, url, text)
    // 在指定位置插入一个新 Run，携带超链接
    engine.register_fn("insert_hyperlink",
        |para_idx: i64, run_idx: i64, url: String, text: String| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            let p = para_mut(doc, para_idx)?;
            let run = Run {
                text: text.clone(),
                hyperlink: Some(url.clone()),
                ..Default::default()
            };
            let pos = (run_idx.max(0) as usize).min(p.runs.len());
            p.runs.insert(pos, run);
            Ok(())
        }))
    });

    // set_list_format(para_idx, list_style, level)
    // list_style: "bullet"/"decimal"/"""（空=清除列表）
    engine.register_fn("set_list_format",
        |para_idx: i64, list_style: String, level: i64| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            if level < 0 {
                return Err(format!("err:level={} 不能为负数(0~8)", level));
            }
            let p = para_mut(doc, para_idx)?;
            if list_style.is_empty() {
                p.list_info = None;
            } else {
                p.list_info = Some(roe_model::writer::ListInfo {
                    list_id:    format!("list_{}", para_idx),
                    level:      (level.min(8)) as u8,
                    style_name: list_style.clone(),
                });
            }
            Ok(())
        }))
    });

    // set_header_footer(header_text, footer_text)
    // 空字符串=清除
    engine.register_fn("set_header_footer", |header_text: String, footer_text: String| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            doc.header.default = if header_text.is_empty() { None } else { Some(plain_text_to_paragraphs(&header_text)) };
            doc.footer.default = if footer_text.is_empty() { None } else { Some(plain_text_to_paragraphs(&footer_text)) };
            Ok(())
        }))
    });

    // insert_image(para_idx, image_base64, width_pt, height_pt)
    // 在 para_idx 位置插入图片（作为独立 BodyElement）
    engine.register_fn("insert_image",
        |para_idx: i64, image_base64: String, width_pt: f64, height_pt: f64| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            if !(width_pt.is_finite() && height_pt.is_finite() && width_pt > 0.0 && height_pt > 0.0) {
                return Err("err:width_pt/height_pt 必须是大于 0 的有限数".to_string());
            }
            let decoded = base64::engine::general_purpose::STANDARD.decode(
                image_base64.replace([' ', '\n', '\r'], "").as_bytes()
            ).map_err(|e| format!("err:image_base64 解码失败: {}", e))?;
            if decoded.is_empty() {
                return Err("err:image_base64 为空".to_string());
            }
            let img = InlineImage {
                image: roe_model::common::ImageData { data: decoded, mime: "image/png".to_string(), ..Default::default() },
                width_pt:    width_pt as f32,
                height_pt:   height_pt as f32,
                ..Default::default()
            };
            let pos = (para_idx.max(0) as usize).min(doc.body.len());
            doc.body.insert(pos, BodyElement::Image(img));
            Ok(())
        }))
    });

    // ── 分节页面设置（Writer接口开发v1 3节7.3条）─────────────────────
    // 模型/解析/渲染三层已就绪（v79+WROOT任务B验证通过），本轮只补接口层。
    // 关键约束（渲染层layout.rs的隐含契约，接口必须遵守，不能只做其中一半）：
    // body里每出现一个BodyElement::Section路标，渲染层就cur_section_index+=1，
    // 然后查doc.sections[cur_section_index]取该节的排版属性——路标出现顺序
    // 必须与doc.sections数组顺序完全一致，因此"插入分节符"必须同时做两件事：
    // ①在body插入路标 ②往doc.sections同步追加一条对应记录，顺序不能错位。
    // 范围收窄说明：header/footer本轮只支持default档（不做奇偶页/首页精细
    // 控制，理由同顶层set_header_footer的既有取舍）；columns本轮只支持
    // 统一栏数num_cols（不做col_widths/col_spacing精细分栏宽度设置）——
    // 这些模型字段确实存在，但要做全会显著扩大范围，先覆盖最常见需求。

    // insert_section_break(body_idx)：在body_idx位置插入分节符路标，同时
    // 在doc.sections末尾追加一条默认Section记录（各字段None=沿用顶层）。
    // body_idx越界时clamp到body.len()（末尾插入），与insert_image的既有
    // 越界处理风格一致。
    engine.register_fn("insert_section_break", |body_idx: i64| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            if body_idx < 0 {
                return Err(format!("err:body 下标={} 不能为负数", body_idx));
            }
            let pos = (body_idx as usize).min(doc.body.len());
            let idx = doc.sections.len();
            let section = roe_model::writer::Section {
                name: format!("Section {}", idx + 1),
                ..Default::default()
            };
            doc.sections.push(section);
            doc.body.insert(pos, BodyElement::Section(roe_model::writer::Section {
                name: format!("Section {}", idx + 1),
                ..Default::default()
            }));
            Ok(())
        }))
    });

    // section_count() -> i64：当前doc.sections条目数

    // get_section(idx) -> JSON：读取指定节当前设置，字段为null表示"沿用
    // WriterDocument顶层默认值"（与Section各字段Option<T>语义一致）。
    // idx越界返回"{}"。

    // set_section_page_layout(idx, width_pt, height_pt, margin_top, margin_bottom, margin_left, margin_right)
    // 各数值参数<=0时不修改对应字段（与set_page_layout/set_table_col_width
    // 等既有函数的"非正数=不修改"约定一致）。idx越界静默忽略。
    engine.register_fn("set_section_page_layout",
        |idx: i64, width_pt: f64, height_pt: f64,
         margin_top: f64, margin_bottom: f64, margin_left: f64, margin_right: f64| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            let section = section_mut(doc, idx)?;
            if width_pt > 0.0 && height_pt > 0.0 {
                section.page_size = Some((width_pt as f32, height_pt as f32));
            }
            if margin_top > 0.0    { section.margin_top    = Some(margin_top as f32); }
            if margin_bottom > 0.0 { section.margin_bottom = Some(margin_bottom as f32); }
            if margin_left > 0.0   { section.margin_left   = Some(margin_left as f32); }
            if margin_right > 0.0  { section.margin_right  = Some(margin_right as f32); }
            Ok(())
        }))
    });

    // set_section_header_footer(idx, header_text, footer_text)
    // 仅支持default档（本轮范围收窄，理由见上方注释）。空字符串=清除该项
    // （回退为None，即"沿用顶层"），与顶层set_header_footer的既有约定一致。
    engine.register_fn("set_section_header_footer",
        |idx: i64, header_text: String, footer_text: String| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            let section = section_mut(doc, idx)?;
            if header_text.is_empty() {
                if let Some(hf) = section.header.as_mut() { hf.default = None; }
            } else {
                let hf = section.header.get_or_insert_with(Default::default);
                hf.default = Some(plain_text_to_paragraphs(&header_text));
            }
            if footer_text.is_empty() {
                if let Some(hf) = section.footer.as_mut() { hf.default = None; }
            } else {
                let hf = section.footer.get_or_insert_with(Default::default);
                hf.default = Some(plain_text_to_paragraphs(&footer_text));
            }
            Ok(())
        }))
    });

    // set_section_title_page(idx, value)：本节首页页眉页脚是否独立
    engine.register_fn("set_section_title_page", |idx: i64, value: bool| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            let section = section_mut(doc, idx)?;
            section.title_page = Some(value);
            Ok(())
        }))
    });

    // set_section_type(idx, section_type)：section_type为
    // "nextPage"/"evenPage"/"oddPage"/"continuous"之一，其余值（含空
    // 字符串）一律清除为None（沿用默认nextPage语义），不做值校验白名单
    // 拒绝——原样存字符串，交给渲染层/写出层自行决定如何解读未知值，
    // 与本文件其余"原样传字符串"风格（如set_frame_anchor）一致。
    engine.register_fn("set_section_type", |idx: i64, section_type: String| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            let section = section_mut(doc, idx)?;
            if !section_type.is_empty()
                && !["nextPage", "evenPage", "oddPage", "continuous"].contains(&section_type.as_str())
            {
                return Err(format!("err:section_type 非法值 \"{}\"(可选 nextPage/evenPage/oddPage/continuous;空串清除)", section_type));
            }
            section.section_type = if section_type.is_empty() { None } else { Some(section_type) };
            Ok(())
        }))
    });

    // set_section_columns(idx, num_cols)：仅设置统一栏数，不做逐栏宽度/
    // 间距精细控制（本轮范围收窄，理由见上方注释）。num_cols<=0静默忽略。
    engine.register_fn("set_section_columns", |idx: i64, num_cols: i64| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            if num_cols <= 0 {
                return Err(format!("err:num_cols={} 必须大于 0", num_cols));
            }
            let section = section_mut(doc, idx)?;
            section.columns = Some(roe_model::writer::DocumentColumns {
                num_cols: num_cols as usize,
                ..Default::default()
            });
            Ok(())
        }))
    });

    // insert_table(para_idx, rows, cols)
    // 在 para_idx 位置插入空表格
    engine.register_fn("insert_table", |para_idx: i64, rows: i64, cols: i64| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            if rows > 200 || cols > 50 {
                return Err(format!("err:表格过大(rows={} 上限 200,cols={} 上限 50)", rows, cols));
            }
            let rows = (rows as usize).max(1);
            let cols = (cols as usize).max(1);
            let table = Table {
                float_pos: None,
                col_widths: vec![72.0; cols], // 默认每列 72pt（约 1 英寸）
                style_id: None,
                first_row: false,
                first_col: false,
                last_row: false,
                last_col: false,
                band_row: false,
                band_col: false,
                default_border_color: None,
                default_border_width: None,
                inside_h_color: None,
                inside_h_width: None,
                inside_v_color: None,
                inside_v_width: None,
                rows: (0..rows).map(|_| TableRow {
                    cells: (0..cols).map(|_| TableCell {
                        content:  vec![CellContent::Paragraph(Paragraph {
                            style: StyleRef("Default".into()),
                            runs:  vec![],
                            ..Default::default()
                        })],
                        col_span: 1,
                        row_span: 1,
                        style:    None,
                        hidden:   false, bg_color: None,
                        border_color: None, border_width: None,
                        pad_left: None, pad_right: None,
                        pad_top: None, pad_bottom: None,
                    }).collect(),
                    row_height: None,
                }).collect(),
            };
            let pos = (para_idx.max(0) as usize).min(doc.body.len());
            doc.body.insert(pos, BodyElement::Table(table));
            Ok(())
        }))
    });

    // ── 表格行列增删（table_ 前缀，避免与 roe-calc 的 insert_row/delete_row/
    //    insert_column/delete_column 撞名，见 ROE_Progress_v165.md 1.4/2.1节）──
    //
    // 通用约束：
    // - 仅支持顶层表格（doc.body 下的 BodyElement::Table），不支持嵌套在
    //   CellContent::Table 里的表格，body_idx 指向非Table或越界时静默忽略。
    // - 本轮不支持合并/拆分单元格，正常情况下每行 cells.len() == col_widths.len()；
    //   insert_column 对 cells.len() <= col_idx 的行做防御性跳过（历史遗留数据
    //   兜底，不追求正确处理合并单元格对齐，该场景本轮不产生也不主动支持）。
    // - 参考LOK（sw/source/uibase/shells/tabsh.cxx）：删除行列命令本身不做
    //   "至少保留N行/N列"式的强保护，但由于我们目前没有配套的"删除整个表格"
    //   函数，删除后会导致行数或列数变为0的操作在此静默忽略，避免产生空表格
    //   的异常状态。

    // table_insert_row(body_idx, row_idx, after) -> "ok" | "err:原因"
    // 在 row_idx 位置的前/后插入一行。新行 cell 数量等于原表列数，格式克隆 row_idx 行。
    // body_idx 非表格/越界、row_idx 越界均报错。
    engine.register_fn("table_insert_row", |body_idx: i64, row_idx: i64, after: bool| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            let table = table_of!(doc, body_idx);
            let nrows = table.rows.len();
            if row_idx < 0 || row_idx as usize >= nrows {
                return Err(format!("err:row_idx={} 越界(该表共 {} 行)", row_idx, nrows));
            }
            let row_idx = row_idx as usize;
            let new_cells: Vec<TableCell> = table.rows[row_idx].cells.iter().map(|src_cell| TableCell {
                content: vec![CellContent::Paragraph(Paragraph {
                    style: StyleRef("Default".into()),
                    runs: vec![],
                    ..Default::default()
                })],
                col_span: 1,
                row_span: 1,
                style: None,
                hidden: false,
                bg_color: src_cell.bg_color,
                border_color: src_cell.border_color,
                border_width: src_cell.border_width,
                pad_left: None, pad_right: None,
                pad_top: None, pad_bottom: None,
            }).collect();
            let new_row = TableRow { cells: new_cells, row_height: None };
            let insert_pos = if after { row_idx + 1 } else { row_idx };
            let insert_pos = insert_pos.min(table.rows.len());
            table.rows.insert(insert_pos, new_row);
            Ok(())
        }))
    });

    // table_delete_row(body_idx, row_idx) -> "ok" | "err:原因"
    // 越界报错;只剩 1 行时拒绝删除(避免空表格)并报错。
    engine.register_fn("table_delete_row", |body_idx: i64, row_idx: i64| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            let table = table_of!(doc, body_idx);
            let nrows = table.rows.len();
            if row_idx < 0 || row_idx as usize >= nrows {
                return Err(format!("err:row_idx={} 越界(该表共 {} 行)", row_idx, nrows));
            }
            if nrows <= 1 {
                return Err("err:不能删除最后一行(表格不能为空)".to_string());
            }
            table.rows.remove(row_idx as usize);
            Ok(())
        }))
    });

    // table_insert_column(body_idx, col_idx, after) -> "ok" | "err:原因"
    // col_widths 对应位置插入新宽度(克隆 col_widths[col_idx]);每行在对应位置插入新 cell。
    // cells.len() <= col_idx 的行做防御性跳过(历史合并单元格遗留)。
    engine.register_fn("table_insert_column", |body_idx: i64, col_idx: i64, after: bool| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            let table = table_of!(doc, body_idx);
            let ncols = table.col_widths.len();
            if col_idx < 0 || col_idx as usize >= ncols {
                return Err(format!("err:col_idx={} 越界(该表共 {} 列)", col_idx, ncols));
            }
            let col_idx = col_idx as usize;
            let new_width = table.col_widths[col_idx];
            let width_pos = if after { col_idx + 1 } else { col_idx };
            let width_pos = width_pos.min(table.col_widths.len());
            table.col_widths.insert(width_pos, new_width);

            for row in table.rows.iter_mut() {
                if row.cells.len() <= col_idx {
                    continue;
                }
                let src_cell = &row.cells[col_idx];
                let new_cell = TableCell {
                    content: vec![CellContent::Paragraph(Paragraph {
                        style: StyleRef("Default".into()),
                        runs: vec![],
                        ..Default::default()
                    })],
                    col_span: 1,
                    row_span: 1,
                    style: None,
                    hidden: false,
                    bg_color: src_cell.bg_color,
                    border_color: src_cell.border_color,
                    border_width: src_cell.border_width,
                    pad_left: None, pad_right: None,
                    pad_top: None, pad_bottom: None,
                };
                let insert_pos = if after { col_idx + 1 } else { col_idx };
                let insert_pos = insert_pos.min(row.cells.len());
                row.cells.insert(insert_pos, new_cell);
            }
            Ok(())
        }))
    });

    // table_delete_column(body_idx, col_idx) -> "ok" | "err:原因"
    // 越界报错;只剩 1 列时拒绝删除并报错。cells.len() <= col_idx 的行防御性跳过。
    engine.register_fn("table_delete_column", |body_idx: i64, col_idx: i64| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            let table = table_of!(doc, body_idx);
            let ncols = table.col_widths.len();
            if col_idx < 0 || col_idx as usize >= ncols {
                return Err(format!("err:col_idx={} 越界(该表共 {} 列)", col_idx, ncols));
            }
            if ncols <= 1 {
                return Err("err:不能删除最后一列(表格不能为空)".to_string());
            }
            let col_idx = col_idx as usize;
            table.col_widths.remove(col_idx);
            for row in table.rows.iter_mut() {
                if row.cells.len() <= col_idx { continue; }
                row.cells.remove(col_idx);
            }
            Ok(())
        }))
    });

    // ── 表格列宽/行高设置 ──────────────────────────────────────────
    // 仅支持顶层表格。body_idx/col_idx/row_idx 非法、数值<=0 均返回 "err:原因"。

    // set_table_col_width(body_idx, col_idx, width_pt) -> "ok" | "err:原因"
    // width_pt <= 0 报错。col_widths 是 Vec<f32>，无"清空恢复默认"语义，故无 clear 函数。
    engine.register_fn("set_table_col_width", |body_idx: i64, col_idx: i64, width_pt: f64| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            if !(width_pt > 0.0) {
                return Err(format!("err:width_pt={} 必须大于 0", width_pt));
            }
            let table = table_of!(doc, body_idx);
            let ncols = table.col_widths.len();
            if col_idx < 0 || col_idx as usize >= ncols {
                return Err(format!("err:col_idx={} 越界(该表共 {} 列)", col_idx, ncols));
            }
            table.col_widths[col_idx as usize] = width_pt as f32;
            Ok(())
        }))
    });

    // set_table_row_height(body_idx, row_idx, height_pt) -> "ok" | "err:原因"
    // height_pt <= 0 报错。
    engine.register_fn("set_table_row_height", |body_idx: i64, row_idx: i64, height_pt: f64| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            if !(height_pt > 0.0) {
                return Err(format!("err:height_pt={} 必须大于 0", height_pt));
            }
            let table = table_of!(doc, body_idx);
            let nrows = table.rows.len();
            if row_idx < 0 || row_idx as usize >= nrows {
                return Err(format!("err:row_idx={} 越界(该表共 {} 行)", row_idx, nrows));
            }
            table.rows[row_idx as usize].row_height = Some(height_pt as f32);
            Ok(())
        }))
    });

    // clear_table_row_height(body_idx, row_idx) -> "ok" | "err:原因"
    // 恢复 row_height 为 None(渲染层按内容自动测量)。
    engine.register_fn("clear_table_row_height", |body_idx: i64, row_idx: i64| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            let table = table_of!(doc, body_idx);
            let nrows = table.rows.len();
            if row_idx < 0 || row_idx as usize >= nrows {
                return Err(format!("err:row_idx={} 越界(该表共 {} 行)", row_idx, nrows));
            }
            table.rows[row_idx as usize].row_height = None;
            Ok(())
        }))
    });

    // ── 表格样式绑定（style_id + 6个bool开关，各自独立小函数，
    //    渲染层slide_renderer.rs确认六个开关之间无互斥/原子性要求）──

    // ── 表格样式绑定、边框:body_idx 非表格/越界返回 err:;颜色分量须在 0~255;宽度须大于 0 ──

    // set_table_style_id -> "ok" | "err:原因"
    // 设置表格样式 id(不校验样式是否存在)。
    engine.register_fn("set_table_style_id", |body_idx: i64, style_id: String| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            let table = table_of!(doc, body_idx);
            table.style_id = Some(style_id);
            Ok(())
        }))
    });

    // clear_table_style_id -> "ok" | "err:原因"
    // 清除表格样式 id。
    engine.register_fn("clear_table_style_id", |body_idx: i64| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            let table = table_of!(doc, body_idx);
            table.style_id = None;
            Ok(())
        }))
    });

    // set_table_first_row -> "ok" | "err:原因"
    // 设置表格样式开关 first_row。
    engine.register_fn("set_table_first_row", |body_idx: i64, value: bool| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            let table = table_of!(doc, body_idx);
            table.first_row = value;
            Ok(())
        }))
    });

    // set_table_first_col -> "ok" | "err:原因"
    // 设置表格样式开关 first_col。
    engine.register_fn("set_table_first_col", |body_idx: i64, value: bool| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            let table = table_of!(doc, body_idx);
            table.first_col = value;
            Ok(())
        }))
    });

    // set_table_last_row -> "ok" | "err:原因"
    // 设置表格样式开关 last_row。
    engine.register_fn("set_table_last_row", |body_idx: i64, value: bool| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            let table = table_of!(doc, body_idx);
            table.last_row = value;
            Ok(())
        }))
    });

    // set_table_last_col -> "ok" | "err:原因"
    // 设置表格样式开关 last_col。
    engine.register_fn("set_table_last_col", |body_idx: i64, value: bool| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            let table = table_of!(doc, body_idx);
            table.last_col = value;
            Ok(())
        }))
    });

    // set_table_band_row -> "ok" | "err:原因"
    // 设置表格样式开关 band_row。
    engine.register_fn("set_table_band_row", |body_idx: i64, value: bool| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            let table = table_of!(doc, body_idx);
            table.band_row = value;
            Ok(())
        }))
    });

    // set_table_band_col -> "ok" | "err:原因"
    // 设置表格样式开关 band_col。
    engine.register_fn("set_table_band_col", |body_idx: i64, value: bool| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            let table = table_of!(doc, body_idx);
            table.band_col = value;
            Ok(())
        }))
    });

    // set_table_default_border_color -> "ok" | "err:原因"
    // r/g/b 须在 0~255,否则报错。
    engine.register_fn("set_table_default_border_color", |body_idx: i64, r: i64, g: i64, b: i64| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            let table = table_of!(doc, body_idx);
            for (nm, v) in [("r", r), ("g", g), ("b", b)] {
                if !(0..=255).contains(&v) {
                    return Err(format!("err:颜色分量 {}={} 超出 0~255", nm, v));
                }
            }
            table.default_border_color = Some(Color { r: r as u8, g: g as u8, b: b as u8, a: 255 });
            Ok(())
        }))
    });

    // clear_table_default_border_color -> "ok" | "err:原因"
    // 清除边框颜色。
    engine.register_fn("clear_table_default_border_color", |body_idx: i64| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            let table = table_of!(doc, body_idx);
            table.default_border_color = None;
            Ok(())
        }))
    });

    // set_table_default_border_width -> "ok" | "err:原因"
    // width_pt 必须大于 0,否则报错。
    engine.register_fn("set_table_default_border_width", |body_idx: i64, width_pt: f64| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            let table = table_of!(doc, body_idx);
            if !(width_pt > 0.0) {
                return Err(format!("err:width_pt={} 必须大于 0", width_pt));
            }
            table.default_border_width = Some(width_pt as f32);
            Ok(())
        }))
    });

    // clear_table_default_border_width -> "ok" | "err:原因"
    // 清除边框宽度。
    engine.register_fn("clear_table_default_border_width", |body_idx: i64| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            let table = table_of!(doc, body_idx);
            table.default_border_width = None;
            Ok(())
        }))
    });

    // set_table_inside_h_border_color -> "ok" | "err:原因"
    // r/g/b 须在 0~255,否则报错。
    engine.register_fn("set_table_inside_h_border_color", |body_idx: i64, r: i64, g: i64, b: i64| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            let table = table_of!(doc, body_idx);
            for (nm, v) in [("r", r), ("g", g), ("b", b)] {
                if !(0..=255).contains(&v) {
                    return Err(format!("err:颜色分量 {}={} 超出 0~255", nm, v));
                }
            }
            table.inside_h_color = Some(Color { r: r as u8, g: g as u8, b: b as u8, a: 255 });
            Ok(())
        }))
    });

    // clear_table_inside_h_border_color -> "ok" | "err:原因"
    // 清除边框颜色。
    engine.register_fn("clear_table_inside_h_border_color", |body_idx: i64| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            let table = table_of!(doc, body_idx);
            table.inside_h_color = None;
            Ok(())
        }))
    });

    // set_table_inside_h_border_width -> "ok" | "err:原因"
    // width_pt 必须大于 0,否则报错。
    engine.register_fn("set_table_inside_h_border_width", |body_idx: i64, width_pt: f64| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            let table = table_of!(doc, body_idx);
            if !(width_pt > 0.0) {
                return Err(format!("err:width_pt={} 必须大于 0", width_pt));
            }
            table.inside_h_width = Some(width_pt as f32);
            Ok(())
        }))
    });

    // clear_table_inside_h_border_width -> "ok" | "err:原因"
    // 清除边框宽度。
    engine.register_fn("clear_table_inside_h_border_width", |body_idx: i64| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            let table = table_of!(doc, body_idx);
            table.inside_h_width = None;
            Ok(())
        }))
    });

    // set_table_inside_v_border_color -> "ok" | "err:原因"
    // r/g/b 须在 0~255,否则报错。
    engine.register_fn("set_table_inside_v_border_color", |body_idx: i64, r: i64, g: i64, b: i64| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            let table = table_of!(doc, body_idx);
            for (nm, v) in [("r", r), ("g", g), ("b", b)] {
                if !(0..=255).contains(&v) {
                    return Err(format!("err:颜色分量 {}={} 超出 0~255", nm, v));
                }
            }
            table.inside_v_color = Some(Color { r: r as u8, g: g as u8, b: b as u8, a: 255 });
            Ok(())
        }))
    });

    // clear_table_inside_v_border_color -> "ok" | "err:原因"
    // 清除边框颜色。
    engine.register_fn("clear_table_inside_v_border_color", |body_idx: i64| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            let table = table_of!(doc, body_idx);
            table.inside_v_color = None;
            Ok(())
        }))
    });

    // set_table_inside_v_border_width -> "ok" | "err:原因"
    // width_pt 必须大于 0,否则报错。
    engine.register_fn("set_table_inside_v_border_width", |body_idx: i64, width_pt: f64| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            let table = table_of!(doc, body_idx);
            if !(width_pt > 0.0) {
                return Err(format!("err:width_pt={} 必须大于 0", width_pt));
            }
            table.inside_v_width = Some(width_pt as f32);
            Ok(())
        }))
    });

    // clear_table_inside_v_border_width -> "ok" | "err:原因"
    // 清除边框宽度。
    engine.register_fn("clear_table_inside_v_border_width", |body_idx: i64| -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            let table = table_of!(doc, body_idx);
            table.inside_v_width = None;
            Ok(())
        }))
    });


    // generate_toc()
    // 扫描 Heading 段落，在文档开头插入目录段落列表
    engine.register_fn("generate_toc", || -> String {
        write_ret(with_doc(|doc| -> Result<(), String> {
            // 收集所有标题
            let headings: Vec<(String, String)> = doc.body.iter().filter_map(|e| {
                if let BodyElement::Paragraph(p) = e {
                    let style = &p.style.0;
                    if style.to_lowercase().contains("heading") ||
                       style.to_lowercase().contains("h1") ||
                       style.to_lowercase().contains("h2") ||
                       style.to_lowercase().contains("h3") {
                        let text = p.runs.iter().map(|r| r.text.as_str()).collect::<String>();
                        if !text.is_empty() {
                            return Some((style.clone(), text));
                        }
                    }
                }
                None
            }).collect();

            if headings.is_empty() {
                return Err("err:没有标题段落(段落样式名需含 heading/h1/h2/h3)".to_string());
            }

            // 插入目录标题段落
            let mut toc_paras: Vec<BodyElement> = vec![
                BodyElement::Paragraph(Paragraph {
                    style: StyleRef("Heading1".into()),
                    runs:  vec![Run { text: "目录".into(), ..Default::default() }],
                    ..Default::default()
                }),
            ];

            // 每个标题生成一行目录条目
            for (_style, text) in &headings {
                toc_paras.push(BodyElement::Paragraph(Paragraph {
                    style: StyleRef("Contents1".into()),
                    runs:  vec![Run { text: text.clone(), ..Default::default() }],
                    ..Default::default()
                }));
            }

            // 插入到文档开头
            let mut new_body = toc_paras;
            new_body.append(&mut doc.body);
            doc.body = new_body;
            Ok(())
        }))
    });

    // save_docx() -> String
    // 把当前 WriterDocument 序列化为 docx 字节流，返回 base64 编码字符串。
    // 与 roe_save_document_to_ooxml_bytes（C-ABI）走同一条 DocxWriter，
    // 保证脚本层和宿主语言调用层产出字节完全一致。写出范围对齐
    // docx_writer.rs 文件头注释（段落+样式名+run文字，不含表格/图片/
    // 页眉页脚等，详见该文件头注释）。
    // 失败（write 内部报错，如极端情况下的编码问题）返回空字符串，
    // 脚本侧应对空字符串做判断，不要假设总是成功。
}

// ── 从 register 内提出的辅助函数(只读函数集要用) ──
    fn alignment_to_str(a: &Option<Alignment>) -> &'static str {
        match a {
            Some(Alignment::Left)    => "left",
            Some(Alignment::Center)  => "center",
            Some(Alignment::Right)   => "right",
            Some(Alignment::Justify) => "justify",
            None                     => "",
        }
    }

    fn script_position_to_str(s: &Option<ScriptPosition>) -> &'static str {
        match s {
            Some(ScriptPosition::Superscript) => "superscript",
            Some(ScriptPosition::Subscript)   => "subscript",
            Some(ScriptPosition::Normal) | None => "normal",
        }
    }

    fn json_color(c: &Option<Color>) -> serde_json::Value {
        match c {
            Some(col) => serde_json::json!({"r": col.r, "g": col.g, "b": col.b, "a": col.a}),
            None => serde_json::Value::Null,
        }
    }

/// 只读函数集(纯查询,不修改文档)。register 与 register_readonly 共用。
fn register_read_fns(engine: &mut Engine) {
    engine.register_fn("paragraph_count", || -> i64 {
        with_doc(|doc| doc.body.len() as i64).unwrap_or(0)
    });

    engine.register_fn("get_paragraph", |idx: i64| -> String {
        with_doc(|doc| {
            doc.body.get(idx as usize).and_then(|e| match e {
                BodyElement::Paragraph(p) => Some(para_text(p)),
                _ => None,
            })
        })
        .flatten()
        .unwrap_or_default()
    });

    engine.register_fn("get_paragraph_style", |name: String| -> String {
        with_doc(|doc| {
            if let Some(s) = doc.styles.paragraph_styles.get(&name) {
                return serde_json::json!({
                    "name":              s.name,
                    "based_on":          s.based_on,
                    "font_size":         s.font_size,
                    "font_family":       s.font_family,
                    "font_family_ascii": s.font_family_ascii,
                    "bold":              s.bold,
                    "italic":            s.italic,
                    "alignment":         alignment_to_str(&s.alignment),
                    "line_height":       s.line_height,
                    "space_before":      s.space_before,
                    "space_after":       s.space_after,
                    "indent_left":       s.indent_left,
                    "indent_first":      s.indent_first,
                    "color":             json_color(&s.color),
                    "list_style_name":   s.list_style_name,
                    "list_level":        s.list_level,
                }).to_string();
            }
            "{}".to_string()
        })
        .unwrap_or_else(|| "{}".to_string())
    });

    engine.register_fn("list_paragraph_styles", || -> String {
        with_doc(|doc| {
            let names: Vec<&String> = doc.styles.paragraph_styles.keys().collect();
            serde_json::json!(names).to_string()
        })
        .unwrap_or_else(|| "[]".to_string())
    });

    engine.register_fn("get_char_style", |name: String| -> String {
        with_doc(|doc| {
            if let Some(s) = doc.styles.char_styles.get(&name) {
                return serde_json::json!({
                    "name":              s.name,
                    "based_on":          s.based_on,
                    "font_size":         s.font_size,
                    "font_family":       s.font_family,
                    "font_family_ascii": s.font_family_ascii,
                    "bold":              s.bold,
                    "italic":            s.italic,
                    "underline":         s.underline,
                    "color":             json_color(&s.color),
                    "strikethrough":     s.strikethrough,
                    "background":        json_color(&s.background),
                    "script":            script_position_to_str(&s.script),
                }).to_string();
            }
            "{}".to_string()
        })
        .unwrap_or_else(|| "{}".to_string())
    });

    engine.register_fn("list_char_styles", || -> String {
        with_doc(|doc| {
            let names: Vec<&String> = doc.styles.char_styles.keys().collect();
            serde_json::json!(names).to_string()
        })
        .unwrap_or_else(|| "[]".to_string())
    });

    engine.register_fn("get_char_format", |para_idx: i64, run_idx: i64| -> String {
        with_doc(|doc| {
            if let Some(BodyElement::Paragraph(p)) = doc.body.get(para_idx as usize) {
                if let Some(run) = p.runs.get(run_idx as usize) {
                    let script_str = match run.script {
                        Some(ScriptPosition::Superscript) => "superscript",
                        Some(ScriptPosition::Subscript)   => "subscript",
                        _                                  => "normal",
                    };
                    let underline_style_str = match run.underline_style {
                        Some(UnderlineStyle::Double) => "double",
                        Some(UnderlineStyle::Wave)   => "wave",
                        Some(UnderlineStyle::None)   => "none",
                        _                             => "single",
                    };
                    return serde_json::json!({
                        "bold":          run.bold.unwrap_or(false),
                        "italic":        run.italic.unwrap_or(false),
                        "underline":     run.underline.unwrap_or(false),
                        "underline_style": underline_style_str,
                        "font_size":     run.font_size,
                        "font_family":   run.font_family,
                        "font_family_ascii": run.font_family_ascii,
                        "color":         run.color.map(|c| format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b)),
                        "background":    run.background.map(|c| format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b)),
                        "strikethrough": run.strikethrough.unwrap_or(false),
                        "script":        script_str,
                        "char_style":    run.char_style.as_ref().map(|s| &s.0),
                    }).to_string();
                }
            }
            "{}".to_string()
        })
        .unwrap_or_else(|| "{}".to_string())
    });

    engine.register_fn("get_char_format_range",
        |para_idx: i64, start_offset: i64, end_offset: i64| -> String {
        use roe_writer::char_format::{
            get_bool_flag_in_range, get_uniform_or_mixed_in_range, get_option_value_in_range,
            BoolFlagResult, RangeQueryResult,
        };

        let result = with_doc(|doc| {
            let p = match doc.body.get(para_idx as usize) {
                Some(BodyElement::Paragraph(p)) => p,
                _ => return serde_json::json!({
                    "error": format!("get_char_format_range: 段落索引 {para_idx} 不存在")
                }).to_string(),
            };
            let rs = start_offset as usize;
            let re = end_offset as usize;

            macro_rules! bool_field {
                ($get:expr) => {
                    match get_bool_flag_in_range(p, rs, re, $get) {
                        Ok(BoolFlagResult::AllTrue) => serde_json::json!(true),
                        Ok(BoolFlagResult::AllFalse) => serde_json::json!(false),
                        Ok(BoolFlagResult::Mixed) => serde_json::json!("mixed"),
                        Err(e) => return serde_json::json!({
                            "error": format!("get_char_format_range: {e}")
                        }).to_string(),
                    }
                };
            }

            let bold_v = bool_field!(|r| r.bold);
            let italic_v = bool_field!(|r| r.italic);
            let underline_v = bool_field!(|r| r.underline);

            let font_size_v = match get_uniform_or_mixed_in_range(
                p, rs, re,
                |r| r.font_size,
                |a: &f32, b: &f32| (a - b).abs() <= f32::EPSILON,
            ) {
                Ok(RangeQueryResult::Uniform(v)) => serde_json::json!(v),
                Ok(RangeQueryResult::Mixed) => serde_json::json!("mixed"),
                Ok(RangeQueryResult::Empty) => serde_json::Value::Null,
                Err(e) => return serde_json::json!({
                    "error": format!("get_char_format_range: {e}")
                }).to_string(),
            };

            let font_family_v = match get_uniform_or_mixed_in_range(
                p, rs, re,
                |r| r.font_family.clone(),
                |a: &String, b: &String| a == b,
            ) {
                Ok(RangeQueryResult::Uniform(v)) => serde_json::json!(v),
                Ok(RangeQueryResult::Mixed) => serde_json::json!("mixed"),
                Ok(RangeQueryResult::Empty) => serde_json::Value::Null,
                Err(e) => return serde_json::json!({
                    "error": format!("get_char_format_range: {e}")
                }).to_string(),
            };

            let font_family_ascii_v = match get_uniform_or_mixed_in_range(
                p, rs, re,
                |r| r.font_family_ascii.clone(),
                |a: &String, b: &String| a == b,
            ) {
                Ok(RangeQueryResult::Uniform(v)) => serde_json::json!(v),
                Ok(RangeQueryResult::Mixed) => serde_json::json!("mixed"),
                Ok(RangeQueryResult::Empty) => serde_json::Value::Null,
                Err(e) => return serde_json::json!({
                    "error": format!("get_char_format_range: {e}")
                }).to_string(),
            };

            let underline_style_v = match get_option_value_in_range(p, rs, re, |r| r.underline_style) {
                Ok(RangeQueryResult::Uniform(style_opt)) => {
                    let s = match style_opt {
                        Some(UnderlineStyle::Double) => "double",
                        Some(UnderlineStyle::Wave) => "wave",
                        Some(UnderlineStyle::None) => "none",
                        Some(UnderlineStyle::Single) => "single",
                        None => "single", // 无内联覆盖，对齐 get_char_format 既有惯例
                    };
                    serde_json::json!(s)
                }
                Ok(RangeQueryResult::Mixed) => serde_json::json!("mixed"),
                Ok(RangeQueryResult::Empty) => serde_json::json!("single"),
                Err(e) => return serde_json::json!({
                    "error": format!("get_char_format_range: {e}")
                }).to_string(),
            };

            serde_json::json!({
                "bold": bold_v,
                "italic": italic_v,
                "underline": underline_v,
                "underline_style": underline_style_v,
                "font_size": font_size_v,
                "font_family": font_family_v,
                "font_family_ascii": font_family_ascii_v,
            }).to_string()
        });

        result.unwrap_or_else(|| serde_json::json!({
            "error": "get_char_format_range: 未打开文档"
        }).to_string())
    });

    engine.register_fn("get_paragraph_format", |para_idx: i64| -> String {
        with_doc(|doc| {
            if let Some(BodyElement::Paragraph(p)) = doc.body.get(para_idx as usize) {
                let align_str = match p.alignment {
                    Some(Alignment::Left)    => "left",
                    Some(Alignment::Center)  => "center",
                    Some(Alignment::Right)   => "right",
                    Some(Alignment::Justify) => "justify",
                    None                     => "",
                };
                return serde_json::json!({
                    "style":          p.style.0,
                    "alignment":      align_str,
                    "indent_left":    p.indent_left,
                    "indent_first":   p.indent_first,
                    "space_before":   p.space_before,
                    "space_after":    p.space_after,
                    "line_height":    p.line_height,
                }).to_string();
            }
            "{}".to_string()
        })
        .unwrap_or_else(|| "{}".to_string())
    });

    engine.register_fn("get_frame_wrap", |body_idx: i64| -> String {
        with_doc(|doc| {
            match doc.body.get(body_idx as usize) {
                Some(BodyElement::Frame(f)) => serde_json::json!({
                    "kind": "frame",
                    "wrap_type": f.wrap_type,
                    "anchor_type_h": f.anchor_type_h,
                    "anchor_type_v": f.anchor_type_v,
                    "align_h": f.align_h,
                    "align_v": f.align_v,
                    "dist_t_pt": f.dist_t_pt,
                    "dist_b_pt": f.dist_b_pt,
                    "dist_l_pt": f.dist_l_pt,
                    "dist_r_pt": f.dist_r_pt,
                }).to_string(),
                Some(BodyElement::Shape(s)) => serde_json::json!({
                    "kind": "shape",
                    "wrap_type": s.wrap_type,
                    "anchor_type_h": s.anchor_type_h,
                    "anchor_type_v": s.anchor_type_v,
                    "align_h": s.align_h,
                    "align_v": s.align_v,
                    "dist_t_pt": s.dist_t_pt,
                    "dist_b_pt": s.dist_b_pt,
                    "dist_l_pt": s.dist_l_pt,
                    "dist_r_pt": s.dist_r_pt,
                }).to_string(),
                _ => "{}".to_string(),
            }
        })
        .unwrap_or_else(|| "{}".to_string())
    });

    engine.register_fn("section_count", || -> i64 {
        with_doc(|doc| doc.sections.len() as i64).unwrap_or(0)
    });

    engine.register_fn("get_section", |idx: i64| -> String {
        with_doc(|doc| {
            if let Some(s) = doc.sections.get(idx as usize) {
                let header_default_text = s.header.as_ref()
                    .and_then(|hf| hf.default.as_ref())
                    .map(|paras| paras.iter().map(para_text).collect::<Vec<_>>().join("\n"));
                let footer_default_text = s.footer.as_ref()
                    .and_then(|hf| hf.default.as_ref())
                    .map(|paras| paras.iter().map(para_text).collect::<Vec<_>>().join("\n"));
                return serde_json::json!({
                    "name":                 s.name,
                    "page_size":            s.page_size,
                    "margin_top":           s.margin_top,
                    "margin_bottom":        s.margin_bottom,
                    "margin_left":          s.margin_left,
                    "margin_right":         s.margin_right,
                    "header_default_text":  header_default_text,
                    "footer_default_text":  footer_default_text,
                    "title_page":           s.title_page,
                    "even_and_odd_headers": s.even_and_odd_headers,
                    "section_type":         s.section_type,
                    "num_cols":             s.columns.as_ref().map(|c| c.num_cols),
                }).to_string();
            }
            "{}".to_string()
        })
        .unwrap_or_else(|| "{}".to_string())
    });

    engine.register_fn("save_docx", || -> String {
        with_doc(|doc| {
            match roe_parser::ooxml::docx_writer::DocxWriter::write(doc) {
                Ok(bytes) => base64::engine::general_purpose::STANDARD.encode(bytes),
                Err(_) => String::new(),
            }
        })
        .unwrap_or_default()
    });

}

/// 只读引擎用:只注册纯查询函数(13 个),不注册任何写函数。
/// 只读函数白名单(与 register_read_fns 注册的名单一致,有测试守着)。
pub(crate) const READ_FN_NAMES: [&str; 22] = [
    "get_writer_formula",
    "paragraph_count", "get_paragraph", "get_paragraph_style", "list_paragraph_styles",
    "get_char_style", "list_char_styles", "get_char_format", "get_char_format_range",
    "get_paragraph_format", "get_frame_wrap", "section_count", "get_section", "save_docx",
    "inspect_document", "get_table_cell", "find_text",
    "get_page_setup", "get_header_footer", "get_comment_threads", "get_object_rotation", "get_writer_chart_json",
];

#[cfg(test)]
pub(crate) fn register_read_fns_for_test(engine: &mut Engine) { register_read_fns(engine); ext::register_read(engine); }

pub fn register_readonly(engine: &mut Engine) {
    register_read_fns(engine);
    ext::register_read(engine);
    crate::calc_api_list::register_writer_readonly_list(engine);
}

fn para_text(p: &Paragraph) -> String {
    p.runs.iter().map(|r| r.text.as_str()).collect::<Vec<_>>().join("")
}

/// WROOT-004: set_header_footer 脚本接口只接受纯文本，包装成单段落单run，
/// 写入 HeaderFooterSet.default（不支持奇偶页/首页，这是纯文本API本身的局限，
/// 不是本次改动引入的能力倒退——脚本层要支持三态需要新增专门的API，留待后续）。
fn plain_text_to_paragraphs(text: &str) -> Vec<roe_model::writer::Paragraph> {
    vec![roe_model::writer::Paragraph {
        runs: vec![roe_model::writer::Run { text: text.to_string(), ..Default::default() }],
        ..Default::default()
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use roe_model::writer::{DocumentMeta, DocumentColumns, HeaderFooterSet, WriterDocument, NumFormat};

    use roe_model::style::StyleRegistry;

    pub(super) fn empty_doc() -> WriterDocument {
        WriterDocument {
            page_size: (595.28, 841.89),
            styles: StyleRegistry::default(),
            comments: Vec::new(),
            body: vec![
                BodyElement::Paragraph(Paragraph {
                    style: StyleRef("Default".into()),
                    runs: vec![Run { text: "Hello World".into(), ..Default::default() }],
                    ..Default::default()
                }),
            ],
            metadata: DocumentMeta::default(),
            header: HeaderFooterSet::default(),
            footer: HeaderFooterSet::default(),
            title_page: false,
            even_and_odd_headers: false,
            columns: DocumentColumns::default(),
            sections: vec![],
            margin_top:    56.69,
            margin_bottom: 56.69,
            margin_left:   70.87,
            margin_right:  70.87,
            footnotes: Vec::new(),
            endnotes: Vec::new(),
            endnote_num_fmt: NumFormat::default(),
            footnote_num_fmt: NumFormat::default(),
        }
    }

    #[test]
    fn test_get_paragraph() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            let text: String = e.eval("get_paragraph(0)").unwrap();
            assert_eq!(text, "Hello World");
        });
    }

    fn eval_str_w(doc: &mut WriterDocument, script: &str) -> String {
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut out = String::new();
        with_document(doc, || {
            out = e.eval::<String>(script).unwrap();
        });
        out
    }

    #[test]
    fn writer_write_fns_return_ok() {
        let mut doc = empty_doc();
        assert_eq!(eval_str_w(&mut doc, r#"set_paragraph(0, "A")"#), "ok");
        assert_eq!(eval_str_w(&mut doc, "set_char_format(0, 0, true, false, false, 12.0)"), "ok");
        assert_eq!(eval_str_w(&mut doc, r#"apply_paragraph_style(0, "Heading1")"#), "ok");
        assert_eq!(eval_str_w(&mut doc, r#"insert_paragraph(99, "B")"#), "ok");
        assert_eq!(eval_str_w(&mut doc, r#"append_paragraph("C")"#), "ok");
        assert_eq!(eval_str_w(&mut doc, "clear_char_format(0, 0)"), "ok");
        assert_eq!(doc.body.len(), 3);
    }

    #[test]
    fn writer_batch2_ok_and_err() {
        let mut doc = empty_doc();
        assert_eq!(eval_str_w(&mut doc, "set_char_color(0, 0, 255, 0, 0)"), "ok");
        let r = eval_str_w(&mut doc, "set_char_color(0, 0, 256, 0, 0)");
        assert!(r.starts_with("err:") && r.contains("r=256"), "{r}");
        assert_eq!(eval_str_w(&mut doc, r#"set_char_font(0, 0, "宋体", "")"#), "ok");
        assert_eq!(eval_str_w(&mut doc, r#"insert_hyperlink(0, 99, "http://x", "t")"#), "ok");
        assert_eq!(eval_str_w(&mut doc, r#"set_list_format(0, "bullet", 99)"#), "ok");
        let r = eval_str_w(&mut doc, r#"set_list_format(0, "bullet", -1)"#);
        assert!(r.starts_with("err:"), "{r}");
        let r = eval_str_w(&mut doc, r#"set_char_font(99, 0, "x", "")"#);
        assert!(r.starts_with("err:"), "{r}");
    }

    #[test]
    fn writer_paragraph_format_bad_align_keeps_old() {
        let mut doc = empty_doc();
        assert_eq!(eval_str_w(&mut doc, r#"set_paragraph_format(0, "right", -1.0, -1.0, -1.0, -1.0)"#), "ok");
        let r = eval_str_w(&mut doc, r#"set_paragraph_format(0, "centre", -1.0, -1.0, -1.0, -1.0)"#);
        assert!(r.starts_with("err:") && r.contains("align"), "{r}");
        match &doc.body[0] {
            BodyElement::Paragraph(p) => assert!(matches!(p.alignment, Some(Alignment::Right))),
            _ => panic!("not paragraph"),
        }
    }

    #[test]
    fn writer_insert_image_validates() {
        let mut doc = empty_doc();
        let r = eval_str_w(&mut doc, r#"insert_image(0, "!!!不是base64", 10.0, 10.0)"#);
        assert!(r.starts_with("err:"), "{r}");
        let r = eval_str_w(&mut doc, r#"insert_image(0, "iVBORw0KGgo=", 0.0, 10.0)"#);
        assert!(r.starts_with("err:"), "{r}");
        assert_eq!(doc.body.len(), 1);
        assert_eq!(eval_str_w(&mut doc, r#"insert_image(0, "iVBORw0KGgo=", 10.0, 10.0)"#), "ok");
        assert_eq!(doc.body.len(), 2);
    }

    #[test]
    fn writer_insert_table_ok_and_limits() {
        let mut doc = empty_doc();
        assert_eq!(eval_str_w(&mut doc, "insert_table(99, 2, 3)"), "ok");
        assert_eq!(doc.body.len(), 2);
        let r = eval_str_w(&mut doc, "insert_table(0, 1000, 3)");
        assert!(r.starts_with("err:") && r.contains("rows=1000"), "{r}");
        assert_eq!(doc.body.len(), 2);
    }

    #[test]
    fn writer_write_fns_report_out_of_range() {
        let mut doc = empty_doc();
        let r = eval_str_w(&mut doc, r#"set_paragraph(99, "x")"#);
        assert!(r.starts_with("err:") && r.contains("99"), "{r}");
        let r = eval_str_w(&mut doc, r#"apply_paragraph_style(99, "Heading1")"#);
        assert!(r.starts_with("err:"), "{r}");
        let r = eval_str_w(&mut doc, "set_char_format(0, 99, true, false, false, 12.0)");
        assert!(r.starts_with("err:") && r.contains("run_idx=99"), "{r}");
        let r = eval_str_w(&mut doc, "clear_char_format(0, 99)");
        assert!(r.starts_with("err:") && r.contains("run_idx=99"), "{r}");
        // 失败不改动文档
        assert_eq!(doc.body.len(), 1);
    }

    #[test]
    fn test_set_paragraph() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("set_paragraph(0, \"Updated\")").unwrap();
            let text: String = e.eval("get_paragraph(0)").unwrap();
            assert_eq!(text, "Updated");
        });
    }

    #[test]
    fn test_insert_paragraph() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("insert_paragraph(0, \"Inserted\")").unwrap();
            let n: i64 = e.eval("paragraph_count()").unwrap();
            assert_eq!(n, 2);
            let text: String = e.eval("get_paragraph(0)").unwrap();
            assert_eq!(text, "Inserted");
        });
    }

    #[test]
    fn test_append_paragraph() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("append_paragraph(\"Appended\")").unwrap();
            let n: i64 = e.eval("paragraph_count()").unwrap();
            assert_eq!(n, 2);
            let text: String = e.eval("get_paragraph(1)").unwrap();
            assert_eq!(text, "Appended");
        });
    }

    #[test]
    fn test_apply_paragraph_style() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("apply_paragraph_style(0, \"Heading1\")").unwrap();
            let fmt: String = e.eval("get_paragraph_format(0)").unwrap();
            let v: serde_json::Value = serde_json::from_str(&fmt).unwrap();
            assert_eq!(v["style"], "Heading1");
        });
    }

    #[test]
    fn test_set_get_char_format() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("set_char_format(0, 0, true, false, true, 14.0)").unwrap();
            let fmt: String = e.eval("get_char_format(0, 0)").unwrap();
            let v: serde_json::Value = serde_json::from_str(&fmt).unwrap();
            assert_eq!(v["bold"], true);
            assert_eq!(v["underline"], true);
            assert_eq!(v["italic"], false);
            assert!((v["font_size"].as_f64().unwrap() - 14.0).abs() < 0.01);
        });
    }

    #[test]
    fn test_replace_text_global_replace_counts_matches() {
        let mut doc = empty_doc(); // 单段落 "Hello World"
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            let result: String = e.eval(
                r#"replace_text("World", "Rust", true, -1, -1)"#
            ).unwrap();
            let v: serde_json::Value = serde_json::from_str(&result).unwrap();
            assert_eq!(v["total_count"], 1);
            assert_eq!(v["affected_para_indices"], serde_json::json!([0]));

            let text: String = e.eval("get_paragraph(0)").unwrap();
            assert_eq!(text, "Hello Rust");
        });
    }

    #[test]
    fn test_replace_text_regex_support() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            // 段落原文 "Hello World"，用字符类 [A-Za-z]+ 匹配每个单词，
            // 全部替换成 "X"（避开 \w：Rhai 脚本字符串字面量自身不
            // 认识 \w 这个转义序列会解析报错，[A-Za-z]+ 等价且不需要
            // 处理"正则转义符再套一层脚本语言转义"的双重转义问题）。
            let result: String = e.eval(
                r#"replace_text("[A-Za-z]+", "X", true, -1, -1)"#
            ).unwrap();
            let v: serde_json::Value = serde_json::from_str(&result).unwrap();
            assert_eq!(v["total_count"], 2);
            let text: String = e.eval("get_paragraph(0)").unwrap();
            assert_eq!(text, "X X");
        });
    }

    #[test]
    fn test_replace_text_case_insensitive_option() {
        let mut doc = empty_doc(); // "Hello World"
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            // 区分大小写：小写 "hello" 不应匹配 "Hello"
            let result_sensitive: String = e.eval(
                r#"replace_text("hello", "Hi", true, -1, -1)"#
            ).unwrap();
            let v1: serde_json::Value = serde_json::from_str(&result_sensitive).unwrap();
            assert_eq!(v1["total_count"], 0);

            // 不区分大小写：应能匹配
            let result_insensitive: String = e.eval(
                r#"replace_text("hello", "Hi", false, -1, -1)"#
            ).unwrap();
            let v2: serde_json::Value = serde_json::from_str(&result_insensitive).unwrap();
            assert_eq!(v2["total_count"], 1);
            let text: String = e.eval("get_paragraph(0)").unwrap();
            assert_eq!(text, "Hi World");
        });
    }

    #[test]
    fn test_replace_text_respects_para_range() {
        let mut doc = empty_doc(); // 段落0: "Hello World"
        doc.body.push(BodyElement::Paragraph(Paragraph {
            style: StyleRef("Default".into()),
            runs: vec![Run { text: "Hello Again".into(), ..Default::default() }],
            ..Default::default()
        }));
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            // 只处理段落范围 [1, 2)，段落0应保持不变
            let result: String = e.eval(
                r#"replace_text("Hello", "Hi", true, 1, 2)"#
            ).unwrap();
            let v: serde_json::Value = serde_json::from_str(&result).unwrap();
            assert_eq!(v["total_count"], 1);
            assert_eq!(v["affected_para_indices"], serde_json::json!([1]));

            let text0: String = e.eval("get_paragraph(0)").unwrap();
            assert_eq!(text0, "Hello World", "段落0不在范围内，应保持不变");
            let text1: String = e.eval("get_paragraph(1)").unwrap();
            assert_eq!(text1, "Hi Again");
        });
    }

    #[test]
    fn test_replace_text_negative_range_means_whole_document() {
        let mut doc = empty_doc();
        doc.body.push(BodyElement::Paragraph(Paragraph {
            style: StyleRef("Default".into()),
            runs: vec![Run { text: "Hello Again".into(), ..Default::default() }],
            ..Default::default()
        }));
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            // start_para/end_para 传 -1 表示不限定范围，应处理全部段落
            let result: String = e.eval(
                r#"replace_text("Hello", "Hi", true, -1, -1)"#
            ).unwrap();
            let v: serde_json::Value = serde_json::from_str(&result).unwrap();
            assert_eq!(v["total_count"], 2);
            assert_eq!(v["affected_para_indices"], serde_json::json!([0, 1]));
        });
    }

    #[test]
    fn test_replace_text_invalid_regex_returns_error() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            let result: String = e.eval(
                r#"replace_text("(unclosed", "X", true, -1, -1)"#
            ).unwrap();
            let v: serde_json::Value = serde_json::from_str(&result).unwrap();
            assert!(v.get("error").is_some(), "非法正则应返回 error 字段: {result}");
        });
    }

    #[test]
    fn test_replace_text_empty_replacement_is_deletion() {
        let mut doc = empty_doc(); // "Hello World"
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            let result: String = e.eval(
                r#"replace_text(" World", "", true, -1, -1)"#
            ).unwrap();
            let v: serde_json::Value = serde_json::from_str(&result).unwrap();
            assert_eq!(v["total_count"], 1);
            let text: String = e.eval("get_paragraph(0)").unwrap();
            assert_eq!(text, "Hello");
        });
    }

    #[test]
    fn test_char_format_range_basic_set_and_get() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            // "Hello World" 长度11字节，对全区间 [0,11) 设置 bold=true, font_size=14
            let err: String = e.eval("set_char_format_range(0, 0, 11, 1, -1, -1, 14.0, \"\", \"\", \"\")").unwrap();
            assert_eq!(err, "", "set_char_format_range 不应报错: {err}");
            let fmt: String = e.eval("get_char_format_range(0, 0, 11)").unwrap();
            let v: serde_json::Value = serde_json::from_str(&fmt).unwrap();
            assert_eq!(v["bold"], true);
            assert!((v["font_size"].as_f64().unwrap() - 14.0).abs() < 0.01);
        });
    }

    #[test]
    fn test_char_format_range_bold_mixed() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            // 只把前5字节("Hello")设为 bold=true，其余保持默认(None/false)
            e.eval::<String>("set_char_format_range(0, 0, 5, 1, -1, -1, -1.0, \"\", \"\", \"\")").unwrap();
            let fmt: String = e.eval("get_char_format_range(0, 0, 11)").unwrap();
            let v: serde_json::Value = serde_json::from_str(&fmt).unwrap();
            // A类语义：None按false计入，一半true一半false => Mixed
            assert_eq!(v["bold"], "mixed");
        });
    }

    #[test]
    fn test_char_format_range_font_size_empty_and_mixed() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            // 全部未设置font_size时 => Empty => null（B类语义）
            let fmt: String = e.eval("get_char_format_range(0, 0, 11)").unwrap();
            let v: serde_json::Value = serde_json::from_str(&fmt).unwrap();
            assert_eq!(v["font_size"], serde_json::Value::Null);

            // 只给前5字节设置font_size，其余保持None（B类：None跳过，不触发mixed）
            e.eval::<String>("set_char_format_range(0, 0, 5, -1, -1, -1, 14.0, \"\", \"\", \"\")").unwrap();
            let fmt2: String = e.eval("get_char_format_range(0, 0, 11)").unwrap();
            let v2: serde_json::Value = serde_json::from_str(&fmt2).unwrap();
            assert!((v2["font_size"].as_f64().unwrap() - 14.0).abs() < 0.01, "B类语义：None应被跳过，不应变成mixed");

            // 再给后半段设置不同的font_size，这次应该mixed
            e.eval::<String>("set_char_format_range(0, 5, 11, -1, -1, -1, 20.0, \"\", \"\", \"\")").unwrap();
            let fmt3: String = e.eval("get_char_format_range(0, 0, 11)").unwrap();
            let v3: serde_json::Value = serde_json::from_str(&fmt3).unwrap();
            assert_eq!(v3["font_size"], "mixed");
        });
    }

    #[test]
    fn test_char_format_range_underline_style_clear_and_set() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            // 全部未设置时（C类语义）：Uniform(None) => "single"（对齐get_char_format既有惯例）
            let fmt: String = e.eval("get_char_format_range(0, 0, 11)").unwrap();
            let v: serde_json::Value = serde_json::from_str(&fmt).unwrap();
            assert_eq!(v["underline_style"], "single");

            // 设置为double
            e.eval::<String>("set_char_format_range(0, 0, 11, -1, -1, -1, -1.0, \"\", \"\", \"double\")").unwrap();
            let fmt2: String = e.eval("get_char_format_range(0, 0, 11)").unwrap();
            let v2: serde_json::Value = serde_json::from_str(&fmt2).unwrap();
            assert_eq!(v2["underline_style"], "double");

            // 再clear掉，回退默认
            e.eval::<String>("set_char_format_range(0, 0, 11, -1, -1, -1, -1.0, \"\", \"\", \"clear\")").unwrap();
            let fmt3: String = e.eval("get_char_format_range(0, 0, 11)").unwrap();
            let v3: serde_json::Value = serde_json::from_str(&fmt3).unwrap();
            assert_eq!(v3["underline_style"], "single");
        });
    }

    #[test]
    fn test_char_format_range_font_family_ascii_set_and_mixed() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            // 全部未设置时 => Empty => null（B类语义，与font_family同款）
            let fmt: String = e.eval("get_char_format_range(0, 0, 11)").unwrap();
            let v: serde_json::Value = serde_json::from_str(&fmt).unwrap();
            assert_eq!(v["font_family_ascii"], serde_json::Value::Null);

            // 设置前5字节为 "Arial"
            e.eval::<String>("set_char_format_range(0, 0, 5, -1, -1, -1, -1.0, \"\", \"Arial\", \"\")").unwrap();
            let fmt2: String = e.eval("get_char_format_range(0, 0, 5)").unwrap();
            let v2: serde_json::Value = serde_json::from_str(&fmt2).unwrap();
            assert_eq!(v2["font_family_ascii"], "Arial");

            // 后半段设置不同值，应mixed
            e.eval::<String>("set_char_format_range(0, 5, 11, -1, -1, -1, -1.0, \"\", \"Calibri\", \"\")").unwrap();
            let fmt3: String = e.eval("get_char_format_range(0, 0, 11)").unwrap();
            let v3: serde_json::Value = serde_json::from_str(&fmt3).unwrap();
            assert_eq!(v3["font_family_ascii"], "mixed");

            // 确认 font_family（主字体名）不受影响，仍为null（两个字段独立）
            assert_eq!(v3["font_family"], serde_json::Value::Null);
        });
    }

    #[test]
    fn test_char_format_range_out_of_bounds_returns_error() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            // "Hello World" 只有11字节，range_end=100 越界
            let err: String = e.eval("set_char_format_range(0, 0, 100, 1, -1, -1, -1.0, \"\", \"\", \"\")").unwrap();
            assert!(err.contains("超出段落范围"), "应返回越界错误信息，实际: {err}");

            let fmt: String = e.eval("get_char_format_range(0, 0, 100)").unwrap();
            let v: serde_json::Value = serde_json::from_str(&fmt).unwrap();
            assert!(v.get("error").is_some(), "应返回 error 字段，实际: {fmt}");
        });
    }

    #[test]
    fn test_clear_char_format() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("set_char_format(0, 0, true, true, true, 16.0)").unwrap();
            e.run("clear_char_format(0, 0)").unwrap();
            let fmt: String = e.eval("get_char_format(0, 0)").unwrap();
            let v: serde_json::Value = serde_json::from_str(&fmt).unwrap();
            assert_eq!(v["bold"], false);
            assert_eq!(v["font_size"], serde_json::Value::Null);
        });
    }

    #[test]
    fn test_set_paragraph_format() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("set_paragraph_format(0, \"center\", 10.0, 5.0, 6.0, 6.0)").unwrap();
            let fmt: String = e.eval("get_paragraph_format(0)").unwrap();
            let v: serde_json::Value = serde_json::from_str(&fmt).unwrap();
            assert_eq!(v["alignment"], "center");
            assert!((v["indent_left"].as_f64().unwrap() - 10.0).abs() < 0.01);
        });
    }

    #[test]
    fn test_set_line_height() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("set_line_height(0, 1.5)").unwrap();
            let fmt: String = e.eval("get_paragraph_format(0)").unwrap();
            let v: serde_json::Value = serde_json::from_str(&fmt).unwrap();
            assert!((v["line_height"].as_f64().unwrap() - 1.5).abs() < 0.01);
        });
    }

    #[test]
    fn test_set_line_height_ignores_non_positive() {
        // value <= 0 表示不修改，约定与 set_paragraph_format 一致
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("set_line_height(0, 1.5)").unwrap();
            e.run("set_line_height(0, -1.0)").unwrap();
            let fmt: String = e.eval("get_paragraph_format(0)").unwrap();
            let v: serde_json::Value = serde_json::from_str(&fmt).unwrap();
            assert!((v["line_height"].as_f64().unwrap() - 1.5).abs() < 0.01);
        });
    }

    #[test]
    fn test_clear_line_height() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("set_line_height(0, 2.0)").unwrap();
            e.run("clear_line_height(0)").unwrap();
            let fmt: String = e.eval("get_paragraph_format(0)").unwrap();
            let v: serde_json::Value = serde_json::from_str(&fmt).unwrap();
            assert_eq!(v["line_height"], serde_json::Value::Null);
        });
    }

    fn make_test_frame() -> FloatFrame {
        FloatFrame {
            origin_para_body_index: None,
            image: ImageData { data: vec![], mime: "image/png".into(), ..Default::default() },
            width_pt: 100.0,
            height_pt: 100.0,
            x_pt: 0.0,
            y_pt: 0.0,
            anchor_type_h: None,
            anchor_type_v: None,
            align_h: None,
            align_v: None,
            wrap_type: None,
            dist_t_pt: 0.0,
            dist_b_pt: 0.0,
            dist_l_pt: 0.0,
            dist_r_pt: 0.0,
            rotation: None,
        }
    }

    fn make_test_shape() -> Shape {
        Shape {
            geometry: ShapeGeometry::Rect,
            style: ShapeStyle::default(),
            text_content: vec![],
            width_pt: 100.0,
            height_pt: 100.0,
            x_pt: 0.0,
            y_pt: 0.0,
            anchor_type_h: None,
            anchor_type_v: None,
            align_h: None,
            align_v: None,
            wrap_type: None,
            dist_t_pt: 0.0,
            dist_b_pt: 0.0,
            dist_l_pt: 0.0,
            dist_r_pt: 0.0,
            flip_v: None,
        rotation: None, }
    }

    #[test]
    fn test_frame_wrap_type_valid_and_invalid() {
        let mut doc = empty_doc();
        doc.body.push(BodyElement::Frame(make_test_frame()));
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            // 合法值生效
            e.run("set_wrap_type(1, \"square\")").unwrap();
            let fmt: String = e.eval("get_frame_wrap(1)").unwrap();
            let v: serde_json::Value = serde_json::from_str(&fmt).unwrap();
            assert_eq!(v["kind"], "frame");
            assert_eq!(v["wrap_type"], "square");
            // 非法值不生效，保持原值
            e.run("set_wrap_type(1, \"bogus\")").unwrap();
            let fmt2: String = e.eval("get_frame_wrap(1)").unwrap();
            let v2: serde_json::Value = serde_json::from_str(&fmt2).unwrap();
            assert_eq!(v2["wrap_type"], "square");
        });
    }

    #[test]
    fn test_shape_anchor_align_distance() {
        let mut doc = empty_doc();
        doc.body.push(BodyElement::Shape(make_test_shape()));
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("set_frame_anchor(1, \"page\", \"paragraph\")").unwrap();
            e.run("set_frame_align(1, \"center\", \"top\")").unwrap();
            e.run("set_wrap_distance(1, 5.0, 5.0, 3.0, 3.0)").unwrap();

            let fmt: String = e.eval("get_frame_wrap(1)").unwrap();
            let v: serde_json::Value = serde_json::from_str(&fmt).unwrap();
            assert_eq!(v["kind"], "shape");
            assert_eq!(v["anchor_type_h"], "page");
            assert_eq!(v["anchor_type_v"], "paragraph");
            assert_eq!(v["align_h"], "center");
            assert_eq!(v["align_v"], "top");
            assert!((v["dist_t_pt"].as_f64().unwrap() - 5.0).abs() < 0.01);
            assert!((v["dist_l_pt"].as_f64().unwrap() - 3.0).abs() < 0.01);
        });
    }

    #[test]
    fn test_wrap_distance_ignores_negative() {
        let mut doc = empty_doc();
        doc.body.push(BodyElement::Frame(make_test_frame()));
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("set_wrap_distance(1, 5.0, 5.0, 5.0, 5.0)").unwrap();
            e.run("set_wrap_distance(1, -1.0, -1.0, -1.0, -1.0)").unwrap();
            let fmt: String = e.eval("get_frame_wrap(1)").unwrap();
            let v: serde_json::Value = serde_json::from_str(&fmt).unwrap();
            assert!((v["dist_t_pt"].as_f64().unwrap() - 5.0).abs() < 0.01);
        });
    }

    #[test]
    fn test_get_frame_wrap_on_non_frame_returns_empty() {
        // body_idx 指向普通段落时，get_frame_wrap 应返回空对象而非报错
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            let fmt: String = e.eval("get_frame_wrap(0)").unwrap();
            assert_eq!(fmt, "{}");
        });
    }

    #[test]
    fn test_set_char_format_ext() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);

            // 颜色 + 背景
            e.run("set_char_color(0, 0, 255, 0, 0)").unwrap();
            e.run("set_char_background(0, 0, 255, 255, 0)").unwrap();
            // 删除线
            e.run("set_char_strikethrough(0, 0, true)").unwrap();
            // 上标
            e.run("set_char_script(0, 0, \"superscript\")").unwrap();
            // 字体名（东亚+西文）
            e.run("set_char_font(0, 0, \"宋体\", \"Times New Roman\")").unwrap();
            // 下划线样式：波浪线
            e.run("set_char_underline_style(0, 0, \"wave\")").unwrap();

            let fmt: String = e.eval("get_char_format(0, 0)").unwrap();
            let v: serde_json::Value = serde_json::from_str(&fmt).unwrap();

            assert_eq!(v["color"], "#ff0000");
            assert_eq!(v["background"], "#ffff00");
            assert_eq!(v["strikethrough"], true);
            assert_eq!(v["script"], "superscript");
            assert_eq!(v["font_family"], "宋体");
            assert_eq!(v["font_family_ascii"], "Times New Roman");
            assert_eq!(v["underline_style"], "wave");
        });
    }

    #[test]
    fn test_underline_style_defaults_to_single() {
        // 未调用 set_char_underline_style 时，underline_style 应默认按 single 处理
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("set_char_format(0, 0, false, false, true, 12.0)").unwrap();
            let fmt: String = e.eval("get_char_format(0, 0)").unwrap();
            let v: serde_json::Value = serde_json::from_str(&fmt).unwrap();
            assert_eq!(v["underline"], true);
            assert_eq!(v["underline_style"], "single");
        });
    }

    #[test]
    fn test_clear_char_format_ext() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("set_char_color(0, 0, 255, 0, 0)").unwrap();
            e.run("set_char_background(0, 0, 0, 255, 0)").unwrap();
            e.run("set_char_strikethrough(0, 0, true)").unwrap();
            e.run("set_char_script(0, 0, \"subscript\")").unwrap();
            e.run("set_char_underline_style(0, 0, \"double\")").unwrap();
            e.run("clear_char_format(0, 0)").unwrap();

            let fmt: String = e.eval("get_char_format(0, 0)").unwrap();
            let v: serde_json::Value = serde_json::from_str(&fmt).unwrap();
            assert_eq!(v["color"], serde_json::Value::Null);
            assert_eq!(v["background"], serde_json::Value::Null);
            assert_eq!(v["strikethrough"], false);
            assert_eq!(v["script"], "normal");
            assert_eq!(v["underline_style"], "single");
        });
    }

    #[test]
    fn test_set_page_layout() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            // A4 横向
            e.run("set_page_layout(841.89, 595.28, 20.0, 20.0, 25.0, 25.0)").unwrap();
        });
        assert!((doc.page_size.0 - 841.89).abs() < 0.01);
        assert!((doc.page_size.1 - 595.28).abs() < 0.01);
    }

    #[test]
    fn test_insert_hyperlink() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run(r#"insert_hyperlink(0, 0, "https://example.com", "Example")"#).unwrap();
            let _n: i64 = e.eval("paragraph_count()").unwrap();
        });
        if let BodyElement::Paragraph(p) = &doc.body[0] {
            let link_run = p.runs.iter().find(|r| r.hyperlink.is_some());
            assert!(link_run.is_some(), "should have a hyperlink run");
            assert_eq!(link_run.unwrap().hyperlink.as_deref(), Some("https://example.com"));
            assert_eq!(link_run.unwrap().text, "Example");
        } else {
            panic!("expected paragraph");
        }
    }

    #[test]
    fn test_set_list_format() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run(r#"set_list_format(0, "bullet", 0)"#).unwrap();
        });
        if let BodyElement::Paragraph(p) = &doc.body[0] {
            assert!(p.list_info.is_some());
            assert_eq!(p.list_info.as_ref().unwrap().style_name, "bullet");
        }
    }

    #[test]
    fn test_set_list_format_clear() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run(r#"set_list_format(0, "bullet", 0)"#).unwrap();
            e.run(r#"set_list_format(0, "", 0)"#).unwrap();
        });
        if let BodyElement::Paragraph(p) = &doc.body[0] {
            assert!(p.list_info.is_none());
        }
    }

    #[test]
    fn test_set_header_footer() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run(r#"set_header_footer("Page Header", "Page Footer")"#).unwrap();
        });
        let header_text = doc.header.default.as_ref()
            .and_then(|paras| paras.first())
            .and_then(|p| p.runs.first())
            .map(|r| r.text.as_str());
        assert_eq!(header_text, Some("Page Header"));
        let footer_text = doc.footer.default.as_ref()
            .and_then(|paras| paras.first())
            .and_then(|p| p.runs.first())
            .map(|r| r.text.as_str());
        assert_eq!(footer_text, Some("Page Footer"));
    }

    #[test]
    fn test_set_header_footer_clear() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run(r#"set_header_footer("Header", "Footer")"#).unwrap();
            e.run(r#"set_header_footer("", "")"#).unwrap();
        });
        assert!(doc.header.default.is_none());
        assert!(doc.footer.default.is_none());
    }

    #[test]
    fn test_insert_image() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run(r#"insert_image(0, "QUJD", 100.0, 50.0)"#).unwrap();
        });
        assert_eq!(doc.body.len(), 2);
        if let BodyElement::Image(img) = &doc.body[0] {
            assert_eq!(img.image.data, vec![65u8, 66, 67]);
            assert!((img.width_pt - 100.0).abs() < 0.01);
            assert!((img.height_pt - 50.0).abs() < 0.01);
        } else {
            panic!("expected Image at index 0");
        }
    }

    #[test]
    fn test_insert_table() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("insert_table(0, 2, 3)").unwrap();
        });
        assert_eq!(doc.body.len(), 2);
        if let BodyElement::Table(t) = &doc.body[0] {
            assert_eq!(t.rows.len(), 2);
            assert_eq!(t.rows[0].cells.len(), 3);
            assert_eq!(t.col_widths.len(), 3);
        } else {
            panic!("expected Table at index 0");
        }
    }

    #[test]
    fn test_table_insert_row_after() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("insert_table(0, 2, 3)").unwrap();
            e.run("set_char_format(0, 0, false, false, false, 12.0)").unwrap(); // 占位，避免未使用告警风格不一致，保持与其余测试类似的显式感
        });
        // 手动给 row 0 的 cell 0 设置一个可识别的样式，验证 insert_row 克隆行为
        if let BodyElement::Table(t) = &mut doc.body[0] {
            t.rows[0].cells[0].bg_color = Some(Color { r: 10, g: 20, b: 30, a: 255 });
        }
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("table_insert_row(0, 0, true)").unwrap();
        });
        if let BodyElement::Table(t) = &doc.body[0] {
            assert_eq!(t.rows.len(), 3);
            // 新行应插入在 row 0 之后，即新的 row 1
            assert_eq!(t.rows[1].cells.len(), 3);
            assert_eq!(t.rows[1].cells[0].bg_color, Some(Color { r: 10, g: 20, b: 30, a: 255 }));
            // row_height 应为 None（新行不继承高度）
            assert_eq!(t.rows[1].row_height, None);
        } else {
            panic!("expected Table at index 0");
        }
    }

    #[test]
    fn test_table_insert_row_before() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("insert_table(0, 2, 3)").unwrap();
            e.run("table_insert_row(0, 1, false)").unwrap();
        });
        if let BodyElement::Table(t) = &doc.body[0] {
            assert_eq!(t.rows.len(), 3);
        } else {
            panic!("expected Table at index 0");
        }
    }

    #[test]
    fn test_table_delete_row() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("insert_table(0, 3, 2)").unwrap();
            e.run("table_delete_row(0, 1)").unwrap();
        });
        if let BodyElement::Table(t) = &doc.body[0] {
            assert_eq!(t.rows.len(), 2);
        } else {
            panic!("expected Table at index 0");
        }
    }

    #[test]
    fn test_table_delete_row_refuses_to_empty_table() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("insert_table(0, 1, 2)").unwrap();
            // 表格只有1行，删除应被静默忽略
            e.run("table_delete_row(0, 0)").unwrap();
        });
        if let BodyElement::Table(t) = &doc.body[0] {
            assert_eq!(t.rows.len(), 1, "只剩1行时delete_row应被静默忽略，不应删到0行");
        } else {
            panic!("expected Table at index 0");
        }
    }

    #[test]
    fn test_table_insert_column() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("insert_table(0, 2, 2)").unwrap();
        });
        if let BodyElement::Table(t) = &mut doc.body[0] {
            t.rows[0].cells[0].border_color = Some(Color { r: 1, g: 2, b: 3, a: 255 });
            t.col_widths[0] = 100.0;
        }
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("table_insert_column(0, 0, true)").unwrap();
        });
        if let BodyElement::Table(t) = &doc.body[0] {
            assert_eq!(t.col_widths.len(), 3);
            assert_eq!(t.col_widths[1], 100.0, "新列宽度应克隆自col_idx=0那一列");
            assert_eq!(t.rows[0].cells.len(), 3);
            assert_eq!(t.rows[0].cells[1].border_color, Some(Color { r: 1, g: 2, b: 3, a: 255 }));
            assert_eq!(t.rows[1].cells.len(), 3);
        } else {
            panic!("expected Table at index 0");
        }
    }

    #[test]
    fn test_table_delete_column() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("insert_table(0, 2, 3)").unwrap();
            e.run("table_delete_column(0, 1)").unwrap();
        });
        if let BodyElement::Table(t) = &doc.body[0] {
            assert_eq!(t.col_widths.len(), 2);
            assert_eq!(t.rows[0].cells.len(), 2);
            assert_eq!(t.rows[1].cells.len(), 2);
        } else {
            panic!("expected Table at index 0");
        }
    }

    #[test]
    fn test_table_delete_column_refuses_to_empty_table() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("insert_table(0, 2, 1)").unwrap();
            e.run("table_delete_column(0, 0)").unwrap();
        });
        if let BodyElement::Table(t) = &doc.body[0] {
            assert_eq!(t.col_widths.len(), 1, "只剩1列时delete_column应被静默忽略");
        } else {
            panic!("expected Table at index 0");
        }
    }

    #[test]
    fn test_table_row_col_ops_ignore_invalid_body_idx() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("insert_paragraph(0, \"not a table\")").unwrap();
            // body_idx=0 指向段落而非表格，四个函数都应静默忽略、不panic
            for sc in ["table_insert_row(0, 0, true)", "table_delete_row(0, 0)",
                       "table_insert_column(0, 0, true)", "table_delete_column(0, 0)"] {
                let r = e.eval::<String>(sc).unwrap();
                assert!(r.starts_with("err:") && r.contains("不是表格"), "{} -> {}", sc, r);
            }
            let r = e.eval::<String>("table_insert_row(99, 0, true)").unwrap(); // body_idx越界
            assert!(r.starts_with("err:") && r.contains("越界"), "{}", r);
        });
        // 未panic即为通过。insert_paragraph(0, ..) 是插入不是替换，
        // 原有1个段落被推到索引1，所以此时 body.len() 应为2；
        // 后续四个 table_* 调用全部作用在非Table目标/越界目标上，
        // 应被静默忽略，不应再改变 body 长度或使其panic。
        assert_eq!(doc.body.len(), 2);
    }

    #[test]
    fn test_table_ops_return_string() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("insert_table(0, 2, 3)").unwrap();
            assert_eq!(e.eval::<String>("table_insert_row(0, 0, true)").unwrap(), "ok");
            assert_eq!(e.eval::<String>("table_insert_column(0, 0, false)").unwrap(), "ok");
            assert_eq!(e.eval::<String>("set_table_row_height(0, 0, 30.0)").unwrap(), "ok");
            assert_eq!(e.eval::<String>("clear_table_row_height(0, 0)").unwrap(), "ok");
            let r = e.eval::<String>("table_delete_row(0, 5)").unwrap();
            assert!(r.starts_with("err:row_idx=5 越界"), "{}", r);
            let r = e.eval::<String>("table_delete_column(0, -1)").unwrap();
            assert!(r.starts_with("err:col_idx=-1 越界"), "{}", r);
            let r = e.eval::<String>("set_table_row_height(0, 0, -2.0)").unwrap();
            assert!(r.starts_with("err:height_pt"), "{}", r);
            // 新插入 1x1 表格(插在 body 0 之前,原表后移),删到只剩 1 行/1 列后拒绝再删
            e.run("insert_table(0, 1, 1)").unwrap();
            let r = e.eval::<String>("table_delete_row(0, 0)").unwrap();
            assert!(r.contains("最后一行"), "{}", r);
            let r = e.eval::<String>("table_delete_column(0, 0)").unwrap();
            assert!(r.contains("最后一列"), "{}", r);
        });
    }

    #[test]
    fn test_table_style_border_ops_return_string() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("insert_table(0, 2, 2)").unwrap();
            for sc in ["set_table_style_id(0, \"X\")", "clear_table_style_id(0)",
                       "set_table_first_row(0, true)", "set_table_band_col(0, false)",
                       "set_table_default_border_color(0, 1, 2, 3)", "clear_table_default_border_color(0)",
                       "set_table_default_border_width(0, 1.0)", "clear_table_default_border_width(0)",
                       "set_table_inside_h_border_color(0, 1, 2, 3)", "set_table_inside_h_border_width(0, 0.5)",
                       "set_table_inside_v_border_color(0, 1, 2, 3)", "clear_table_inside_v_border_width(0)"] {
                assert_eq!(e.eval::<String>(sc).unwrap(), "ok", "{}", sc);
            }
            let r = e.eval::<String>("set_table_default_border_color(0, 256, 0, 0)").unwrap();
            assert!(r.starts_with("err:颜色分量 r=256"), "{}", r);
            let r = e.eval::<String>("set_table_inside_v_border_color(0, 0, -1, 0)").unwrap();
            assert!(r.starts_with("err:颜色分量 g=-1"), "{}", r);
            for sc in ["set_table_default_border_width(0, 0.0)", "set_table_inside_h_border_width(0, -1.0)",
                       "set_table_inside_v_border_width(0, -5.0)"] {
                let r = e.eval::<String>(sc).unwrap();
                assert!(r.starts_with("err:width_pt"), "{} -> {}", sc, r);
            }
            let r = e.eval::<String>("set_table_first_row(99, true)").unwrap();
            assert!(r.starts_with("err:") && r.contains("越界"), "{}", r);
            e.run("insert_paragraph(0, \"p\")").unwrap();
            let r = e.eval::<String>("clear_table_style_id(0)").unwrap();
            assert!(r.contains("不是表格"), "{}", r);
        });
    }

    #[test]
    fn test_char_misc_ops_return_string() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("insert_paragraph(0, \"abc\")").unwrap();
            for sc in ["set_char_background(0, 0, 255, 255, 0)", "clear_char_background(0, 0)",
                       "set_char_color(0, 0, 1, 2, 3)", "clear_char_color(0, 0)",
                       "set_char_strikethrough(0, 0, true)", "set_char_script(0, 0, \"superscript\")",
                       "set_char_underline_style(0, 0, \"wave\")", "set_line_height(0, 1.5)",
                       "clear_line_height(0)"] {
                assert_eq!(e.eval::<String>(sc).unwrap(), "ok", "{}", sc);
            }
            let r = e.eval::<String>("set_char_background(0, 0, 0, 300, 0)").unwrap();
            assert!(r.starts_with("err:颜色分量 g=300"), "{}", r);
            let r = e.eval::<String>("set_char_script(0, 0, \"up\")").unwrap();
            assert!(r.starts_with("err:mode 非法值"), "{}", r);
            let r = e.eval::<String>("set_char_underline_style(0, 0, \"dots\")").unwrap();
            assert!(r.starts_with("err:style 非法值"), "{}", r);
            let r = e.eval::<String>("set_line_height(0, 0.0)").unwrap();
            assert!(r.starts_with("err:value"), "{}", r);
            let r = e.eval::<String>("clear_char_color(0, 9)").unwrap();
            assert!(r.starts_with("err:run_idx=9 越界"), "{}", r);
            let r = e.eval::<String>("clear_line_height(99)").unwrap();
            assert!(r.contains("越界"), "{}", r);
        });
    }

    #[test]
    fn test_frame_ops_return_string() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("insert_paragraph(0, \"p\")").unwrap();
            for sc in ["set_wrap_type(0, \"square\")", "set_frame_anchor(0, \"page\", \"\")",
                       "set_frame_align(0, \"center\", \"top\")", "set_wrap_distance(0, 1.0, 1.0, 1.0, 1.0)"] {
                let r = e.eval::<String>(sc).unwrap();
                assert!(r.starts_with("err:") && r.contains("不是图文框/形状"), "{} -> {}", sc, r);
            }
            let r = e.eval::<String>("set_wrap_type(99, \"square\")").unwrap();
            assert!(r.starts_with("err:") && r.contains("越界"), "{}", r);
            let r = e.eval::<String>("set_frame_align(-1, \"left\", \"\")").unwrap();
            assert!(r.contains("不能为负数"), "{}", r);
            assert_eq!(e.eval::<String>("set_page_layout(595.0, 842.0, 72.0, 72.0, 72.0, 72.0)").unwrap(), "ok");
            assert_eq!(e.eval::<String>("set_page_layout(0.0, 0.0, -1.0, -1.0, -1.0, -1.0)").unwrap(), "ok");
        });
    }

    #[test]
    fn test_section_ops_return_string() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            assert_eq!(e.eval::<String>("set_header_footer(\"h\", \"f\")").unwrap(), "ok");
            assert_eq!(e.eval::<String>("insert_section_break(0)").unwrap(), "ok");
            for sc in ["set_section_page_layout(0, 595.0, 842.0, 72.0, 72.0, 72.0, 72.0)",
                       "set_section_header_footer(0, \"a\", \"b\")", "set_section_title_page(0, true)",
                       "set_section_type(0, \"continuous\")", "set_section_type(0, \"\")",
                       "set_section_columns(0, 2)"] {
                assert_eq!(e.eval::<String>(sc).unwrap(), "ok", "{}", sc);
            }
            let r = e.eval::<String>("set_section_type(0, \"bogus\")").unwrap();
            assert!(r.starts_with("err:section_type 非法值"), "{}", r);
            let r = e.eval::<String>("set_section_columns(0, 0)").unwrap();
            assert!(r.starts_with("err:num_cols=0"), "{}", r);
            let r = e.eval::<String>("set_section_title_page(99, true)").unwrap();
            assert!(r.starts_with("err:节下标 idx=99 越界"), "{}", r);
            let r = e.eval::<String>("set_section_page_layout(-1, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0)").unwrap();
            assert!(r.contains("不能为负数"), "{}", r);
            let r = e.eval::<String>("insert_section_break(-1)").unwrap();
            assert!(r.starts_with("err:body 下标=-1"), "{}", r);
        });
    }

    #[test]
    fn test_style_and_toc_ops_return_string() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            assert_eq!(e.eval::<String>(r#"add_paragraph_style("{\"name\":\"PX\"}")"#).unwrap(), "ok");
            assert_eq!(e.eval::<String>(r#"add_char_style("{\"name\":\"CX\"}")"#).unwrap(), "ok");
            let r = e.eval::<String>(r#"add_paragraph_style("not json")"#).unwrap();
            assert!(r.starts_with("err:style_json 不是合法 JSON"), "{}", r);
            let r = e.eval::<String>(r#"add_char_style("{}")"#).unwrap();
            assert!(r.contains("缺少字符串字段 name"), "{}", r);
            let r = e.eval::<String>(r#"add_paragraph_style("{\"name\":\"Y\",\"alignment\":\"bogus\"}")"#).unwrap();
            assert!(r.starts_with("err:alignment 非法值"), "{}", r);
            let r = e.eval::<String>(r#"add_char_style("{\"name\":\"Z\",\"script\":\"bogus\"}")"#).unwrap();
            assert!(r.starts_with("err:script 非法值"), "{}", r);
            assert_eq!(e.eval::<String>(r#"remove_paragraph_style("PX")"#).unwrap(), "ok");
            assert_eq!(e.eval::<String>(r#"remove_char_style("CX")"#).unwrap(), "ok");
            let r = e.eval::<String>(r#"remove_paragraph_style("PX")"#).unwrap();
            assert!(r.contains("不存在"), "{}", r);
            let r = e.eval::<String>(r#"remove_char_style("nope")"#).unwrap();
            assert!(r.contains("不存在"), "{}", r);
            let r = e.eval::<String>("generate_toc()").unwrap();
            assert!(r.starts_with("err:没有标题段落"), "{}", r);
            e.run(r#"insert_paragraph(0, "T")"#).unwrap();
            e.run(r#"apply_paragraph_style(0, "Heading1")"#).unwrap();
            assert_eq!(e.eval::<String>("generate_toc()").unwrap(), "ok");
        });
    }

    #[test]
    fn test_set_table_col_width() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("insert_table(0, 2, 3)").unwrap();
            e.run("set_table_col_width(0, 1, 150.0)").unwrap();
        });
        if let BodyElement::Table(t) = &doc.body[0] {
            assert!((t.col_widths[1] - 150.0).abs() < 0.01);
            // 其余列不受影响，仍为 insert_table 默认的 72pt
            assert!((t.col_widths[0] - 72.0).abs() < 0.01);
        } else {
            panic!("expected Table at index 0");
        }
    }

    #[test]
    fn test_set_table_col_width_ignores_non_positive() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("insert_table(0, 2, 3)").unwrap();
            assert_eq!(e.eval::<String>("set_table_col_width(0, 0, 150.0)").unwrap(), "ok");
            let r1 = e.eval::<String>("set_table_col_width(0, 0, -1.0)").unwrap();
            let r2 = e.eval::<String>("set_table_col_width(0, 0, 0.0)").unwrap();
            assert!(r1.starts_with("err:") && r2.starts_with("err:"), "{} / {}", r1, r2);
        });
        if let BodyElement::Table(t) = &doc.body[0] {
            assert!((t.col_widths[0] - 150.0).abs() < 0.01, "非正数应被忽略，保留之前设置的150.0");
        } else {
            panic!("expected Table at index 0");
        }
    }

    #[test]
    fn test_set_and_clear_table_row_height() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("insert_table(0, 2, 2)").unwrap();
            e.run("set_table_row_height(0, 0, 30.0)").unwrap();
        });
        if let BodyElement::Table(t) = &doc.body[0] {
            assert_eq!(t.rows[0].row_height, Some(30.0));
            assert_eq!(t.rows[1].row_height, None, "未设置的行应保持None");
        } else {
            panic!("expected Table at index 0");
        }
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("clear_table_row_height(0, 0)").unwrap();
        });
        if let BodyElement::Table(t) = &doc.body[0] {
            assert_eq!(t.rows[0].row_height, None, "clear后应恢复为None（自动测量）");
        } else {
            panic!("expected Table at index 0");
        }
    }

    #[test]
    fn test_set_table_row_height_ignores_non_positive() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("insert_table(0, 2, 2)").unwrap();
            e.run("set_table_row_height(0, 0, 30.0)").unwrap();
            e.run("set_table_row_height(0, 0, -5.0)").unwrap();
            e.run("set_table_row_height(0, 0, 0.0)").unwrap();
        });
        if let BodyElement::Table(t) = &doc.body[0] {
            assert_eq!(t.rows[0].row_height, Some(30.0), "非正数应被忽略，保留之前设置的30.0");
        } else {
            panic!("expected Table at index 0");
        }
    }

    #[test]
    fn test_table_col_width_row_height_ignore_invalid_targets() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("insert_table(0, 2, 2)").unwrap();
            // 越界的 body_idx / col_idx / row_idx，均应静默忽略、不panic
            e.run("set_table_col_width(99, 0, 100.0)").unwrap();
            e.run("set_table_col_width(0, 99, 100.0)").unwrap();
            e.run("set_table_row_height(99, 0, 100.0)").unwrap();
            e.run("set_table_row_height(0, 99, 100.0)").unwrap();
            e.run("clear_table_row_height(99, 0)").unwrap();
            e.run("clear_table_row_height(0, 99)").unwrap();
        });
        // 未panic即为通过；表格结构应保持不变
        if let BodyElement::Table(t) = &doc.body[0] {
            assert_eq!(t.col_widths.len(), 2);
            assert_eq!(t.rows.len(), 2);
        } else {
            panic!("expected Table at index 0");
        }
    }

    #[test]
    fn test_generate_toc() {
        let mut doc = empty_doc();
        // 添加几个标题段落
        doc.body.push(BodyElement::Paragraph(Paragraph {
            style: StyleRef("Heading1".into()),
            runs: vec![Run { text: "第一章".into(), ..Default::default() }],
            ..Default::default()
        }));
        doc.body.push(BodyElement::Paragraph(Paragraph {
            style: StyleRef("Heading2".into()),
            runs: vec![Run { text: "第一节".into(), ..Default::default() }],
            ..Default::default()
        }));
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("generate_toc()").unwrap();
        });
        // 文档开头应该是目录标题
        if let BodyElement::Paragraph(p) = &doc.body[0] {
            assert_eq!(p.style.0, "Heading1");
            let text: String = p.runs.iter().map(|r| r.text.as_str()).collect();
            assert_eq!(text, "目录");
        } else {
            panic!("expected TOC heading at index 0");
        }
        // 目录条目应该包含标题文字
        let all_text: Vec<String> = doc.body.iter().filter_map(|e| {
            if let BodyElement::Paragraph(p) = e {
                let t: String = p.runs.iter().map(|r| r.text.as_str()).collect();
                if !t.is_empty() { Some(t) } else { None }
            } else { None }
        }).collect();
        assert!(all_text.contains(&"第一章".to_string()));
        assert!(all_text.contains(&"第一节".to_string()));
    }


    #[test]
    fn test_save_docx_produces_valid_bytes() {
        // 验证 save_docx() 脚本接口：base64 解码后应能被 DocxParser 正常
        // 读回，且内容与 empty_doc() 构造的原始文档语义一致（段落数量+
        // 文字），证明 rhai 层→DocxWriter→字节流这条链路完整可用，
        // 不只是"能编译/不panic"这种弱验证。
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            let b64: String = e.eval("save_docx()").unwrap();
            assert!(!b64.is_empty(), "save_docx() 不应返回空字符串");

            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64.as_bytes())
                .expect("base64 解码失败");

            let parsed = crate_docx_parse(&bytes)
                .expect("DocxParser 读回失败，说明 save_docx 产出的字节流不是合法 docx");

            assert_eq!(parsed.body.len(), 1);
            match &parsed.body[0] {
                BodyElement::Paragraph(p) => {
                    let text: String = p.runs.iter().map(|r| r.text.as_str()).collect();
                    assert_eq!(text, "Hello World");
                }
                _ => panic!("expected Paragraph"),
            }
        });
    }


    // ── 表格样式绑定：style_id + 6个bool开关 ──────────────────────

    #[test]
    fn test_set_and_clear_table_style_id() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("insert_table(0, 2, 2)").unwrap();
            e.run(r#"set_table_style_id(0, "myStyle123")"#).unwrap();
        });
        if let BodyElement::Table(t) = &doc.body[0] {
            assert_eq!(t.style_id.as_deref(), Some("myStyle123"));
        } else {
            panic!("expected Table at index 0");
        }
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("clear_table_style_id(0)").unwrap();
        });
        if let BodyElement::Table(t) = &doc.body[0] {
            assert_eq!(t.style_id, None);
        } else {
            panic!("expected Table at index 0");
        }
    }

    #[test]
    fn test_set_table_style_bool_flags() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("insert_table(0, 2, 2)").unwrap();
            e.run("set_table_first_row(0, true)").unwrap();
            e.run("set_table_first_col(0, true)").unwrap();
            e.run("set_table_last_row(0, true)").unwrap();
            e.run("set_table_last_col(0, true)").unwrap();
            e.run("set_table_band_row(0, true)").unwrap();
            e.run("set_table_band_col(0, true)").unwrap();
        });
        if let BodyElement::Table(t) = &doc.body[0] {
            assert!(t.first_row);
            assert!(t.first_col);
            assert!(t.last_row);
            assert!(t.last_col);
            assert!(t.band_row);
            assert!(t.band_col);
        } else {
            panic!("expected Table at index 0");
        }
        // 再次调用传 false，确认可以关闭（不是只能设一次）
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("set_table_first_row(0, false)").unwrap();
        });
        if let BodyElement::Table(t) = &doc.body[0] {
            assert!(!t.first_row, "应能重新设为false，不是一次性开关");
            assert!(t.first_col, "其余开关不应受影响");
        } else {
            panic!("expected Table at index 0");
        }
    }

    // ── 表格边框颜色/宽度：default / inside_h / inside_v 三组 ──────

    #[test]
    fn test_set_and_clear_table_default_border() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("insert_table(0, 2, 2)").unwrap();
            e.run("set_table_default_border_color(0, 255, 0, 0)").unwrap();
            e.run("set_table_default_border_width(0, 1.5)").unwrap();
        });
        if let BodyElement::Table(t) = &doc.body[0] {
            assert_eq!(t.default_border_color, Some(Color { r: 255, g: 0, b: 0, a: 255 }));
            assert!((t.default_border_width.unwrap() - 1.5).abs() < 0.01);
        } else {
            panic!("expected Table at index 0");
        }
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("clear_table_default_border_color(0)").unwrap();
            e.run("clear_table_default_border_width(0)").unwrap();
        });
        if let BodyElement::Table(t) = &doc.body[0] {
            assert_eq!(t.default_border_color, None);
            assert_eq!(t.default_border_width, None);
        } else {
            panic!("expected Table at index 0");
        }
    }

    #[test]
    fn test_set_table_default_border_width_ignores_non_positive() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("insert_table(0, 2, 2)").unwrap();
            e.run("set_table_default_border_width(0, 0.0)").unwrap();
            e.run("set_table_default_border_width(0, -1.0)").unwrap();
        });
        if let BodyElement::Table(t) = &doc.body[0] {
            assert_eq!(t.default_border_width, None, "非正数应静默忽略，与set_table_col_width约定一致");
        } else {
            panic!("expected Table at index 0");
        }
    }

    #[test]
    fn test_set_and_clear_table_inside_h_border() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("insert_table(0, 2, 2)").unwrap();
            e.run("set_table_inside_h_border_color(0, 0, 255, 0)").unwrap();
            e.run("set_table_inside_h_border_width(0, 0.75)").unwrap();
        });
        if let BodyElement::Table(t) = &doc.body[0] {
            assert_eq!(t.inside_h_color, Some(Color { r: 0, g: 255, b: 0, a: 255 }));
            assert!((t.inside_h_width.unwrap() - 0.75).abs() < 0.01);
        } else {
            panic!("expected Table at index 0");
        }
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("clear_table_inside_h_border_color(0)").unwrap();
            e.run("clear_table_inside_h_border_width(0)").unwrap();
        });
        if let BodyElement::Table(t) = &doc.body[0] {
            assert_eq!(t.inside_h_color, None);
            assert_eq!(t.inside_h_width, None);
        } else {
            panic!("expected Table at index 0");
        }
    }

    #[test]
    fn test_set_table_inside_h_border_width_ignores_non_positive() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("insert_table(0, 2, 2)").unwrap();
            e.run("set_table_inside_h_border_width(0, -5.0)").unwrap();
        });
        if let BodyElement::Table(t) = &doc.body[0] {
            assert_eq!(t.inside_h_width, None);
        } else {
            panic!("expected Table at index 0");
        }
    }

    #[test]
    fn test_set_and_clear_table_inside_v_border() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("insert_table(0, 2, 2)").unwrap();
            e.run("set_table_inside_v_border_color(0, 0, 0, 255)").unwrap();
            e.run("set_table_inside_v_border_width(0, 2.0)").unwrap();
        });
        if let BodyElement::Table(t) = &doc.body[0] {
            assert_eq!(t.inside_v_color, Some(Color { r: 0, g: 0, b: 255, a: 255 }));
            assert!((t.inside_v_width.unwrap() - 2.0).abs() < 0.01);
        } else {
            panic!("expected Table at index 0");
        }
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("clear_table_inside_v_border_color(0)").unwrap();
            e.run("clear_table_inside_v_border_width(0)").unwrap();
        });
        if let BodyElement::Table(t) = &doc.body[0] {
            assert_eq!(t.inside_v_color, None);
            assert_eq!(t.inside_v_width, None);
        } else {
            panic!("expected Table at index 0");
        }
    }

    #[test]
    fn test_set_table_inside_v_border_width_ignores_non_positive() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("insert_table(0, 2, 2)").unwrap();
            e.run("set_table_inside_v_border_width(0, 0.0)").unwrap();
        });
        if let BodyElement::Table(t) = &doc.body[0] {
            assert_eq!(t.inside_v_width, None);
        } else {
            panic!("expected Table at index 0");
        }
    }

    // ── 通用防御测试：无效body_idx（非Table / 越界）静默忽略 ────────

    #[test]
    fn test_table_style_and_border_fns_ignore_invalid_body_idx() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            // body[0] 是empty_doc()自带的段落，不是Table
            e.run(r#"set_table_style_id(0, "x")"#).unwrap();
            e.run("clear_table_style_id(0)").unwrap();
            e.run("set_table_first_row(0, true)").unwrap();
            e.run("set_table_band_col(0, true)").unwrap();
            e.run("set_table_default_border_color(0, 1, 2, 3)").unwrap();
            e.run("set_table_default_border_width(0, 1.0)").unwrap();
            e.run("set_table_inside_h_border_color(0, 1, 2, 3)").unwrap();
            e.run("set_table_inside_v_border_width(0, 1.0)").unwrap();
            // 越界body_idx（文档里根本没有索引99）
            e.run(r#"set_table_style_id(99, "x")"#).unwrap();
            e.run("set_table_last_col(99, true)").unwrap();
        });
        // 未panic即说明防御生效；同时确认body[0]的段落本身未被破坏
        if let BodyElement::Paragraph(p) = &doc.body[0] {
            let text: String = p.runs.iter().map(|r| r.text.as_str()).collect();
            assert_eq!(text, "Hello World", "非Table的body元素不应被表格函数意外修改");
        } else {
            panic!("expected Paragraph at index 0 (empty_doc()的既有内容)");
        }
    }



    // ── 样式定义增删改查：paragraph_styles ──────────────────────────

    #[test]
    fn test_add_get_paragraph_style() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            let json = r#"{
                "name": "Heading1",
                "font_size": 24.0,
                "bold": true,
                "alignment": "center",
                "color": {"r": 10, "g": 20, "b": 30, "a": 255}
            }"#;
            e.run(&format!(r#"add_paragraph_style(`{json}`)"#)).unwrap();
        });
        assert_eq!(doc.styles.paragraph_styles.len(), 1);
        let s = doc.styles.paragraph_styles.get("Heading1").expect("should exist");
        assert_eq!(s.font_size, Some(24.0));
        assert!(s.bold);
        assert!(matches!(s.alignment, Some(Alignment::Center)), "alignment应为Center，实际是{:?}", s.alignment);
        assert_eq!(s.color, Some(Color { r: 10, g: 20, b: 30, a: 255 }));

        // get_paragraph_style 应能读回同样的值
        let mut result = String::new();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            result = e.eval(r#"get_paragraph_style("Heading1")"#).unwrap();
        });
        let v: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["font_size"], 24.0);
        assert_eq!(v["bold"], true);
        assert_eq!(v["alignment"], "center");
        assert_eq!(v["color"]["r"], 10);
    }

    #[test]
    fn test_add_paragraph_style_upserts_same_name() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run(r#"add_paragraph_style(`{"name": "Body", "font_size": 12.0}`)"#).unwrap();
            e.run(r#"add_paragraph_style(`{"name": "Body", "font_size": 14.0, "bold": true}`)"#).unwrap();
        });
        // 覆盖而不是新增一条：仍然只有1条记录
        assert_eq!(doc.styles.paragraph_styles.len(), 1);
        let s = doc.styles.paragraph_styles.get("Body").unwrap();
        assert_eq!(s.font_size, Some(14.0));
        assert!(s.bold, "第二次add应整条覆盖，不是只更新font_size");
    }

    #[test]
    fn test_remove_paragraph_style() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run(r#"add_paragraph_style(`{"name": "ToDelete"}`)"#).unwrap();
            e.run(r#"remove_paragraph_style("ToDelete")"#).unwrap();
        });
        assert!(doc.styles.paragraph_styles.get("ToDelete").is_none());
    }

    #[test]
    fn test_get_paragraph_style_returns_empty_object_when_missing() {
        let mut doc = empty_doc();
        let mut result = String::new();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            result = e.eval(r#"get_paragraph_style("NoSuchStyle")"#).unwrap();
        });
        assert_eq!(result, "{}");
    }

    #[test]
    fn test_list_paragraph_styles() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run(r#"add_paragraph_style(`{"name": "Heading1"}`)"#).unwrap();
            e.run(r#"add_paragraph_style(`{"name": "Heading2"}`)"#).unwrap();
        });
        let mut result = String::new();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            result = e.eval("list_paragraph_styles()").unwrap();
        });
        let v: Vec<String> = serde_json::from_str(&result).unwrap();
        assert_eq!(v.len(), 2);
        assert!(v.contains(&"Heading1".to_string()));
        assert!(v.contains(&"Heading2".to_string()));
    }

    #[test]
    fn test_add_paragraph_style_ignores_invalid_input() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            // 非法JSON
            e.run(r#"add_paragraph_style("not json")"#).unwrap();
            // 合法JSON但缺name字段
            e.run(r#"add_paragraph_style(`{"font_size": 12.0}`)"#).unwrap();
        });
        assert_eq!(doc.styles.paragraph_styles.len(), 0, "非法输入应静默忽略，不产生任何记录");
    }

    // ── 样式定义增删改查：char_styles ────────────────────────────────

    #[test]
    fn test_add_get_char_style() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            let json = r#"{
                "name": "Emphasis",
                "italic": true,
                "underline": true,
                "script": "superscript",
                "background": {"r": 255, "g": 255, "b": 0, "a": 255}
            }"#;
            e.run(&format!(r#"add_char_style(`{json}`)"#)).unwrap();
        });
        let s = doc.styles.char_styles.get("Emphasis").expect("should exist");
        assert!(s.italic);
        assert!(s.underline);
        assert!(matches!(s.script, Some(ScriptPosition::Superscript)), "script应为Superscript，实际是{:?}", s.script);
        assert_eq!(s.background, Some(Color { r: 255, g: 255, b: 0, a: 255 }));

        let mut result = String::new();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            result = e.eval(r#"get_char_style("Emphasis")"#).unwrap();
        });
        let v: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["italic"], true);
        assert_eq!(v["script"], "superscript");
        assert_eq!(v["background"]["g"], 255);
    }

    #[test]
    fn test_remove_char_style() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run(r#"add_char_style(`{"name": "ToDelete"}`)"#).unwrap();
            e.run(r#"remove_char_style("ToDelete")"#).unwrap();
        });
        assert!(doc.styles.char_styles.get("ToDelete").is_none());
    }

    #[test]
    fn test_get_char_style_returns_empty_object_when_missing() {
        let mut doc = empty_doc();
        let mut result = String::new();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            result = e.eval(r#"get_char_style("NoSuchStyle")"#).unwrap();
        });
        assert_eq!(result, "{}");
    }

    #[test]
    fn test_list_char_styles() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run(r#"add_char_style(`{"name": "Strong"}`)"#).unwrap();
        });
        let mut result = String::new();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            result = e.eval("list_char_styles()").unwrap();
        });
        let v: Vec<String> = serde_json::from_str(&result).unwrap();
        assert_eq!(v, vec!["Strong".to_string()]);
    }

    #[test]
    fn test_add_char_style_ignores_invalid_input() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run(r#"add_char_style("not json")"#).unwrap();
            e.run(r#"add_char_style(`{"italic": true}`)"#).unwrap();
        });
        assert_eq!(doc.styles.char_styles.len(), 0, "非法输入应静默忽略");
    }



    // ── 分节页面设置：insert_section_break + set_section_* ──────────

    #[test]
    fn test_insert_section_break_syncs_body_and_sections() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("insert_section_break(1)").unwrap();
        });
        // body里应插入了一个Section路标（原body[0]的段落被推到body[0]之前
        // 还是之后取决于插入位置1，这里直接检查body长度+类型）
        assert_eq!(doc.body.len(), 2);
        assert!(matches!(doc.body[1], BodyElement::Section(_)), "body[1]应为Section路标");
        // doc.sections应同步追加一条记录
        assert_eq!(doc.sections.len(), 1);
        assert_eq!(doc.sections[0].name, "Section 1");
    }

    #[test]
    fn test_insert_section_break_clamps_out_of_range_index() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            // body_idx=99远超doc.body.len()，应clamp到末尾插入，不panic
            e.run("insert_section_break(99)").unwrap();
        });
        assert_eq!(doc.body.len(), 2);
        assert!(matches!(doc.body[1], BodyElement::Section(_)));
        assert_eq!(doc.sections.len(), 1);
    }

    #[test]
    fn test_section_count() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("insert_section_break(1)").unwrap();
            e.run("insert_section_break(2)").unwrap();
        });
        let mut count = -1i64;
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            count = e.eval("section_count()").unwrap();
        });
        assert_eq!(count, 2);
    }

    #[test]
    fn test_set_section_page_layout_and_get_section() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("insert_section_break(1)").unwrap();
            e.run("set_section_page_layout(0, 400.0, 600.0, 50.0, 50.0, 40.0, 40.0)").unwrap();
        });
        assert_eq!(doc.sections[0].page_size, Some((400.0, 600.0)));
        assert_eq!(doc.sections[0].margin_top, Some(50.0));

        let mut result = String::new();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            result = e.eval("get_section(0)").unwrap();
        });
        let v: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["page_size"][0], 400.0);
        assert_eq!(v["page_size"][1], 600.0);
        assert_eq!(v["margin_top"], 50.0);
    }

    #[test]
    fn test_set_section_page_layout_ignores_non_positive() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("insert_section_break(1)").unwrap();
            e.run("set_section_page_layout(0, -1.0, -1.0, -1.0, -1.0, -1.0, -1.0)").unwrap();
        });
        assert_eq!(doc.sections[0].page_size, None, "非正数不应修改page_size");
        assert_eq!(doc.sections[0].margin_top, None, "非正数不应修改margin_top");
    }

    #[test]
    fn test_set_section_header_footer() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("insert_section_break(1)").unwrap();
            e.run(r#"set_section_header_footer(0, "Header A", "Footer A")"#).unwrap();
        });
        let hf_header = doc.sections[0].header.as_ref().unwrap();
        let header_text: String = hf_header.default.as_ref().unwrap()
            .iter().map(|p| p.runs.iter().map(|r| r.text.as_str()).collect::<String>())
            .collect::<Vec<_>>().join("\n");
        assert_eq!(header_text, "Header A");

        // 清空
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run(r#"set_section_header_footer(0, "", "")"#).unwrap();
        });
        assert!(doc.sections[0].header.as_ref().unwrap().default.is_none());
        assert!(doc.sections[0].footer.as_ref().unwrap().default.is_none());
    }

    #[test]
    fn test_set_section_title_page() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("insert_section_break(1)").unwrap();
            e.run("set_section_title_page(0, true)").unwrap();
        });
        assert_eq!(doc.sections[0].title_page, Some(true));
    }

    #[test]
    fn test_set_and_clear_section_type() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("insert_section_break(1)").unwrap();
            e.run(r#"set_section_type(0, "evenPage")"#).unwrap();
        });
        assert_eq!(doc.sections[0].section_type.as_deref(), Some("evenPage"));
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run(r#"set_section_type(0, "")"#).unwrap();
        });
        assert_eq!(doc.sections[0].section_type, None);
    }

    #[test]
    fn test_set_section_columns() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("insert_section_break(1)").unwrap();
            e.run("set_section_columns(0, 3)").unwrap();
        });
        assert_eq!(doc.sections[0].columns.as_ref().unwrap().num_cols, 3);
    }

    #[test]
    fn test_set_section_columns_ignores_non_positive() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            e.run("insert_section_break(1)").unwrap();
            e.run("set_section_columns(0, 0)").unwrap();
            e.run("set_section_columns(0, -2)").unwrap();
        });
        assert!(doc.sections[0].columns.is_none());
    }

    #[test]
    fn test_section_setters_ignore_out_of_range_index() {
        let mut doc = empty_doc();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            // 尚未插入任何section，doc.sections为空，idx=0越界
            e.run("set_section_page_layout(0, 100.0, 100.0, 10.0, 10.0, 10.0, 10.0)").unwrap();
            e.run(r#"set_section_header_footer(0, "x", "y")"#).unwrap();
            e.run("set_section_title_page(0, true)").unwrap();
            e.run(r#"set_section_type(0, "evenPage")"#).unwrap();
            e.run("set_section_columns(0, 2)").unwrap();
        });
        // 未panic即说明防御生效
        assert_eq!(doc.sections.len(), 0, "越界调用不应意外创建section记录");

        let mut result = String::new();
        with_document(&mut doc, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            result = e.eval("get_section(0)").unwrap();
        });
        assert_eq!(result, "{}", "越界get_section应返回空对象");
    }


    // 独立的小 helper：避免测试模块直接依赖 roe-parser 的具体 crate 路径
    // 写法差异（roe-script 目前只在这一处需要反向验证，不值得在生产代码
    // 里加一层封装，封装在测试内部即可）。
    fn crate_docx_parse(bytes: &[u8]) -> Result<WriterDocument, String> {
        roe_parser::ooxml::docx::DocxParser::parse(bytes).map_err(|e| format!("{e:?}"))
    }

}
