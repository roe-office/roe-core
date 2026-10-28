//! roe_model::writer::WriterDocument → DOCX (OOXML WordprocessingML) 字节流
//!
//! 与 docx.rs parser 对称设计，范围对齐 parser 现状：
//! - 只写出 parser 已读取的字段：BodyElement::Paragraph（Table/Section/Image
//!   parser 不读，不写）、Paragraph.style（w:pStyle）、Run.text（w:t，仅文字，
//!   其余 Run 字段如 bold/italic/font_size 等 parser 不读，不写）
//! - list_info（w:numId/w:ilvl）刻意不写：parser 解析编号列表时把编号文本
//!   （如"1. "）直接拼进 run 文字里，list_info 只是附带记录，不是从文字反推
//!   回去的来源。若 writer 同时写 list_info 又保留 run 里已经包含的编号文字，
//!   会导致 roundtrip 后编号被解析两次（"1. 1. 正文"）。本版本选择不写
//!   list_info，run 文字按字面原样写出（已包含编号文本），生成的 docx 在
//!   Word 里文字内容正确，但不会被识别为真正的列表（无法用 Word 列表样式
//!   批量改编号）。这是已知的范围内取舍，不是遗漏——根本问题在 parser 把
//!   编号"烧"进了文字而非结构化保留，需要专项修 parser 后才能两边都改。
//! - page_size/margin_*/header/footer：parser 不读真实值（硬编码
//!   常量或恒为 None/default），writer 同样不写，保留 OOXML 默认值。
//! - word/styles.xml（StyleRegistry 的 paragraph_styles）：本次新增写出，
//!   第一版只覆盖 ParagraphStyle 的 name/based_on/bold/italic/font_size/
//!   alignment 六个字段，其余字段（line_height/indent_*/color/
//!   list_style_name 等）与 CharStyle/CellStyle 留给后续小步逐步补齐，
//!   见 ROE_Progress 交接文档。
//!   [注：本段落上方"styles 不写"的表述曾经是对的，但 alignment/indent/
//!   spacing 等段落格式字段已在更早的会话补上了写出逻辑（见下方
//!   write_paragraph_xml），本条注释本身也曾经过时未同步，此次一并订正。]
//! - OOXML 是 zip 容器，接口返回 Vec<u8>，与 xlsx_writer.rs 接口形状一致。
//!
//! 生成的最小合法 OPC 包文件清单：
//!   [Content_Types].xml
//!   _rels/.rels
//!   word/document.xml
//!   word/_rels/document.xml.rels（空关系表，占位，保持包结构完整）

use std::io::Write;

use roe_model::writer::{BodyElement, WriterDocument};

use crate::ParseError;

thread_local! {
    /// v255: list_id -> numId（write 期间由 DocxWriter::write 填充，供 write_paragraph_xml 写 numPr）
    static LIST_NUM: std::cell::RefCell<std::collections::HashMap<String, u32>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

thread_local! {
    /// v282: 合法的 (脚注id, 尾注id, 批注id)，write 期间填充；
    /// write_paragraph_xml 据此跳过悬空引用，避免写出 Word 打不开的 docx。
    static NOTE_IDS: std::cell::RefCell<(Vec<u32>, Vec<u32>, Vec<u32>)> =
        std::cell::RefCell::new((Vec::new(), Vec::new(), Vec::new()));
}

fn note_id_ok(kind: u8, id: u32) -> bool {
    NOTE_IDS.with(|n| {
        let n = n.borrow();
        let v = match kind {
            0 => &n.0,
            1 => &n.1,
            _ => &n.2,
        };
        v.contains(&id)
    })
}

pub struct DocxWriter;

impl DocxWriter {
    /// WriterDocument 内存模型 → docx 字节流（zip 包）
    pub fn write(doc: &WriterDocument) -> Result<Vec<u8>, ParseError> {
        // 图片收集：body 内所有 BodyElement::Image 各自独立编号，不做
        // 内容去重（与 pptx_writer.rs 的既有约定一致：parser 解析时也是
        // 独立收集，不跨位置共享同一张图片的物理文件）。
        let mut media_counter: u32 = 1;
        let mut image_rels: Vec<(String, String)> = Vec::new(); // (rId, 文件名)
        let mut images: Vec<(String, Vec<u8>)> = Vec::new(); // (zip内路径, 字节数据)

        // v255: ROE_ 列表 -> numbering.xml
        let num_list = collect_numbering(doc);
        LIST_NUM.with(|m| {
            let mut m = m.borrow_mut();
            m.clear();
            for (lid, nid, _) in &num_list { m.insert(lid.clone(), *nid); }
        });
        let numbering_xml = write_numbering_xml(doc, &num_list);
        NOTE_IDS.with(|n| { *n.borrow_mut() = nc_collect_ids(doc); });
        V318G_CHARTS.with(|c| c.borrow_mut().clear());
        let document_xml = write_document_xml_with_media(
            doc,
            &mut media_counter,
            &mut image_rels,
            &mut images,
        );

        let (document_xml, hf_out) = inject_header_footer(doc, document_xml);
        let v318g_parts: Vec<(String, String, bool)> = V318G_CHARTS.with(|c| std::mem::take(&mut *c.borrow_mut()));

        // 有图片时才需要在 Content_Types 里声明对应扩展名，避免固定
        // 声明所有类型导致与"实际内容是否含图片"脱节（虽然多声明几个
        // Default Extension 本身不会导致 Word 打不开，但保持"按需生成"
        // 与本文件其余部分风格一致）。
        let mime_exts: Vec<&str> = {
            let mut exts: Vec<&str> = image_rels
                .iter()
                .filter_map(|(_, filename)| filename.rsplit('.').next())
                .collect();
            exts.sort_unstable();
            exts.dedup();
            exts
        };

        LIST_NUM.with(|m| m.borrow_mut().clear());
        let mut content_types = write_content_types(&mime_exts);
        if !num_list.is_empty() {
            content_types = content_types.replace("</Types>", r#"<Override PartName="/word/numbering.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.numbering+xml"/>
</Types>"#);
        }
        let root_rels = write_root_rels();
        let mut document_rels = write_document_rels(&image_rels);
        if !num_list.is_empty() {
            document_rels = document_rels.replace("</Relationships>", r#"<Relationship Id="rIdNumbering" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/numbering" Target="numbering.xml"/></Relationships>"#);
        }
        {
            let mut ov = String::new();
            for (path, _) in &hf_out.parts {
                let ct = if path.starts_with("word/header") {
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.header+xml"
                } else if path.starts_with("word/footer") {
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.footer+xml"
                } else {
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.settings+xml"
                };
                ov.push_str(&format!("<Override PartName=\"/{}\" ContentType=\"{}\"/>", path, ct));
            }
            if !ov.is_empty() {
                content_types = content_types.replace("</Types>", &format!("{}</Types>", ov));
            }
            let mut rl = String::new();
            for (rid, ty, target) in &hf_out.rels {
                rl.push_str(&format!(
                    "<Relationship Id=\"{}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/{}\" Target=\"{}\"/>",
                    rid, ty, target
                ));
            }
            if !rl.is_empty() {
                document_rels = document_rels.replace("</Relationships>", &format!("{}</Relationships>", rl));
            }
        }
        // v282: 脚注/尾注/批注 part
        let mut hf_out = hf_out;
        let nc = inject_notes_comments(doc, &mut hf_out);
        NOTE_IDS.with(|n| { *n.borrow_mut() = (Vec::new(), Vec::new(), Vec::new()); });
        if !nc.overrides.is_empty() {
            content_types = content_types.replace("</Types>", &format!("{}</Types>", nc.overrides));
        }
        if !nc.rels.is_empty() {
            document_rels = document_rels.replace("</Relationships>", &format!("{}</Relationships>", nc.rels));
        }
        for (i, (_, _, is_stat)) in v318g_parts.iter().enumerate() {
            let n = i + 1;
            if *is_stat {
                content_types = content_types.replace("</Types>", &format!("<Override PartName=\"/word/charts/chartEx{n}.xml\" ContentType=\"application/vnd.ms-office.chartex+xml\"/></Types>"));
                document_rels = document_rels.replace("</Relationships>", &format!("<Relationship Id=\"rIdChart{n}\" Type=\"http://schemas.microsoft.com/office/2014/relationships/chartEx\" Target=\"charts/chartEx{n}.xml\"/></Relationships>"));
            } else {
                content_types = content_types.replace("</Types>", &format!("<Override PartName=\"/word/charts/chart{n}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.drawingml.chart+xml\"/></Types>"));
                document_rels = document_rels.replace("</Relationships>", &format!("<Relationship Id=\"rIdChart{n}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart\" Target=\"charts/chart{n}.xml\"/></Relationships>"));
            }
        }
        let styles_xml = write_styles_xml(doc);

        let buf = Vec::new();
        let cursor = std::io::Cursor::new(buf);
        let mut zw = zip::ZipWriter::new(cursor);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);

        write_zip_entry(&mut zw, opts, "[Content_Types].xml", &content_types)?;
        write_zip_entry(&mut zw, opts, "_rels/.rels", &root_rels)?;
        write_zip_entry(&mut zw, opts, "word/document.xml", &document_xml)?;
        write_zip_entry(&mut zw, opts, "word/_rels/document.xml.rels", &document_rels)?;
        write_zip_entry(&mut zw, opts, "word/styles.xml", &styles_xml)?;
        if !num_list.is_empty() {
            write_zip_entry(&mut zw, opts, "word/numbering.xml", &numbering_xml)?;
        }

        for (path, xml) in &hf_out.parts {
            write_zip_entry(&mut zw, opts, path, xml)?;
        }
        for (path, xml) in &nc.parts {
            write_zip_entry(&mut zw, opts, path, xml)?;
        }
        for (i, (xml, _, is_stat)) in v318g_parts.iter().enumerate() {
            let n = i + 1;
            let path = if *is_stat { format!("word/charts/chartEx{n}.xml") } else { format!("word/charts/chart{n}.xml") };
            write_zip_entry(&mut zw, opts, &path, xml)?;
        }
        for (path, data) in &images {
            write_zip_binary(&mut zw, opts, path, data)?;
        }

        let cursor = zw
            .finish()
            .map_err(|e| ParseError::Malformed(format!("zip finish failed: {e}")))?;
        Ok(cursor.into_inner())
    }
}

// ─── v279: 页眉页脚 part（文字 + 页码域）─────────────────────────────────────
struct HfOut {
    parts: Vec<(String, String)>,
    rels: Vec<(String, String, String)>,
}

const HF_NS: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships""#;

/// 把页码占位符 run 换成标准复杂域（begin/instrText/separate/缓存值/end 各一个 run）
fn hf_replace_fields(mut xml: String) -> String {
    for (sent, instr) in [("\u{E000}P\u{E000}", "PAGE"), ("\u{E000}N\u{E000}", "NUMPAGES")] {
        let needle = format!("<w:t xml:space=\"preserve\">{}</w:t></w:r>", sent);
        while let Some(pos) = xml.find(&needle) {
            let run_start = match xml[..pos].rfind("<w:r>") { Some(s) => s, None => break };
            let rpr = xml[run_start + 5..pos].to_string();
            let end = pos + needle.len();
            let rep = format!(
                "<w:r>{r}<w:fldChar w:fldCharType=\"begin\"/></w:r><w:r>{r}<w:instrText xml:space=\"preserve\"> {i} </w:instrText></w:r><w:r>{r}<w:fldChar w:fldCharType=\"separate\"/></w:r><w:r>{r}<w:t>1</w:t></w:r><w:r>{r}<w:fldChar w:fldCharType=\"end\"/></w:r>",
                r = rpr, i = instr
            );
            xml.replace_range(run_start..end, &rep);
        }
    }
    xml
}

fn hf_part_xml(is_header: bool, paras: &[roe_model::writer::Paragraph]) -> String {
    let tag = if is_header { "w:hdr" } else { "w:ftr" };
    let mut body = String::new();
    for p in paras {
        let mut q = p.clone();
        for r in q.runs.iter_mut() {
            match r.field {
                Some(roe_model::writer::FieldKind::PageNumber) => {
                    r.text = "\u{E000}P\u{E000}".to_string();
                    r.field = None;
                }
                Some(roe_model::writer::FieldKind::NumPages) => {
                    r.text = "\u{E000}N\u{E000}".to_string();
                    r.field = None;
                }
                _ => {}
            }
        }
        body.push_str(&write_paragraph_xml(&q));
    }
    if body.is_empty() { body.push_str("<w:p/>"); }
    let body = hf_replace_fields(body).replace("<w:pStyle w:val=\"\"/>", "");
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<{tag} {ns}>{body}</{tag}>",
        tag = tag, ns = HF_NS, body = body
    )
}

/// 在 document.xml 每个 <w:sectPr> 后面插入 header/footer 引用，并生成对应 part。
fn inject_header_footer(doc: &WriterDocument, document_xml: String) -> (String, HfOut) {
    let mut out = HfOut { parts: Vec::new(), rels: Vec::new() };
    let tag = "<w:sectPr>";
    let total = document_xml.matches(tag).count();
    if total == 0 { return (document_xml, out); }
    let mut cache: std::collections::HashMap<String, (String, String)> = std::collections::HashMap::new();
    let (mut n_h, mut n_f) = (0u32, 0u32);
    let mut result = String::with_capacity(document_xml.len() + 512);
    let mut rest: &str = &document_xml;
    let mut idx = 0usize;
    while let Some(pos) = rest.find(tag) {
        result.push_str(&rest[..pos + tag.len()]);
        rest = &rest[pos + tag.len()..];
        let section = if doc.sections.is_empty() {
            None
        } else if idx + 1 == total {
            doc.sections.last()
        } else {
            doc.sections.get(idx)
        };
        idx += 1;
        let hset = section.and_then(|s| s.header.as_ref()).unwrap_or(&doc.header);
        let fset = section.and_then(|s| s.footer.as_ref()).unwrap_or(&doc.footer);
        let eo = section.and_then(|s| s.even_and_odd_headers).unwrap_or(doc.even_and_odd_headers);
        let mut refs = String::new();
        for is_header in [true, false] {
            let set = if is_header { hset } else { fset };
            for ty in ["default", "even", "first"] {
                let slot = match ty {
                    "default" => set.default.as_ref(),
                    "even" => { if eo { set.even.as_ref().or(set.default.as_ref()) } else { None } }
                    _ => set.first.as_ref(),
                };
                let Some(paras) = slot else { continue };
                let xml = hf_part_xml(is_header, paras);
                let key = format!("{}|{}", is_header, xml);
                let rid = if let Some(v) = cache.get(&key) {
                    v.0.clone()
                } else {
                    let (kind, n) = if is_header { n_h += 1; ("header", n_h) } else { n_f += 1; ("footer", n_f) };
                    let file = format!("{}{}.xml", kind, n);
                    let rid = format!("rId{}{}", if is_header { "Hdr" } else { "Ftr" }, n);
                    out.parts.push((format!("word/{}", file), xml));
                    out.rels.push((rid.clone(), kind.to_string(), file.clone()));
                    cache.insert(key, (rid.clone(), file));
                    rid
                };
                let el = if is_header { "headerReference" } else { "footerReference" };
                refs.push_str(&format!("<w:{} w:type=\"{}\" r:id=\"{}\"/>", el, ty, rid));
            }
        }
        result.push_str(&refs);
    }
    result.push_str(rest);
    if doc.even_and_odd_headers {
        let settings = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<w:settings xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"><w:evenAndOddHeaders/></w:settings>"
        );
        out.parts.push(("word/settings.xml".to_string(), settings));
        out.rels.push(("rIdSettings".to_string(), "settings".to_string(), "settings.xml".to_string()));
    }
    (result, out)
}

// ─── v282: 脚注 / 尾注 / 批注 part ─────────────────────────────────────────
struct NcOut {
    parts: Vec<(String, String)>,
    overrides: String,
    rels: String,
}

const NC_NS: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:w14="http://schemas.microsoft.com/office/word/2010/wordml" xmlns:w15="http://schemas.microsoft.com/office/word/2012/wordml" mc:Ignorable="w14 w15""#;

fn nc_collect_ids(doc: &WriterDocument) -> (Vec<u32>, Vec<u32>, Vec<u32>) {
    use roe_model::writer::NoteType;
    let f: Vec<u32> = doc.footnotes.iter().filter(|n| n.note_type == NoteType::Normal).map(|n| n.id).collect();
    let e: Vec<u32> = doc.endnotes.iter().filter(|n| n.note_type == NoteType::Normal).map(|n| n.id).collect();
    let c: Vec<u32> = doc.comments.iter().map(|c| c.id).collect();
    (f, e, c)
}

fn nc_free_id(used: &[u32]) -> u32 {
    let mut n = 0u32;
    while used.contains(&n) { n += 1; }
    n
}

fn nc_para_id(id: u32) -> String {
    format!("{:08X}", 0x1000_0000u32 + (id & 0xF_FFFF) * 0x101)
}

fn nc_notes_xml(notes: &[roe_model::writer::Note], is_foot: bool, sep2: u32) -> String {
    use roe_model::writer::NoteType;
    let tag = if is_foot { "footnote" } else { "endnote" };
    let root = if is_foot { "footnotes" } else { "endnotes" };
    let sp = r#"<w:pPr><w:spacing w:after="0" w:line="240" w:lineRule="auto"/></w:pPr>"#;
    let mut body = String::new();
    body.push_str(&format!(
        r#"<w:{tag} w:type="separator" w:id="-1"><w:p>{sp}<w:r><w:separator/></w:r></w:p></w:{tag}>"#
    ));
    body.push_str(&format!(
        r#"<w:{tag} w:type="continuationSeparator" w:id="{sep2}"><w:p>{sp}<w:r><w:continuationSeparator/></w:r></w:p></w:{tag}>"#
    ));
    for n in notes.iter().filter(|n| n.note_type == NoteType::Normal) {
        let mut inner = String::new();
        for el in &n.content {
            if let BodyElement::Paragraph(p) = el {
                inner.push_str(&write_paragraph_xml(p));
            }
        }
        if inner.is_empty() { inner.push_str("<w:p></w:p>"); }
        body.push_str(&format!(r#"<w:{tag} w:id="{}">{}</w:{tag}>"#, n.id, inner));
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<w:{root} {NC_NS}>{body}</w:{root}>"
    )
}

fn nc_comments_xml(comments: &[roe_model::writer::Comment]) -> (String, String) {
    let mut body = String::new();
    let mut ex = String::new();
    for c in comments {
        let mut ps: Vec<String> = Vec::new();
        for el in &c.content {
            if let BodyElement::Paragraph(p) = el {
                ps.push(write_paragraph_xml(p));
            }
        }
        if ps.is_empty() { ps.push("<w:p></w:p>\n".to_string()); }
        // 第一段开头补 annotationRef（Word 约定）
        let ar = r#"<w:r><w:annotationRef/></w:r>"#;
        let first = ps[0].clone();
        ps[0] = if first.starts_with("<w:p><w:pPr>") {
            match first.find("</w:pPr>") {
                Some(i) => format!("{}{}{}", &first[..i + 8], ar, &first[i + 8..]),
                None => first.clone(),
            }
        } else if first.starts_with("<w:p>") {
            format!("<w:p>{}{}", ar, &first[5..])
        } else {
            first.clone()
        };
        // 最后一段加 w14:paraId（commentsExtended 靠它关联 done / 回复）
        let pid = nc_para_id(c.id);
        let li = ps.len() - 1;
        if ps[li].starts_with("<w:p>") {
            let t = ps[li].clone();
            ps[li] = format!(r#"<w:p w14:paraId="{}" w14:textId="77777777">{}"#, pid, &t[5..]);
        }
        let mut attrs = format!(r#" w:id="{}" w:author="{}""#, c.id, xml_escape_attr(&c.author));
        if let Some(d) = &c.date {
            attrs.push_str(&format!(r#" w:date="{}""#, xml_escape_attr(d)));
        }
        if let Some(i) = &c.initials {
            attrs.push_str(&format!(r#" w:initials="{}""#, xml_escape_attr(i)));
        }
        body.push_str(&format!("<w:comment{}>{}</w:comment>", attrs, ps.concat()));

        let mut e = format!(
            r#"<w15:commentEx w15:paraId="{}" w15:done="{}""#,
            pid,
            if c.done { 1 } else { 0 }
        );
        if let Some(pp) = c.parent_id {
            if comments.iter().any(|x| x.id == pp) {
                e.push_str(&format!(r#" w15:paraIdParent="{}""#, nc_para_id(pp)));
            }
        }
        e.push_str("/>");
        ex.push_str(&e);
    }
    let head = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n";
    let comments_xml = format!("{head}<w:comments {NC_NS}>{body}</w:comments>");
    let ext_xml = format!(
        "{head}<w15:commentsEx xmlns:w15=\"http://schemas.microsoft.com/office/word/2012/wordml\" xmlns:mc=\"http://schemas.openxmlformats.org/markup-compatibility/2006\" mc:Ignorable=\"w15\">{ex}</w15:commentsEx>"
    );
    (comments_xml, ext_xml)
}

fn inject_notes_comments(doc: &WriterDocument, hf: &mut HfOut) -> NcOut {
    let mut out = NcOut { parts: Vec::new(), overrides: String::new(), rels: String::new() };
    let rel_base = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
    let ct_base = "application/vnd.openxmlformats-officedocument.wordprocessingml";
    let (fids, eids, _) = nc_collect_ids(doc);
    let mut pr = String::new();

    if !fids.is_empty() {
        let s2 = nc_free_id(&fids);
        out.parts.push(("word/footnotes.xml".to_string(), nc_notes_xml(&doc.footnotes, true, s2)));
        out.overrides.push_str(&format!(r#"<Override PartName="/word/footnotes.xml" ContentType="{ct_base}.footnotes+xml"/>"#));
        out.rels.push_str(&format!(r#"<Relationship Id="rIdFootnotes" Type="{rel_base}/footnotes" Target="footnotes.xml"/>"#));
        pr.push_str(&format!(r#"<w:footnotePr><w:footnote w:id="-1"/><w:footnote w:id="{s2}"/></w:footnotePr>"#));
    }
    if !eids.is_empty() {
        let s2 = nc_free_id(&eids);
        out.parts.push(("word/endnotes.xml".to_string(), nc_notes_xml(&doc.endnotes, false, s2)));
        out.overrides.push_str(&format!(r#"<Override PartName="/word/endnotes.xml" ContentType="{ct_base}.endnotes+xml"/>"#));
        out.rels.push_str(&format!(r#"<Relationship Id="rIdEndnotes" Type="{rel_base}/endnotes" Target="endnotes.xml"/>"#));
        pr.push_str(&format!(r#"<w:endnotePr><w:endnote w:id="-1"/><w:endnote w:id="{s2}"/></w:endnotePr>"#));
    }
    if !doc.comments.is_empty() {
        let (cx, ex) = nc_comments_xml(&doc.comments);
        out.parts.push(("word/comments.xml".to_string(), cx));
        out.parts.push(("word/commentsExtended.xml".to_string(), ex));
        out.overrides.push_str(&format!(r#"<Override PartName="/word/comments.xml" ContentType="{ct_base}.comments+xml"/>"#));
        out.overrides.push_str(&format!(r#"<Override PartName="/word/commentsExtended.xml" ContentType="{ct_base}.commentsExtended+xml"/>"#));
        out.rels.push_str(&format!(r#"<Relationship Id="rIdComments" Type="{rel_base}/comments" Target="comments.xml"/>"#));
        out.rels.push_str(r#"<Relationship Id="rIdCommentsExt" Type="http://schemas.microsoft.com/office/2011/relationships/commentsExtended" Target="commentsExtended.xml"/>"#);
    }
    // settings.xml：Word 要求分隔线注释在 settings 里登记
    if !pr.is_empty() {
        if let Some(i) = hf.parts.iter().position(|e| e.0 == "word/settings.xml") {
            let x = hf.parts[i].1.replace("</w:settings>", &format!("{}</w:settings>", pr));
            hf.parts[i].1 = x;
        } else {
            let s = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<w:settings xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">{}</w:settings>",
                pr
            );
            out.parts.push(("word/settings.xml".to_string(), s));
            out.overrides.push_str(&format!(r#"<Override PartName="/word/settings.xml" ContentType="{ct_base}.settings+xml"/>"#));
            out.rels.push_str(&format!(r#"<Relationship Id="rIdSettings" Type="{rel_base}/settings" Target="settings.xml"/>"#));
        }
    }
    out
}

// ─── v255: word/numbering.xml（ROE_ 自建列表）─────────────────────────────────
/// 返回 (list_id, numId, 样式名)。numId 从 1 开始，每个 list_id 一个，
/// abstractNumId 与 numId 相同（每个列表独立一份 abstractNum，避免导入端共享计数器）。
fn collect_numbering(doc: &WriterDocument) -> Vec<(String, u32, String)> {
    let mut out: Vec<(String, u32, String)> = Vec::new();
    for elem in &doc.body {
        if let BodyElement::Paragraph(p) = elem {
            if let Some(li) = &p.list_info {
                if li.style_name.starts_with("ROE_")
                    && doc.styles.list_styles.contains_key(&li.style_name)
                    && !out.iter().any(|(id, _, _)| id == &li.list_id)
                {
                    let n = out.len() as u32 + 1;
                    out.push((li.list_id.clone(), n, li.style_name.clone()));
                }
            }
        }
    }
    out
}

fn write_numbering_xml(doc: &WriterDocument, lists: &[(String, u32, String)]) -> String {
    use roe_model::writer::{ListLevelStyle, NumFormat};
    let mut abs_xml = String::new();
    let mut num_xml = String::new();
    for (_lid, nid, style_name) in lists {
        let Some(ls) = doc.styles.list_styles.get(style_name) else { continue };
        abs_xml.push_str(&format!(
            r#"<w:abstractNum w:abstractNumId="{nid}"><w:multiLevelType w:val="hybridMultilevel"/><w:name w:val="{style_name}"/>"#
        ));
        for (i, lv) in ls.levels.iter().enumerate().take(9) {
            let (fmt, text, start, ind) = match lv {
                ListLevelStyle::Number(n) => {
                    let f = match n.format {
                        NumFormat::Arabic => "decimal",
                        NumFormat::LowerAlpha => "lowerLetter",
                        NumFormat::UpperAlpha => "upperLetter",
                        NumFormat::LowerRoman => "lowerRoman",
                        NumFormat::UpperRoman => "upperRoman",
                        NumFormat::None => "none",
                    };
                    (f, format!("{}%{}{}", n.prefix, i + 1, n.suffix), n.start_value, &n.indent)
                }
                ListLevelStyle::Bullet(b) => {
                    ("bullet", format!("{}{}{}", b.prefix, b.bullet_char, b.suffix), 1u32, &b.indent)
                }
            };
            let left = (ind.margin_left as f64 * 20.0).round() as i64;
            let hanging = ((-ind.text_indent) as f64 * 20.0).round().max(0.0) as i64;
            abs_xml.push_str(&format!(
                r#"<w:lvl w:ilvl="{i}"><w:start w:val="{start}"/><w:numFmt w:val="{fmt}"/><w:lvlText w:val="{}"/><w:lvlJc w:val="left"/><w:pPr><w:ind w:left="{left}" w:hanging="{hanging}"/></w:pPr></w:lvl>"#,
                xml_escape_attr(&text)
            ));
        }
        abs_xml.push_str("</w:abstractNum>");
        num_xml.push_str(&format!(
            r#"<w:num w:numId="{nid}"><w:abstractNumId w:val="{nid}"/></w:num>"#
        ));
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">{abs_xml}{num_xml}</w:numbering>"#
    )
}

fn write_zip_entry<W: Write + std::io::Seek>(
    zw: &mut zip::ZipWriter<W>,
    opts: zip::write::SimpleFileOptions,
    path: &str,
    content: &str,
) -> Result<(), ParseError> {
    zw.start_file(path, opts)
        .map_err(|e| ParseError::Malformed(format!("zip start_file '{path}' failed: {e}")))?;
    zw.write_all(content.as_bytes())
        .map_err(|e| ParseError::Malformed(format!("zip write '{path}' failed: {e}")))?;
    Ok(())
}

/// 与 write_zip_entry 相同，但写二进制内容（图片字节），不做 UTF-8 假设。
fn write_zip_binary<W: Write + std::io::Seek>(
    zw: &mut zip::ZipWriter<W>,
    opts: zip::write::SimpleFileOptions,
    path: &str,
    content: &[u8],
) -> Result<(), ParseError> {
    zw.start_file(path, opts)
        .map_err(|e| ParseError::Malformed(format!("zip start_file '{path}' failed: {e}")))?;
    zw.write_all(content)
        .map_err(|e| ParseError::Malformed(format!("zip write '{path}' failed: {e}")))?;
    Ok(())
}

// ─── [Content_Types].xml ──────────────────────────────────────────────────────

fn write_content_types(image_exts: &[&str]) -> String {
    let mut defaults = String::new();
    for ext in image_exts {
        let content_type = match *ext {
            "png" => "image/png",
            "jpeg" | "jpg" => "image/jpeg",
            "gif" => "image/gif",
            // "bin" 等未知类型：与 pptx_writer.rs 的既有取舍不同——pptx
            // 侧对 image/unknown 落到 .bin 但未在 Content_Types 声明，
            // 属于潜在缺口；这里顺手补全，避免同样问题在 docx 侧重演，
            // 不影响 pptx_writer.rs（两份文件独立维护）。
            _ => "application/octet-stream",
        };
        defaults.push_str(&format!(
            r#"<Default Extension="{ext}" ContentType="{content_type}"/>"#
        ));
    }

    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
{defaults}<Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
<Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>
</Types>"#
    )
}

// ─── _rels/.rels（包级关系，指向 document.xml）────────────────────────────────

fn write_root_rels() -> String {
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#
        .to_string()
}

// ─── word/_rels/document.xml.rels（空关系表，占位）────────────────────────────

fn write_document_rels(image_rels: &[(String, String)]) -> String {
    let mut rels = String::new();
    for (rid, filename) in image_rels {
        rels.push_str(&format!(
            r#"<Relationship Id="{rid}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/{filename}"/>"#
        ));
    }
    // styles.xml 关系：固定使用 "rIdStyles" 这个非数字 rId，从字符串层面
    // 避免与上面 image_rels 循环产出的 "rId1"/"rId2".. 数字编号冲突。
    // 总是写出（不判断 doc.styles 是否为空），理由见本函数调用处附近的
    // 模块级说明。
    rels.push_str(
        r#"<Relationship Id="rIdStyles" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>"#
    );
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
{rels}</Relationships>"#
    )
}

// ─── word/styles.xml ────────────────────────────────────────────────────────
// 第一版：只覆盖 ParagraphStyle 的 name/based_on/bold/italic/font_size/
// alignment 六个字段。line_height/indent_left/indent_first/space_before/
// space_after/color/list_style_name 等其余 ParagraphStyle 字段，以及
// CharStyle/CellStyle 整体，留给后续小步逐步补齐（参照《Writer接口开发
// 设计文档》1.1节字符/段落格式补全时"先跑通管道、再逐步加字段"的节奏）。
/// v225：把 StyleRegistry 的 docDefaults 字段写成 <w:docDefaults>。
/// 全部为 None 时返回空串。读入端对应 docx.rs 的 docDefaults 解析区。
fn write_doc_defaults_xml(styles: &roe_model::style::StyleRegistry) -> String {
    fn esc(s: &str) -> String {
        s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
    }
    let mut rpr = String::new();
    if styles.doc_default_font_family.is_some() || styles.doc_default_font_family_ascii.is_some() {
        let mut attrs = String::new();
        if let Some(a) = styles.doc_default_font_family_ascii.as_deref() {
            attrs.push_str(&format!(r#" w:ascii="{0}" w:hAnsi="{0}""#, esc(a)));
        }
        if let Some(e) = styles.doc_default_font_family.as_deref() {
            attrs.push_str(&format!(r#" w:eastAsia="{}""#, esc(e)));
        }
        rpr.push_str(&format!("<w:rFonts{attrs}/>"));
    }
    if let Some(c) = styles.doc_default_color {
        rpr.push_str(&format!(r#"<w:color w:val="{:02X}{:02X}{:02X}"/>"#, c.r, c.g, c.b));
    }
    if let Some(sz) = styles.doc_default_font_size {
        let hp = (sz * 2.0).round() as i64;
        rpr.push_str(&format!(r#"<w:sz w:val="{hp}"/><w:szCs w:val="{hp}"/>"#));
    }

    let mut ppr = String::new();
    let mut sp = String::new();
    if let Some(v) = styles.doc_default_space_before {
        sp.push_str(&format!(r#" w:before="{}""#, (v * 20.0).round() as i64));
    }
    if let Some(v) = styles.doc_default_space_after {
        sp.push_str(&format!(r#" w:after="{}""#, (v * 20.0).round() as i64));
    }
    if let Some(v) = styles.doc_default_line_height {
        sp.push_str(&format!(r#" w:line="{}" w:lineRule="auto""#, (v * 240.0).round() as i64));
    }
    if !sp.is_empty() {
        ppr.push_str(&format!("<w:spacing{sp}/>"));
    }
    let mut ind = String::new();
    if let Some(v) = styles.doc_default_indent_left {
        ind.push_str(&format!(r#" w:left="{}""#, (v * 20.0).round() as i64));
    }
    if let Some(v) = styles.doc_default_indent_first {
        if v < 0.0 {
            ind.push_str(&format!(r#" w:hanging="{}""#, ((-v) * 20.0).round() as i64));
        } else if v > 0.0 {
            ind.push_str(&format!(r#" w:firstLine="{}""#, (v * 20.0).round() as i64));
        }
    }
    if !ind.is_empty() {
        ppr.push_str(&format!("<w:ind{ind}/>"));
    }

    if rpr.is_empty() && ppr.is_empty() {
        return String::new();
    }
    let mut out = String::from("<w:docDefaults>");
    if !rpr.is_empty() {
        out.push_str(&format!("<w:rPrDefault><w:rPr>{rpr}</w:rPr></w:rPrDefault>"));
    }
    if !ppr.is_empty() {
        out.push_str(&format!("<w:pPrDefault><w:pPr>{ppr}</w:pPr></w:pPrDefault>"));
    }
    out.push_str("</w:docDefaults>\n");
    out
}

fn write_styles_xml(doc: &WriterDocument) -> String {
    let mut styles_body = String::new();

    for style in doc.styles.paragraph_styles.values() {
        styles_body.push_str(&write_one_paragraph_style_xml(style));
    }

    for style in doc.styles.char_styles.values() {
        styles_body.push_str(&write_one_char_style_xml(style));
    }

    let doc_defaults_xml = write_doc_defaults_xml(&doc.styles);

    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
{doc_defaults_xml}{styles_body}</w:styles>"#
    )
}

/// <w:rPr> 内字符级属性的共用拼接逻辑，供 ParagraphStyle 和 CharStyle
/// 两条写出路径复用（两者都需要在各自的 <w:style> 定义里带一份默认
/// 字符格式）。参数取两个结构体字段的并集：ParagraphStyle 目前没有
/// strikethrough/background/script 三个字段，调用处传 None 即可，不
/// 假装 ParagraphStyle 拥有这些字段。
///
/// strikethrough/background/script 三者的写法分别照抄 write_run_props_xml
/// 里 Run.strikethrough/Run.background/Run.script 已验证过的既有实现
/// （background 用 w:shd 不用 w:highlight，理由同该函数头注释：前者
/// 支持任意RGB，精度更高，不会像16种预设颜色名那样有损）。
///
/// 返回值不含外层 <w:rPr>...</w:rPr> 包裹，只是内部片段拼接结果，
/// 全部为空时返回空字符串——由调用方决定是否要包一层 <w:rPr>（比如
/// ParagraphStyle 有一步"rpr是否为空"的判断逻辑，让调用方自己包更
/// 灵活，不在这个函数里强制包一层再让调用方剥掉）。
fn write_char_props_rpr_xml(
    bold: bool,
    italic: bool,
    font_size: Option<f32>,
    color: Option<roe_model::common::Color>,
    strikethrough: Option<bool>,
    background: Option<roe_model::common::Color>,
    script: Option<roe_model::writer::ScriptPosition>,
    underline: bool,
    font_family: Option<String>,
    font_family_ascii: Option<String>,
) -> String {
    let mut rpr = String::new();
    if bold {
        rpr.push_str("<w:b/>");
    }
    if italic {
        rpr.push_str("<w:i/>");
    }
    if let Some(true) = strikethrough {
        rpr.push_str("<w:strike/>");
    }
    if let Some(size) = font_size {
        // OOXML w:sz 单位是半磅（half-point），例如 12pt -> w:val="24"
        let half_points = (size * 2.0).round() as i64;
        rpr.push_str(&format!(r#"<w:sz w:val="{half_points}"/>"#));
    }
    // color：<w:color w:val="RRGGBB"/>，不是 xml_color_attr 那种属性
    // 片段写法——后者是给表格边框 w:color="..." 用的，标签形状不同，
    // 不能混用。
    if let Some(c) = color {
        rpr.push_str(&format!(
            r#"<w:color w:val="{:02X}{:02X}{:02X}"/>"#,
            c.r, c.g, c.b
        ));
    }
    // background：与 write_run_props_xml 里 run.background 的写法完全
    // 一致，走 w:shd 而非 w:highlight（理由见函数头注释）。
    if let Some(bg) = background {
        rpr.push_str(&format!(
            r#"<w:shd w:val="clear" w:color="auto" {}/>"#,
            xml_fill_attr(Some(bg))
        ));
    }
    match script {
        Some(roe_model::writer::ScriptPosition::Superscript) => {
            rpr.push_str(r#"<w:vertAlign w:val="superscript"/>"#);
        }
        Some(roe_model::writer::ScriptPosition::Subscript) => {
            rpr.push_str(r#"<w:vertAlign w:val="subscript"/>"#);
        }
        Some(roe_model::writer::ScriptPosition::Normal) | None => {}
    }
    // 下划线：CharStyle/ParagraphStyle 只有 bool 字段，没有 Run 那样的
    // underline_style 四选一，语义上只有"true才写w:u single"说得通——
    // false 和"没设置"在样式定义层面没有区分的必要，与 Run 侧
    // underline_val_str 需要区分 Some(false)/None 的场景不同，不能
    // 直接复用那个辅助函数（参数类型也是 &Run，签名对不上）。
    if underline {
        rpr.push_str(r#"<w:u w:val="single"/>"#);
    }
    // 字体名：与 write_run_props_xml 的 w:rFonts 写法一致，东亚字体走
    // w:eastAsia，西文字体走 w:ascii。
    if font_family.is_some() || font_family_ascii.is_some() {
        let mut attrs = String::new();
        if let Some(ref ascii) = font_family_ascii {
            attrs.push_str(&format!(r#" w:ascii="{}""#, xml_escape_attr(ascii)));
        }
        if let Some(ref east_asia) = font_family {
            attrs.push_str(&format!(r#" w:eastAsia="{}""#, xml_escape_attr(east_asia)));
        }
        rpr.push_str(&format!(r#"<w:rFonts{attrs}/>"#));
    }
    rpr
}

/// CharStyle 写出层第一小步：八个基础字段（name/based_on/bold/italic/
/// font_size/color/strikethrough/script/underline/font_family/
/// font_family_ascii），不含 background——后者需要先设计"RGB→最接近
/// 预设颜色名"的映射（CharStyle 对应解析路径只认 w:highlight 16种
/// 预设色，不认 w:shd，与 ParagraphStyle/Run 的 background 写法不能
/// 直接复用），留作独立的第二小步，见交接文档。
fn write_one_char_style_xml(style: &roe_model::style::CharStyle) -> String {
    // background：此前留 None 是因为解析层（read_styles 的 in_rpr 分支）
    // 当时只认 w:highlight（16种预设色名），不认 w:shd（任意RGB）。本次
    // 会话已经给 read_styles 补上了 w:shd 解析（ROE-STYLE-SHD，新增
    // CharProps.shd_background 字段+to_char_style优先级+merge_char继承
    // 合并三处），所以这里不再需要设计"RGB→最接近预设色名"的有损映射，
    // 直接传 style.background，写出 w:shd，与 Run.background/
    // write_char_props_rpr_xml 函数本身早就实现好的 w:shd 写法保持一致
    // （该函数一直就支持 background 参数，只是此前调用处不敢传，因为
    // 传了也读不回来；现在读得回来了，接上即可，是"零成本"的最后一步）。
    let rpr = write_char_props_rpr_xml(
        style.bold,
        style.italic,
        style.font_size,
        style.color,
        style.strikethrough,
        style.background,
        style.script,
        style.underline,
        style.font_family.clone(),
        style.font_family_ascii.clone(),
    );
    let rpr_xml = if rpr.is_empty() {
        String::new()
    } else {
        format!("<w:rPr>{rpr}</w:rPr>")
    };

    let based_on_xml = match &style.based_on {
        Some(parent) => format!(r#"<w:basedOn w:val="{}"/>"#, xml_escape_attr(parent)),
        None => String::new(),
    };

    format!(
        r#"<w:style w:type="character" w:styleId="{}"><w:name w:val="{}"/>{based_on_xml}{rpr_xml}</w:style>"#,
        xml_escape_attr(&style.name),
        xml_escape_attr(&style.name),
    )
}

fn write_one_paragraph_style_xml(style: &roe_model::style::ParagraphStyle) -> String {
    let rpr = write_char_props_rpr_xml(
        style.bold,
        style.italic,
        style.font_size,
        style.color,
        None, // ParagraphStyle 目前没有 strikethrough 字段
        None, // ParagraphStyle 目前没有 background 字段
        None, // ParagraphStyle 目前没有 script 字段
        false, // ParagraphStyle 目前没有 underline 字段
        style.font_family.clone(), // 此前扩展签名时漏传，本次补上，非新增字段
        style.font_family_ascii.clone(), // 同上
    );
    let rpr_xml = if rpr.is_empty() {
        String::new()
    } else {
        format!("<w:rPr>{rpr}</w:rPr>")
    };

    let mut ppr = String::new();
    let jc_val = match style.alignment {
        Some(roe_model::style::Alignment::Left) => Some("left"),
        Some(roe_model::style::Alignment::Center) => Some("center"),
        Some(roe_model::style::Alignment::Right) => Some("right"),
        Some(roe_model::style::Alignment::Justify) => Some("both"),
        None => None,
    };
    if let Some(val) = jc_val {
        ppr.push_str(&format!(r#"<w:jc w:val="{val}"/>"#));
    }

    // w:ind：与 write_paragraph_xml 里 Paragraph.indent_left/indent_first
    // 的换算规则完全一致（pt*20=twips；indent_first 符号决定 w:firstLine
    // 还是 w:hanging，两者互斥）。
    let mut ind_attrs = String::new();
    if let Some(left) = style.indent_left {
        let twips = (left as f64 * 20.0).round() as i64;
        ind_attrs.push_str(&format!(r#" w:left="{twips}""#));
    }
    if let Some(first) = style.indent_first {
        if first >= 0.0 {
            let twips = (first as f64 * 20.0).round() as i64;
            ind_attrs.push_str(&format!(r#" w:firstLine="{twips}""#));
        } else {
            let twips = ((-first) as f64 * 20.0).round() as i64;
            ind_attrs.push_str(&format!(r#" w:hanging="{twips}""#));
        }
    }
    if !ind_attrs.is_empty() {
        ppr.push_str(&format!(r#"<w:ind{ind_attrs}/>"#));
    }

    // w:spacing：与 write_paragraph_xml 里 Paragraph.space_before/
    // space_after/line_height 的换算规则完全一致（pt*20=twips；
    // line_height 是倍数，*240还原成 w:line 原始值）。
    let mut spacing_attrs = String::new();
    if let Some(before) = style.space_before {
        let twips = (before as f64 * 20.0).round() as i64;
        spacing_attrs.push_str(&format!(r#" w:before="{twips}""#));
    }
    if let Some(after) = style.space_after {
        let twips = (after as f64 * 20.0).round() as i64;
        spacing_attrs.push_str(&format!(r#" w:after="{twips}""#));
    }
    if let Some(lh) = style.line_height {
        let val = (lh as f64 * 240.0).round() as i64;
        spacing_attrs.push_str(&format!(r#" w:line="{val}""#));
    }
    if !spacing_attrs.is_empty() {
        ppr.push_str(&format!(r#"<w:spacing{spacing_attrs}/>"#));
    }

    let ppr_xml = if ppr.is_empty() {
        String::new()
    } else {
        format!("<w:pPr>{ppr}</w:pPr>")
    };

    let based_on_xml = match &style.based_on {
        Some(base) => format!(r#"<w:basedOn w:val="{base}"/>"#),
        None => String::new(),
    };

    // w:styleId 直接用 style.name 本身作为 ID（与 style 定义/引用两侧
    // 保持同一个字符串，避免额外维护一张 name -> styleId 映射表）。
    format!(
        r#"<w:style w:type="paragraph" w:styleId="{name}"><w:name w:val="{name}"/>{based_on_xml}{ppr_xml}{rpr_xml}</w:style>"#,
        name = style.name,
    )
}

// ─── word/document.xml ─────────────────────────────────────────────────────────

fn write_document_xml_with_media(
    doc: &WriterDocument,
    media_counter: &mut u32,
    image_rels: &mut Vec<(String, String)>,
    images: &mut Vec<(String, Vec<u8>)>,
) -> String {
    let mut body_xml = String::new();
    let mut rid_counter: u32 = 1;
    // Section 路标出现的顺序 -> doc.sections 数组下标，两者严格一一对应
    // （见上方 BodyElement::Section 分支注释）。
    let mut section_idx: usize = 0;

    for elem in &doc.body {
        match elem {
            BodyElement::Paragraph(p) => {
                body_xml.push_str(&write_paragraph_xml(p));
            }
            BodyElement::Table(t) => {
                body_xml.push_str(&write_table_xml(t));
            }
            BodyElement::Image(img) => {
                let ext = mime_to_ext(&img.image.mime);
                let filename = format!("image{}.{}", *media_counter, ext);
                *media_counter += 1;
                let rid = format!("rId{rid_counter}");
                rid_counter += 1;

                images.push((format!("word/media/{filename}"), img.image.data.clone()));
                image_rels.push((rid.clone(), filename));

                // 内联图片在 OOXML body 里外层仍需包一个 <w:p><w:r>...
                // </w:r></w:p>（drawing 本身不是合法的独立 block-level
                // 内容），与 docx.rs parser 端"inline drawing 出现在
                // w:r 内部"的解析路径对称（parser 是在 run 内部遇到
                // wp:inline 分支）。本批次不追求还原图片原本所在的
                // 具体段落上下文（BodyElement::Image 本身就是独立
                // body 元素，不含段落格式信息），每张图片各自包一个
                // 最简单的空样式段落。
                body_xml.push_str(&write_image_drawing_xml(img, &rid));
            }
            BodyElement::Section(_) => {
                // Section 路标：docx.rs 解析端用"body 里出现第 N 个
                // Section 路标，就查 doc.sections[N] 取该节属性"的索引
                // 对应关系（见 ROE_Progress_v167 §"接口设计的关键约束"），
                // Section.name 只是调试用占位字符串（"Section 1".."），
                // Section.content 恒为空（模型层注释已声明，parser从不
                // 填充），两者都不对应任何 OOXML 元素，写出时忽略。
                //
                // 每个路标对应"这一节结束"，本批次统一采用"插入一个只含
                // sectPr、无实际内容的哑段落"来承载分节信息（不区分路标
                // 前面是 Paragraph 还是 Table，一律走这条路径，简化实现；
                // 用户确认这是 OOXML 里表格后紧跟 sectPr 的标准写法，
                // 空段落是合法且常见的做法，不是权宜之计）。
                let section = doc.sections.get(section_idx).cloned().unwrap_or_default();
                body_xml.push_str(&write_section_paragraph_xml(&section));
                section_idx += 1;
            }
            BodyElement::Frame(frame) => {
                let ext = mime_to_ext(&frame.image.mime);
                let filename = format!("image{}.{}", *media_counter, ext);
                *media_counter += 1;
                let rid = format!("rId{rid_counter}");
                rid_counter += 1;

                images.push((format!("word/media/{filename}"), frame.image.data.clone()));
                image_rels.push((rid.clone(), filename));

                // 与 Image 分支一致：drawing 需要外包一层 <w:p><w:r>...
                // </w:r></w:p>，图文框浮动定位信息全部写在 wp:anchor 内部，
                // 不依赖外层这个壳段落的任何格式。origin_para_body_index
                // 是 parser 内部用于回填锚点段落的辅助字段，不对应任何
                // OOXML 元素，写出时忽略（与 Section.name/content 同类）。
                body_xml.push_str(&write_frame_drawing_xml(frame, &rid));
            }
            BodyElement::Shape(shape) => {
                // Shape 写出（2026-09-16会话新增，摸底见 ROE_Progress_v170.md）。
                body_xml.push_str(&write_shape_xml(shape));
            }
            BodyElement::InlineChart { model, width_pt, height_pt } => {
                let xml = crate::ooxml::chart_writer::chart_to_xml(model);
                body_xml.push_str(&v318g_chart_drawing(xml, *width_pt, *height_pt, false));
            }
            BodyElement::InlineStatChart { model, width_pt, height_pt } => {
                let xml = crate::ooxml::chartex_writer::chartex_to_xml(model);
                body_xml.push_str(&v318g_chart_drawing(xml, *width_pt, *height_pt, true));
            }
            // InlineChart：parser 不读，writer 不写（范围内有意跳过，
            // 留给后续批次；Table/Image/Section/Frame/Shape 已支持）
            _ => {}
        }
    }

    // 文档级"最终 sectPr"：docx 规范要求 w:body 末尾必须有一个不属于任何
    // 段落的 sectPr（代表最后一节，即使全文档只有一节/未显式分节也要有）。
    // 取 doc.sections 最后一个元素的属性；若 sections 为空（未分节文档），
    // 则用 WriterDocument 顶层字段兜底（与 Section 字段"None 表示沿用
    // 顶层值"的既有语义一致）。
    let final_section_pr = write_final_section_pr_xml(doc);

    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:pic="http://schemas.openxmlformats.org/drawingml/2006/picture" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:wps="http://schemas.microsoft.com/office/word/2010/wordprocessingShape" xmlns:m="http://schemas.openxmlformats.org/officeDocument/2006/math">
<w:body>
{body_xml}{final_section_pr}</w:body>
</w:document>"#
    )
}

/// wp:inline 环绕间距四方向：pt -> EMU（×12700），与 docx.rs parser 端
/// "EMU/12700 换算为 pt" 的换算方向对称（见 roe-model/src/writer.rs
/// InlineImage.dist_t_pt 字段注释）。
thread_local! {
    // v318g: (部件 xml, 占位, 是否统计图)；顺序即编号
    static V318G_CHARTS: std::cell::RefCell<Vec<(String, String, bool)>> = std::cell::RefCell::new(Vec::new());
}

/// v318g: 登记图表部件并返回 body 里的 inline drawing（外包 w:p/w:r）
fn v318g_chart_drawing(part_xml: String, width_pt: f32, height_pt: f32, is_stat: bool) -> String {
    let n = V318G_CHARTS.with(|c| {
        let mut c = c.borrow_mut();
        c.push((part_xml, String::new(), is_stat));
        c.len()
    });
    let cx = (width_pt as f64 * 12700.0).round() as i64;
    let cy = (height_pt as f64 * 12700.0).round() as i64;
    let rid = format!("rIdChart{n}");
    let inner = if is_stat {
        format!(r#"<a:graphicData uri="http://schemas.microsoft.com/office/drawing/2014/chartex"><cx:chart xmlns:cx="http://schemas.microsoft.com/office/drawing/2014/chartex" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" r:id="{rid}"/></a:graphicData>"#)
    } else {
        format!(r#"<a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/chart"><c:chart xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" r:id="{rid}"/></a:graphicData>"#)
    };
    format!(
        r#"<w:p><w:r><w:drawing><wp:inline distT="0" distB="0" distL="0" distR="0"><wp:extent cx="{cx}" cy="{cy}"/><wp:docPr id="{n}" name="Chart {n}"/><a:graphic>{inner}</a:graphic></wp:inline></w:drawing></w:r></w:p>
"#
    )
}

fn write_image_drawing_xml(img: &roe_model::writer::InlineImage, rid: &str) -> String {
    let cx = (img.width_pt as f64 * 12700.0).round() as i64;
    let cy = (img.height_pt as f64 * 12700.0).round() as i64;
    let img_rot_attr = match img.rotation {
        Some(d) if d.abs() > 0.01 => format!(r#" rot="{}""#, ((d as f64).rem_euclid(360.0) * 60000.0).round() as i64),
        _ => String::new(),
    };
    let dist_t = (img.dist_t_pt as f64 * 12700.0).round() as i64;
    let dist_b = (img.dist_b_pt as f64 * 12700.0).round() as i64;
    let dist_l = (img.dist_l_pt as f64 * 12700.0).round() as i64;
    let dist_r = (img.dist_r_pt as f64 * 12700.0).round() as i64;

    format!(
        r#"<w:p><w:r><w:drawing><wp:inline distT="{dist_t}" distB="{dist_b}" distL="{dist_l}" distR="{dist_r}">
<wp:extent cx="{cx}" cy="{cy}"/>
<wp:docPr id="1" name="Picture"/>
<a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/picture">
<pic:pic>
<pic:nvPicPr><pic:cNvPr id="1" name="Picture"/><pic:cNvPicPr/></pic:nvPicPr>
<pic:blipFill><a:blip r:embed="{rid}"/><a:stretch><a:fillRect/></a:stretch></pic:blipFill>
<pic:spPr><a:xfrm{img_rot_attr}><a:off x="0" y="0"/><a:ext cx="{cx}" cy="{cy}"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></pic:spPr>
</pic:pic>
</a:graphicData></a:graphic>
</wp:inline></w:drawing></w:r></w:p>
"#
    )
}

/// FloatFrame（图片浮动环绕）写出，对应 OOXML wp:anchor（区别于
/// write_image_drawing_xml 用的 wp:inline，浮动图片不是简单内嵌在
/// 文字流中，需要携带锚定/环绕语义）。
///
/// 范围（2026-09-16会话，Writer接口开发设计文档 v1 1.1节"图文框环绕能力"
/// 的存盘落地批次）：width_pt/height_pt/x_pt/y_pt/anchor_type_h/v/
/// align_h/v/wrap_type/dist_t-b-l-r_pt/rotation，全部经docx.rs解析端
/// 正向+反向测试确认支持（frames[0].rotation 有专门的Some/None两个
/// 测试，其余字段见docx.rs 7190-7380行附近fixture），故本函数全部
/// 字段均可用write→reparse验证，不需要"直接断言XML"这条路径。
///
/// 不写出：origin_para_body_index（parser内部回填锚点段落用的辅助
/// 字段，不对应任何OOXML元素，与Section.name/content同类，本函数
/// 忽略此字段）。
fn write_frame_drawing_xml(frame: &roe_model::writer::FloatFrame, rid: &str) -> String {
    let cx = (frame.width_pt as f64 * 12700.0).round() as i64;
    let cy = (frame.height_pt as f64 * 12700.0).round() as i64;
    let dist_t = (frame.dist_t_pt as f64 * 12700.0).round() as i64;
    let dist_b = (frame.dist_b_pt as f64 * 12700.0).round() as i64;
    let dist_l = (frame.dist_l_pt as f64 * 12700.0).round() as i64;
    let dist_r = (frame.dist_r_pt as f64 * 12700.0).round() as i64;

    let position_h_xml = write_frame_position_xml(
        "wp:positionH",
        frame.anchor_type_h.as_deref(),
        frame.align_h.as_deref(),
        frame.x_pt,
    );
    let position_v_xml = write_frame_position_xml(
        "wp:positionV",
        frame.anchor_type_v.as_deref(),
        frame.align_v.as_deref(),
        frame.y_pt,
    );
    let wrap_xml = write_frame_wrap_xml(frame.wrap_type.as_deref());

    // rotation：与 write_image_drawing_xml 的 a:xfrm 结构一致，仅多一个
    // rot 属性。docx.rs 解析端换算规则是 raw/60000.0（2738行），这里做
    // 逆运算 *60000，四舍五入取整。None 时不写 rot 属性（等同不旋转，
    // 与解析端"无rot属性时应为None"的验证测试对称）。
    let rot_attr = match frame.rotation {
        Some(deg) => format!(r#" rot="{}""#, (deg as f64 * 60000.0).round() as i64),
        None => String::new(),
    };

    format!(
        r#"<w:p><w:r><w:drawing><wp:anchor distT="{dist_t}" distB="{dist_b}" distL="{dist_l}" distR="{dist_r}" simplePos="0" relativeHeight="1" behindDoc="0" locked="0" layoutInCell="1" allowOverlap="1">
{position_h_xml}
{position_v_xml}
<wp:extent cx="{cx}" cy="{cy}"/>
{wrap_xml}
<wp:docPr id="1" name="Picture"/>
<a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/picture">
<pic:pic>
<pic:nvPicPr><pic:cNvPr id="1" name="Picture"/><pic:cNvPicPr/></pic:nvPicPr>
<pic:blipFill><a:blip r:embed="{rid}"/><a:stretch><a:fillRect/></a:stretch></pic:blipFill>
<pic:spPr><a:xfrm{rot_attr}><a:off x="0" y="0"/><a:ext cx="{cx}" cy="{cy}"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></pic:spPr>
</pic:pic>
</a:graphicData></a:graphic>
</wp:anchor></w:drawing></w:r></w:p>
"#
    )
}

/// wp:positionH / wp:positionV 公共写法：align 优先于 posOffset（与模型
/// 字段注释"align_h/v 有值时应优先于 x_pt/y_pt 使用"一致）。relativeFrom
/// 缺省时用 docx.rs 解析端 fixture 里最常见的默认值兜底（H="column"，
/// V="paragraph"，与真实样本 roe-parser docx.rs 7314/7315行一致），
/// 避免写出 relativeFrom 缺失的不完整标签。
fn write_frame_position_xml(tag: &str, relative_from: Option<&str>, align: Option<&str>, offset_pt: f32) -> String {
    let default_relative_from = if tag == "wp:positionH" { "column" } else { "paragraph" };
    let relative_from = relative_from.unwrap_or(default_relative_from);
    match align {
        Some(a) => format!(r#"<{tag} relativeFrom="{relative_from}"><wp:align>{a}</wp:align></{tag}>"#),
        None => {
            let offset_emu = (offset_pt as f64 * 12700.0).round() as i64;
            format!(r#"<{tag} relativeFrom="{relative_from}"><wp:posOffset>{offset_emu}</wp:posOffset></{tag}>"#)
        }
    }
}

/// wp:wrap* 系列标签互斥五选一（docx.rs 3719行注释：均为 wp:anchor
/// 直接子元素、互斥五选一）。None 或未识别字符串时不写任何 wrap 标签
/// （docx.rs 对应反向测试：无 wrap 标签时 wrap_type 应为 None，这里
/// 对称处理）。
fn write_frame_wrap_xml(wrap_type: Option<&str>) -> String {
    match wrap_type {
        Some("none") => "<wp:wrapNone/>".to_string(),
        Some("square") => r#"<wp:wrapSquare wrapText="bothSides"/>"#.to_string(),
        Some("tight") => r#"<wp:wrapTight wrapText="bothSides"/>"#.to_string(),
        Some("through") => r#"<wp:wrapThrough wrapText="bothSides"/>"#.to_string(),
        Some("topAndBottom") => "<wp:wrapTopAndBottom/>".to_string(),
        _ => String::new(),
    }
}

/// Shape（自选图形/文本框）写出，对应 OOXML wps:wsp（DrawingML）。
/// 参照 write_frame_drawing_xml 的写法模式：外壳按 inline/anchor 二选一，
/// 内部固定嵌套 a:graphic > a:graphicData > wps:wsp > wps:spPr 结构，
/// 与 wps_document 测试 fixture（roe-parser/src/ooxml/docx.rs）的真实
/// 层级一致。
///
/// 本轮范围收窄（Writer接口开发设计文档v1 1.1节"图文框环绕能力"存盘
/// 落地批次，Shape部分，详见 ROE_Progress_v170.md 三、3.1节拍板方案）：
/// - geometry: Rect/Ellipse/Line 完整支持；PresetPath 按 preset_name
///   原样写回 prst；Custom 按内容原样写 prst；RoundRect/Triangle 退化
///   为对应 prst，不写 avLst/adj 精细参数（能写出合法XML但不追求adj
///   精度，不是遗漏）。
/// - fill: 只支持 Solid(Color)，其余（None/Gradient/Picture/
///   PictureTiled）不写 a:solidFill 标签，等同无填充。
/// - stroke: StrokeStyle{color, width} 直接支持。
/// - 位置/环绕: 复用 write_frame_position_xml / write_frame_wrap_xml。
/// - 文本框内容: 直接复用 write_paragraph_xml 逐段落写出（按
///   parse_isolated_paragraphs 真实支持的字段范围来，不写 background/
///   script/underline_style/font_size——但这是 write_paragraph_xml
///   自身职责范围，此处不重复处理）。
/// - flip_v: Some(true) 时 a:xfrm 加 flipV="1"，None/Some(false) 不写。
/// - shadow/head_arrow/tail_arrow: 本轮不写，降级跳过。
///
/// inline vs anchor 判断规则（与 docx.rs 两处真实构造代码完全对称）：
/// anchor_type_h/anchor_type_v/align_h/align_v/wrap_type 只要有一个不是
/// None，就判定为 anchor 场景走 wp:anchor；全部为 None 才判定 inline
/// 场景走 wp:inline。
fn write_shape_xml(shape: &roe_model::writer::Shape) -> String {
    use roe_model::common::{FillStyle, ShapeGeometry};

    let cx = (shape.width_pt as f64 * 12700.0).round() as i64;
    let cy = (shape.height_pt as f64 * 12700.0).round() as i64;

    // geometry -> prst 映射
    let prst = match &shape.geometry {
        ShapeGeometry::Rect => "rect".to_string(),
        ShapeGeometry::RoundRect { .. } => "roundRect".to_string(),
        ShapeGeometry::Ellipse => "ellipse".to_string(),
        ShapeGeometry::Triangle => "triangle".to_string(),
        ShapeGeometry::Line => "line".to_string(),
        ShapeGeometry::PresetPath(data) => {
            if data.preset_name.is_empty() {
                "rect".to_string()
            } else {
                data.preset_name.clone()
            }
        }
        ShapeGeometry::Custom(s) => {
            if s.is_empty() { "rect".to_string() } else { s.clone() }
        }
    };
    let prst_attr = xml_escape_attr(&prst);

    // fill：仅 Solid(Color) 支持，其余不写标签
    let fill_xml = match &shape.style.fill {
        Some(FillStyle::Gradient { stops, kind: roe_model::common::GradientKind::Linear { angle }, .. }) if stops.len() >= 2 => {
            let mut gs = String::new();
            for s in stops.iter() {
                gs.push_str(&format!(
                    r#"<a:gs pos="{}"><a:srgbClr val="{:02X}{:02X}{:02X}"/></a:gs>"#,
                    (s.pos.clamp(0.0, 1.0) as f64 * 100000.0).round() as i64,
                    s.color.r, s.color.g, s.color.b
                ));
            }
            format!(
                r#"<a:gradFill rotWithShape="1"><a:gsLst>{}</a:gsLst><a:lin ang="{}" scaled="0"/></a:gradFill>"#,
                gs,
                ((*angle as f64).rem_euclid(360.0) * 60000.0).round() as i64
            )
        }
        Some(FillStyle::Solid(color)) => {
            format!(
                r#"<a:solidFill><a:srgbClr val="{:02X}{:02X}{:02X}"/></a:solidFill>"#,
                color.r, color.g, color.b
            )
        }
        _ => String::new(),
    };

    // stroke：StrokeStyle 直接支持
    let stroke_xml = match &shape.style.stroke {
        Some(stroke) => {
            let width_emu = (stroke.width as f64 * 12700.0).round() as i64;
            format!(
                r#"<a:ln w="{}"><a:solidFill><a:srgbClr val="{:02X}{:02X}{:02X}"/></a:solidFill></a:ln>"#,
                width_emu, stroke.color.r, stroke.color.g, stroke.color.b
            )
        }
        None => String::new(),
    };

    // flip_v
    let rot_attr = match shape.rotation {
        Some(d) if d.abs() > 0.01 => format!(r#" rot="{}""#, ((d as f64).rem_euclid(360.0) * 60000.0).round() as i64),
        _ => String::new(),
    };
    let flip_attr = match shape.flip_v {
        Some(true) => r#" flipV="1""#,
        _ => "",
    };

    // 文本框内容：非空时才写 wps:txbx，逐段落复用 write_paragraph_xml
    let txbx_xml = if shape.text_content.is_empty() {
        String::new()
    } else {
        let mut paras_xml = String::new();
        for p in &shape.text_content {
            paras_xml.push_str(&write_paragraph_xml(p));
        }
        format!(r#"<wps:txbx><w:txbxContent>{paras_xml}</w:txbxContent></wps:txbx>"#)
    };

    let sp_pr = format!(
        r#"<wps:spPr><a:xfrm{rot_attr}{flip_attr}><a:off x="0" y="0"/><a:ext cx="{cx}" cy="{cy}"/></a:xfrm><a:prstGeom prst="{prst_attr}"><a:avLst/></a:prstGeom>{fill_xml}{stroke_xml}</wps:spPr>"#
    );

    let wps_wsp = format!("<wps:wsp>{sp_pr}{txbx_xml}</wps:wsp>");

    let graphic_xml = format!(
        r#"<a:graphic><a:graphicData uri="http://schemas.microsoft.com/office/word/2010/wordprocessingShape">{wps_wsp}</a:graphicData></a:graphic>"#
    );

    // inline vs anchor 判断
    let is_anchor = shape.anchor_type_h.is_some()
        || shape.anchor_type_v.is_some()
        || shape.align_h.is_some()
        || shape.align_v.is_some()
        || shape.wrap_type.is_some();

    if is_anchor {
        let dist_t = (shape.dist_t_pt as f64 * 12700.0).round() as i64;
        let dist_b = (shape.dist_b_pt as f64 * 12700.0).round() as i64;
        let dist_l = (shape.dist_l_pt as f64 * 12700.0).round() as i64;
        let dist_r = (shape.dist_r_pt as f64 * 12700.0).round() as i64;

        let position_h_xml = write_frame_position_xml(
            "wp:positionH",
            shape.anchor_type_h.as_deref(),
            shape.align_h.as_deref(),
            shape.x_pt,
        );
        let position_v_xml = write_frame_position_xml(
            "wp:positionV",
            shape.anchor_type_v.as_deref(),
            shape.align_v.as_deref(),
            shape.y_pt,
        );
        let wrap_xml = write_frame_wrap_xml(shape.wrap_type.as_deref());

        format!(
            r#"<w:p><w:r><w:drawing><wp:anchor distT="{dist_t}" distB="{dist_b}" distL="{dist_l}" distR="{dist_r}" simplePos="0" relativeHeight="1" behindDoc="0" locked="0" layoutInCell="1" allowOverlap="1">
{position_h_xml}
{position_v_xml}
<wp:extent cx="{cx}" cy="{cy}"/>
{wrap_xml}
<wp:docPr id="1" name="Shape"/>
{graphic_xml}
</wp:anchor></w:drawing></w:r></w:p>
"#
        )
    } else {
        format!(
            r#"<w:p><w:r><w:drawing><wp:inline distT="0" distB="0" distL="0" distR="0">
<wp:extent cx="{cx}" cy="{cy}"/>
<wp:docPr id="1" name="Shape"/>
{graphic_xml}
</wp:inline></w:drawing></w:r></w:p>
"#
        )
    }
}

/// mime -> 文件扩展名，与 pptx_writer.rs 的 mime_to_ext 同名同逻辑，
/// 两份文件各自独立维护一份（未提取公共模块，与本文件与 pptx_writer.rs
/// "对称但不共享代码"的既有关系一致，例如两边各自都有自己的
/// xml_escape_text）。
fn mime_to_ext(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpeg",
        "image/gif" => "gif",
        _ => "bin",
    }
}

// ─── Section (分节) 写出 ────────────────────────────────────────────────────
//
// 范围（本批次）：page_size / margin_top-bottom-left-right / columns
// （仅 num_cols，不含 col_widths/col_spacing 精细分栏）/ title_page /
// section_type。与 ROE_Progress_v167 "接口层范围收窄"的既有取舍一致。
//
// 本批次不写（留给后续批次）：
// - header/footer 引用（w:headerReference/w:footerReference）：需要
//   额外生成 word/headerN.xml、word/footerN.xml 独立 part + 对应
//   document.xml.rels 关系条目 + [Content_Types].xml Override 声明，
//   工作量独立于"sectPr 基础字段写出"，值得单独一批处理。
// - even_and_odd_headers：docx 规范里这是 settings.xml 全局开关，不是
//   逐 sectPr 属性，本批次不生成 settings.xml，故此字段本批次不涉及。
// - columns 的 col_widths/col_spacing 精细宽度（DocumentColumns 里
//   equalWidth=0 场景）：本批次仅支持 num_cols 单一整数，与
//   set_section_columns 接口现有范围对齐。

fn write_section_paragraph_xml(section: &roe_model::writer::Section) -> String {
    let sect_pr_inner = write_section_pr_xml(section);
    format!("<w:p><w:pPr><w:sectPr>{sect_pr_inner}</w:sectPr></w:pPr></w:p>\n")
}

fn write_final_section_pr_xml(doc: &WriterDocument) -> String {
    let inner = match doc.sections.last() {
        Some(section) => write_section_pr_xml(section),
        None => {
            // 未分节文档：用 WriterDocument 顶层字段合成一个临时 Section，
            // 复用同一份写出逻辑，避免维护两套字段映射代码。
            let synthetic = roe_model::writer::Section {
                name: String::new(),
                content: Vec::new(),
                page_size: Some(doc.page_size),
                margin_top: Some(doc.margin_top),
                margin_bottom: Some(doc.margin_bottom),
                margin_left: Some(doc.margin_left),
                margin_right: Some(doc.margin_right),
                header: None,
                footer: None,
                title_page: Some(doc.title_page),
                even_and_odd_headers: Some(doc.even_and_odd_headers),
                section_type: None,
                columns: Some(doc.columns.clone()),
            };
            write_section_pr_xml(&synthetic)
        }
    };
    format!("<w:sectPr>{inner}</w:sectPr>")
}

/// 生成 <w:sectPr>...</w:sectPr> 内部字段。None 字段一律不写对应标签
/// （docx.rs 解析端对应字段保持 None/沿用顶层默认值，两者语义对称）。
fn write_section_pr_xml(section: &roe_model::writer::Section) -> String {
    let mut inner = String::new();

    if let Some(title_page) = section.title_page {
        if title_page {
            inner.push_str("<w:titlePg/>");
        }
    }

    if let Some(section_type) = &section.section_type {
        inner.push_str(&format!(
            r#"<w:type w:val="{}"/>"#,
            xml_escape_attr(section_type)
        ));
    }

    if let Some((w, h)) = section.page_size {
        let w_twips = (w as f64 * 20.0).round() as i64;
        let h_twips = (h as f64 * 20.0).round() as i64;
        inner.push_str(&format!(r#"<w:pgSz w:w="{w_twips}" w:h="{h_twips}"/>"#));
    }

    let has_margin = section.margin_top.is_some()
        || section.margin_bottom.is_some()
        || section.margin_left.is_some()
        || section.margin_right.is_some();
    if has_margin {
        // docx.rs 解析端对四边各自独立赋值（互不依赖），写出时对缺失的
        // 某一边省略对应属性，而非补0，避免"未设置"被误写成"显式设为0"。
        let mut attrs = String::new();
        if let Some(v) = section.margin_top {
            attrs.push_str(&format!(r#" w:top="{}""#, (v as f64 * 20.0).round() as i64));
        }
        if let Some(v) = section.margin_bottom {
            attrs.push_str(&format!(r#" w:bottom="{}""#, (v as f64 * 20.0).round() as i64));
        }
        if let Some(v) = section.margin_left {
            attrs.push_str(&format!(r#" w:left="{}""#, (v as f64 * 20.0).round() as i64));
        }
        if let Some(v) = section.margin_right {
            attrs.push_str(&format!(r#" w:right="{}""#, (v as f64 * 20.0).round() as i64));
        }
        inner.push_str(&format!("<w:pgMar{attrs}/>"));
    }

    if let Some(cols) = &section.columns {
        if cols.num_cols > 1 {
            inner.push_str(&format!(r#"<w:cols w:num="{}"/>"#, cols.num_cols));
        }
        // num_cols<=1：单栏是 OOXML 默认值，不写 w:cols 标签，与
        // write_tblpr_xml 等本文件其余"默认值不显式写出"的一贯风格一致。
    }

    inner
}

// ─── Table 写出 ─────────────────────────────────────────────────────────────
//
// 范围（本批次，docx_writer 扩展第一批，对应 ROE_Progress_v167 第4.1节
// "Table基础结构写出跑通"目标）：
// - w:tbl / w:tblGrid（列宽）/ w:tr（行高）/ w:tc（gridSpan 合并、cell 内
//   Paragraph 内容）
// - 表格样式绑定：style_id → w:tblStyle；first_row/first_col/last_row/
//   last_col/band_row/band_col 六个 bool → w:tblLook 对应位
// - 表格级边框：default_border_*/inside_h_*/inside_v_* → w:tblBorders
// - cell 自身：bg_color（w:shd）/ border_color+border_width（w:tcBorders）
//
// 本批次不写（留给后续批次，与 ROE_Progress_v167 §2.4 分批推进原则一致）：
// - Table.float_pos（表格浮动定位，属于图文框环绕范畴，与 FloatFrame/Shape
//   的 wrap_type 一起处理更合适，单独一批）
// - TableCell 的 pad_left/right/top/bottom（内边距，非本轮 Writer 接口
//   1.1节列出的重点字段，先跳过降低复杂度）
// - TableCell.row_span：DOCX 的合并语义是"物理收窄"（被合并的 cell 在
//   XML 里根本不存在，见 docx.rs w:vMerge 解析注释），要正确写出
//   row_span>1 需要同时处理 w:vMerge="restart"/延续行"跳过对应列"的
//   协调逻辑，属于"单元格合并/拆分"范畴（Writer设计文档3节7.7条已定性
//   为模型层设计缺口，本批次不涉及）。本批次对 row_span>1 的 cell
//   仅写出 col_span（gridSpan），不写 vMerge，即退化为"每行独立不跨行"
//   的效果，是已知的范围内取舍。
// - CellContent::Image / CellContent::Table（cell 内嵌图片/嵌套表格）：
//   仅处理 CellContent::Paragraph，其余变体静默跳过，覆盖绝大多数真实
//   表格场景（纯文字单元格），嵌套场景留给后续批次。

fn write_table_xml(t: &roe_model::common::Table) -> String {
    let tblpr_xml = write_tblpr_xml(t);

    let mut grid_xml = String::new();
    if !t.col_widths.is_empty() {
        grid_xml.push_str("<w:tblGrid>");
        for w in &t.col_widths {
            // pt -> twips (1pt = 20 twips)，与 docx.rs 解析方向的逆运算
            let twips = (*w as f64 * 20.0).round() as i64;
            grid_xml.push_str(&format!(r#"<w:gridCol w:w="{twips}"/>"#));
        }
        grid_xml.push_str("</w:tblGrid>");
    }

    let mut rows_xml = String::new();
    for row in &t.rows {
        rows_xml.push_str(&write_table_row_xml(row, &t.col_widths));
    }

    format!("<w:tbl>{tblpr_xml}{grid_xml}{rows_xml}</w:tbl>\n")
}

fn write_tblpr_xml(t: &roe_model::common::Table) -> String {
    let mut inner = String::new();

    if let Some(style_id) = &t.style_id {
        inner.push_str(&format!(
            r#"<w:tblStyle w:val="{}"/>"#,
            xml_escape_attr(style_id)
        ));
    }

    if !t.col_widths.is_empty() {
        let total_tw: i64 = t.col_widths.iter().map(|w| (*w as f64 * 20.0).round() as i64).sum();
        if total_tw > 0 {
            inner.push_str(&format!(r#"<w:tblW w:w="{total_tw}" w:type="dxa"/>"#));
        }
    }
    inner.push_str(&write_tblborders_xml(t));

    // w:tblLook：firstRow/firstColumn/lastRow/lastColumn/noHBand/noVBand
    // 各占一位。noHBand/noVBand 与 band_row/band_col 语义相反（"no band"
    // = 不要斑马纹），故取反。val 的十六进制掩码写法是 OOXML 惯例
    // （0x0020=firstRow, 0x0040=lastRow, 0x0080=firstCol, 0x0100=lastCol,
    // 0x0200=noHBand, 0x0400=noVBand），但更常见、更保险的做法是直接写
    // 各个独立布尔属性，避免掩码计算出错；Word 对两种写法都能识别，
    // 这里选独立属性写法，可读性更好、不易写错。
    inner.push_str(&format!(
        r#"<w:tblLook w:firstRow="{}" w:firstColumn="{}" w:lastRow="{}" w:lastColumn="{}" w:noHBand="{}" w:noVBand="{}"/>"#,
        bool01(t.first_row),
        bool01(t.first_col),
        bool01(t.last_row),
        bool01(t.last_col),
        bool01(!t.band_row),
        bool01(!t.band_col),
    ));

    if inner.is_empty() {
        String::new()
    } else {
        format!("<w:tblPr>{inner}</w:tblPr>")
    }
}

fn write_tblborders_xml(t: &roe_model::common::Table) -> String {
    let mut edges = String::new();

    // 四边使用 default_border_color/width；docx.rs 解析时四边只要有一边
    // 有值就记进 default_border_*（see cur_tbl_border_color 取第一次命中
    // 值的逻辑），故写出时对称地把 default_border_* 同时写给 top/left/
    // bottom/right 四边，与解析方向对称。
    if t.default_border_color.is_some() || t.default_border_width.is_some() {
        for edge in ["top", "left", "bottom", "right"] {
            edges.push_str(&format!(
                r#"<w:{edge} w:val="single" w:sz="{}" w:space="0"{}/>"#,
                border_width_to_sz(t.default_border_width),
                xml_color_attr(t.default_border_color),
            ));
        }
    }

    if t.inside_h_color.is_some() || t.inside_h_width.is_some() {
        edges.push_str(&format!(
            r#"<w:insideH w:val="single" w:sz="{}" w:space="0"{}/>"#,
            border_width_to_sz(t.inside_h_width),
            xml_color_attr(t.inside_h_color),
        ));
    }
    if t.inside_v_color.is_some() || t.inside_v_width.is_some() {
        edges.push_str(&format!(
            r#"<w:insideV w:val="single" w:sz="{}" w:space="0"{}/>"#,
            border_width_to_sz(t.inside_v_width),
            xml_color_attr(t.inside_v_color),
        ));
    }

    if edges.is_empty() {
        String::new()
    } else {
        format!("<w:tblBorders>{edges}</w:tblBorders>")
    }
}

fn write_table_row_xml(row: &roe_model::common::TableRow, col_widths: &[f32]) -> String {
    let trpr_xml = match row.row_height {
        Some(h) if h > 0.0 => {
            let twips = (h as f64 * 20.0).round() as i64;
            format!(r#"<w:trPr><w:trHeight w:val="{twips}"/></w:trPr>"#)
        }
        _ => String::new(),
    };

    let mut cells_xml = String::new();
    let mut grid_idx = 0usize;
    for cell in &row.cells {
        // hidden=true 的 cell 是 PPTX 合并延续格的展示字段，DOCX 侧
        // 该字段恒为 false（见 TableCell.hidden 字段注释），此处按
        // "如果意外出现就跳过"处理，防御性写法，不依赖 DOCX 恒为
        // false 这个假设在所有历史/未来数据里都成立。
        if cell.hidden {
            continue;
        }
        let span = (cell.col_span as usize).max(1);
        let w_tw: Option<i64> = if col_widths.is_empty() { None } else {
            let end = (grid_idx + span).min(col_widths.len());
            let sum: f32 = if grid_idx < end { col_widths[grid_idx..end].iter().sum() } else { 0.0 };
            if sum > 0.0 { Some((sum as f64 * 20.0).round() as i64) } else { None }
        };
        grid_idx += span;
        cells_xml.push_str(&write_table_cell_xml_w(cell, w_tw));
    }

    format!("<w:tr>{trpr_xml}{cells_xml}</w:tr>")
}

#[allow(dead_code)]
fn write_table_cell_xml(cell: &roe_model::common::TableCell) -> String {
    write_table_cell_xml_w(cell, None)
}

fn write_table_cell_xml_w(cell: &roe_model::common::TableCell, width_tw: Option<i64>) -> String {
    let mut tcpr_inner = String::new();
    if let Some(w) = width_tw {
        tcpr_inner.push_str(&format!(r#"<w:tcW w:w="{w}" w:type="dxa"/>"#));
    }

    if cell.col_span > 1 {
        tcpr_inner.push_str(&format!(r#"<w:gridSpan w:val="{}"/>"#, cell.col_span));
    }

    if cell.bg_color.is_some() {
        tcpr_inner.push_str(&format!(
            r#"<w:shd w:val="clear" w:color="auto"{}/>"#,
            xml_fill_attr(cell.bg_color)
        ));
    }

    if cell.border_color.is_some() || cell.border_width.is_some() {
        let sz = border_width_to_sz(cell.border_width);
        let color_attr = xml_color_attr(cell.border_color);
        tcpr_inner.push_str("<w:tcBorders>");
        for edge in ["top", "left", "bottom", "right"] {
            tcpr_inner.push_str(&format!(
                r#"<w:{edge} w:val="single" w:sz="{sz}" w:space="0"{color_attr}/>"#
            ));
        }
        tcpr_inner.push_str("</w:tcBorders>");
    }

    let tcpr_xml = if tcpr_inner.is_empty() {
        String::new()
    } else {
        format!("<w:tcPr>{tcpr_inner}</w:tcPr>")
    };

    let mut content_xml = String::new();
    for c in &cell.content {
        if let roe_model::common::CellContent::Paragraph(p) = c {
            content_xml.push_str(&write_paragraph_xml(p));
        }
        // Image / Table（嵌套）：本批次不支持，静默跳过，见文件头本节
        // 范围说明。
    }
    // OOXML 要求 w:tc 至少含一个 w:p，空 cell 补一个空段落，避免生成
    // 不合法文档（Word 打开会报"文档已损坏"）。
    if content_xml.is_empty() {
        content_xml.push_str("<w:p/>");
    }

    format!("<w:tc>{tcpr_xml}{content_xml}</w:tc>")
}

fn bool01(b: bool) -> &'static str {
    if b { "1" } else { "0" }
}

/// pt -> OOXML w:sz（八分之一磅），<=0 或 None 时退回 4（0.5pt 细线），
/// 与前端 set_table_default_border_width 等既有接口"宽度<=0静默忽略/
/// 不写"的保守取向一致：这里选择给个最小可见细线而非完全不写 w:sz
/// （w:sz 是 OOXML 边框元素的必需属性之一，为空会不合法）。
fn border_width_to_sz(width_pt: Option<f32>) -> i64 {
    match width_pt {
        Some(w) if w > 0.0 => (w as f64 * 8.0).round() as i64,
        _ => 4,
    }
}

/// 生成 w:color 属性片段（含前导空格），None 时返回 "auto"，与
/// docx.rs 解析端"val != auto 才记录颜色"的语义对称。
fn xml_color_attr(color: Option<roe_model::common::Color>) -> String {
    match color {
        Some(c) => format!(r#" w:color="{:02X}{:02X}{:02X}""#, c.r, c.g, c.b),
        None => r#" w:color="auto""#.to_string(),
    }
}

/// 生成 w:fill 属性片段（含前导空格），用于 w:shd 背景色。
fn xml_fill_attr(color: Option<roe_model::common::Color>) -> String {
    match color {
        Some(c) => format!(r#" w:fill="{:02X}{:02X}{:02X}""#, c.r, c.g, c.b),
        None => r#" w:fill="auto""#.to_string(),
    }
}

fn write_paragraph_xml(p: &roe_model::writer::Paragraph) -> String {
    let mut runs_xml = String::new();
    for run in &p.runs {
        // v282: 批注范围起点（只写 doc.comments 里存在的 id）
        for id in &run.comment_range_start {
            if note_id_ok(2, *id) {
                runs_xml.push_str(&format!(r#"<w:commentRangeStart w:id="{}"/>"#, id));
            }
        }
        let is_note_field = matches!(
            run.field,
            Some(roe_model::writer::FieldKind::FootnoteRef) | Some(roe_model::writer::FieldKind::EndnoteRef)
        );
        if let Some(f) = &run.formula {
            // v224: 公式 run 的 text 为空，latex 转 m:oMath 写出
            runs_xml.push_str(&super::latex_omml::latex_to_omml(&f.latex));
        } else {
            if !run.text.is_empty() && !is_note_field {
                let rpr_xml = write_run_props_xml(run);
                let t_xml = run.text.split('\n')
                    .map(|seg| format!(r#"<w:t xml:space="preserve">{}</w:t>"#, xml_escape_text(seg)))
                    .collect::<Vec<_>>()
                    .join("<w:br/>");
                runs_xml.push_str(&format!("<w:r>{}{}</w:r>", rpr_xml, t_xml));
            }
            // v282: 脚注/尾注引用、注释内部编号占位符
            let sup = r#"<w:rPr><w:vertAlign w:val="superscript"/></w:rPr>"#;
            if let Some(id) = run.footnote_ref {
                if note_id_ok(0, id) {
                    runs_xml.push_str(&format!(r#"<w:r>{}<w:footnoteReference w:id="{}"/></w:r>"#, sup, id));
                }
            } else if let Some(id) = run.endnote_ref {
                if note_id_ok(1, id) {
                    runs_xml.push_str(&format!(r#"<w:r>{}<w:endnoteReference w:id="{}"/></w:r>"#, sup, id));
                }
            } else if run.field == Some(roe_model::writer::FieldKind::FootnoteRef) {
                runs_xml.push_str(&format!("<w:r>{}<w:footnoteRef/></w:r>", sup));
            } else if run.field == Some(roe_model::writer::FieldKind::EndnoteRef) {
                runs_xml.push_str(&format!("<w:r>{}<w:endnoteRef/></w:r>", sup));
            }
            // v282: 批注引用
            if let Some(id) = run.comment_ref {
                if note_id_ok(2, id) {
                    runs_xml.push_str(&format!(r#"<w:r><w:commentReference w:id="{}"/></w:r>"#, id));
                }
            }
        }
        // v282: 批注范围终点
        for id in &run.comment_range_end {
            if note_id_ok(2, *id) {
                runs_xml.push_str(&format!(r#"<w:commentRangeEnd w:id="{}"/>"#, id));
            }
        }
    }

    if p.column_break_after {
        runs_xml.push_str(r#"<w:r><w:br w:type="column"/></w:r>"#);
    }

    // 段落格式补全（2026-09-16会话，Writer接口开发设计文档v1 1.1节"段落格式
    // 补全"存盘落地）：alignment(w:jc)/indent_left+indent_first(w:ind)/
    // space_before+space_after+line_height(w:spacing)。docx.rs正文段落级
    // w:pPr解析对这五个字段均有完整支持（3679-3694行w:jc四档映射、
    // w:ind的w:left/w:firstLine/w:hanging三属性、w:spacing的w:before/
    // w:after/w:line三属性），故全部走write→reparse验证，不需要直接
    // 断言XML这条路径。
    //
    // w:jc映射（与docx.rs 3683-3688行反向对称，多值同义词场景取第一个，
    // 即Left写"left"而非"start"、Right写"right"而非"end"）：
    //   Alignment::Left    -> "left"
    //   Alignment::Center  -> "center"
    //   Alignment::Right   -> "right"
    //   Alignment::Justify -> "both"
    let jc_xml = match p.alignment {
        Some(roe_model::style::Alignment::Left) => r#"<w:jc w:val="left"/>"#.to_string(),
        Some(roe_model::style::Alignment::Center) => r#"<w:jc w:val="center"/>"#.to_string(),
        Some(roe_model::style::Alignment::Right) => r#"<w:jc w:val="right"/>"#.to_string(),
        Some(roe_model::style::Alignment::Justify) => r#"<w:jc w:val="both"/>"#.to_string(),
        None => String::new(),
    };

    // 已知parser缺口（2026-09-16会话发现）：docx.rs正文段落级（in_ppr_body
    // 状态机路径）完全不解析w:ind标签。文件里唯二两处b"w:ind" if in_ppr的
    // 匹配，一处在styles.xml段落样式定义路径（459行，state变量in_ppr不带
    // _body后缀，服务于ParagraphStyle而非正文Paragraph），另一处在
    // parse_isolated_paragraphs（4979行，note/页眉页脚专用小函数）路径，
    // 两者都不是"读取正文<w:p><w:pPr><w:ind>"这条路径。与w:jc/w:spacing
    // （两者均有对应的in_ppr_body && cur_para.is_some()正文解析分支）
    // 形成对比，是一个真实的、独立的parser缺口，不是writer bug。
    // 因此indent_left/indent_first两个字段：写出逻辑正确（below），但
    // 无法通过write→reparse验证，测试改用直接断言write_paragraph_xml生成
    // 的XML字符串，与Run批次underline_style/script的验证方式一致。
    //
    // w:ind：indent_left直接对应w:left（pt*20=twips）。indent_first符号
    // 决定写w:firstLine还是w:hanging——与docx.rs 468-476行反向对称：
    // 正值时docx.rs写入indent_first为正（来自w:firstLine），负值时
    // docx.rs取相反数存为indent_first（来自w:hanging）。写出时按同一
    // 约定反向映射：indent_first>=0 -> w:firstLine=值本身；
    // indent_first<0 -> w:hanging=绝对值。两者互斥，不会同时写两个属性
    // （与解析端"每次只可能命中一个attr.key"的语义一致）。
    let mut ind_attrs = String::new();
    if let Some(left) = p.indent_left {
        let twips = (left as f64 * 20.0).round() as i64;
        ind_attrs.push_str(&format!(r#" w:left="{twips}""#));
    }
    if let Some(first) = p.indent_first {
        if first >= 0.0 {
            let twips = (first as f64 * 20.0).round() as i64;
            ind_attrs.push_str(&format!(r#" w:firstLine="{twips}""#));
        } else {
            let twips = ((-first) as f64 * 20.0).round() as i64;
            ind_attrs.push_str(&format!(r#" w:hanging="{twips}""#));
        }
    }
    let ind_xml = if ind_attrs.is_empty() {
        String::new()
    } else {
        format!(r#"<w:ind{ind_attrs}/>"#)
    };

    // w:spacing：space_before/space_after为pt*20=twips；line_height是
    // docx.rs按"val/240.0"换算成的倍数（240=单倍行距，480=双倍），写出
    // 时反向乘240还原w:line原始值。三个属性可以任意子集组合出现在同一个
    // <w:spacing>标签上（与解析端每个attr独立判断一致），全部为None时
    // 不写整个标签。
    let mut spacing_attrs = String::new();
    if let Some(before) = p.space_before {
        let twips = (before as f64 * 20.0).round() as i64;
        spacing_attrs.push_str(&format!(r#" w:before="{twips}""#));
    }
    if let Some(after) = p.space_after {
        let twips = (after as f64 * 20.0).round() as i64;
        spacing_attrs.push_str(&format!(r#" w:after="{twips}""#));
    }
    if let (Some(abs), Some(exact)) = (p.line_abs_pt, p.line_exact) {
        let val = (abs as f64 * 20.0).round() as i64;
        let rule = if exact { "exact" } else { "atLeast" };
        spacing_attrs.push_str(&format!(r#" w:line="{val}" w:lineRule="{rule}""#));
    } else if let Some(lh) = p.line_height {
        let val = (lh as f64 * 240.0).round() as i64;
        spacing_attrs.push_str(&format!(r#" w:line="{val}""#));
    }
    let spacing_xml = if spacing_attrs.is_empty() {
        String::new()
    } else {
        format!(r#"<w:spacing{spacing_attrs}/>"#)
    };

    // style 为空字符串（StyleRef 默认值）时不写 w:pStyle，避免写出无意义的
    // <w:pStyle w:val=""/>；parser 在没有 w:pStyle 时本就回退为 "Normal"。
    let pstyle_xml = if p.style.0.is_empty() {
        String::new()
    } else {
        format!(
            r#"<w:pStyle w:val="{}"/>"#,
            xml_escape_attr(&p.style.0)
        )
    };

    // v255: ROE_ 列表段写真正的 w:numPr（映射在 DocxWriter::write 里按 list_id 建好）
    let numpr_xml = match &p.list_info {
        Some(li) => LIST_NUM
            .with(|m| m.borrow().get(&li.list_id).copied())
            .map(|nid| format!(r#"<w:numPr><w:ilvl w:val="{}"/><w:numId w:val="{}"/></w:numPr>"#, li.level, nid))
            .unwrap_or_default(),
        None => String::new(),
    };
    let pb_xml = if p.page_break_before { "<w:pageBreakBefore/>" } else { "" };
    let shd_xml = match p.shading {
        Some(c) => format!(r#"<w:shd w:val="clear" w:color="auto" w:fill="{:02X}{:02X}{:02X}"/>"#, c[0], c[1], c[2]),
        None => String::new(),
    };
    let ppr_inner = format!("{pstyle_xml}{pb_xml}{numpr_xml}{shd_xml}{jc_xml}{ind_xml}{spacing_xml}");
    let ppr_xml = if ppr_inner.is_empty() {
        String::new()
    } else {
        format!("<w:pPr>{ppr_inner}</w:pPr>")
    };

    format!("<w:p>{ppr_xml}{runs_xml}</w:p>\n")
}

// ─── Run 内联格式（w:rPr）写出 ─────────────────────────────────────────────────
//
// 范围（2026-09-16 会话，Writer接口开发设计文档 v1 1.1节"字符格式补全"的
// 存盘落地批次）：bold/italic/strikethrough/color/background/font_family/
// font_family_ascii/underline(bool)/font_size 九项。
//
// 明确不在本函数写出范围内（docx.rs 正文 run 级 w:rPr 解析端本身不支持，
// 详见 ROE_Progress_v168.md 3.1节踩坑记录延续排查结论）：
// - underline_style（四选一枚举：单/双/波浪/无）：docx.rs 的 w:u 解析只读
//   w:val != "none" 转成 bool，w:val 具体是 single/double/wave 完全没有
//   映射到 UnderlineStyle 枚举。这里仍然按 underline_style 优先、否则退回
//   underline(bool) 的语义写出 w:val，写出这一半是对的，只是 reparse 验证
//   不了 underline_style 本身（reparse 只能拿回 bool），测试需要用直接
//   断言 XML 字符串的方式验证，不能用 write→reparse。
// - script（上标/下标 ScriptPosition）：docx.rs 正文 run 内联 w:vertAlign
//   完全未解析（唯一读取 w:vertAlign 的地方是 styles.xml 字符样式定义,
//   走 CharProps/to_char_style 独立路径，与 run 自身内联 script 字段是
//   两回事）。同样，这里仍然正确写出 w:vertAlign，只是测试要用直接断言
//   XML 字符串验证，不能用 write→reparse。
fn write_run_props_xml(run: &roe_model::writer::Run) -> String {
    let mut inner = String::new();

    // w:rStyle：引用命名字符样式（CharStyle），必须放在 <w:rPr> 内其余
    // 属性之前——照抄真实样本 tdf141548.docx 的既有顺序（docx.rs 9855行
    // 附近注释提到的样本）。这是本次摸底新发现的写出层缺口：Run.char_style
    // 字段和解析层 w:rStyle 读取（docx.rs 3494/5105行等）早就存在，但
    // 写出层此前从未把 char_style 转成 <w:rStyle> 标签，导致就算定义了
    // CharStyle，Run 也没有办法在真实文档里"引用"它——两者是配套的，
    // 光有 write_one_char_style_xml 定义样式本身，没有这一步，样式定义
    // 出来的东西在文档里永远用不上。
    if let Some(ref style_ref) = run.char_style {
        inner.push_str(&format!(
            r#"<w:rStyle w:val="{}"/>"#,
            xml_escape_attr(&style_ref.0)
        ));
    }

    if let Some(true) = run.bold {
        inner.push_str("<w:b/>");
    } else if let Some(false) = run.bold {
        inner.push_str(r#"<w:b w:val="false"/>"#);
    }

    if let Some(true) = run.italic {
        inner.push_str("<w:i/>");
    } else if let Some(false) = run.italic {
        inner.push_str(r#"<w:i w:val="false"/>"#);
    }

    if let Some(true) = run.strikethrough {
        inner.push_str("<w:strike/>");
    }

    // 下划线：underline_style 优先（四选一映射到 w:val），否则退回
    // underline(bool)（true -> single，false -> none）。两个字段都为 None
    // 时不写 w:u。docx.rs 解析端目前只能把 w:val 还原成 bool（见函数头
    // 注释），underline_style 本身的 reparse 验证走不通，是已知、记录在案
    // 的 parser 端缺口，不是这里写出逻辑的问题。
    let underline_val = underline_val_str(run);
    if let Some(val) = underline_val {
        inner.push_str(&format!(r#"<w:u w:val="{val}"/>"#));
    }

    if let Some(sz) = run.font_size {
        // w:sz 是半点单位（half-point），与 docx.rs 解析端 3322 行附近
        // 换算规则（/2.0）互为逆运算，这里 *2.0。
        let half_points = (sz * 2.0).round() as i32;
        inner.push_str(&format!(r#"<w:sz w:val="{half_points}"/>"#));
    }

    if let Some(color) = run.color {
        inner.push_str(&format!(
            r#"<w:color w:val="{:02X}{:02X}{:02X}"/>"#,
            color.r, color.g, color.b
        ));
    }

    // background 用户确认走 w:shd（任意RGB十六进制），不用 w:highlight
    // （只支持16种预定义颜色名，会丢失精度）。
    if let Some(bg) = run.background {
        inner.push_str(&format!(r#"<w:shd w:val="clear" w:color="auto" {}/>"#, xml_fill_attr(Some(bg))));
    }

    // 上标/下标：docx.rs 正文 run 内联完全不解析，写出仍然正确，测试需要
    // 用直接断言 XML 验证（见函数头注释）。
    match run.script {
        Some(roe_model::writer::ScriptPosition::Superscript) => {
            inner.push_str(r#"<w:vertAlign w:val="superscript"/>"#);
        }
        Some(roe_model::writer::ScriptPosition::Subscript) => {
            inner.push_str(r#"<w:vertAlign w:val="subscript"/>"#);
        }
        Some(roe_model::writer::ScriptPosition::Normal) | None => {}
    }

    // 字体名：东亚 font_family 优先写 w:eastAsia，font_family_ascii 写
    // w:ascii。与 docx.rs 解析端（3434行附近）的读取顺序对称
    // （east_asia_font.or(ascii_font) 作为最终 font_family）。
    if run.font_family.is_some() || run.font_family_ascii.is_some() {
        let mut attrs = String::new();
        if let Some(ref ascii) = run.font_family_ascii {
            attrs.push_str(&format!(r#" w:ascii="{}""#, xml_escape_attr(ascii)));
        }
        if let Some(ref east_asia) = run.font_family {
            attrs.push_str(&format!(r#" w:eastAsia="{}""#, xml_escape_attr(east_asia)));
        }
        inner.push_str(&format!(r#"<w:rFonts{attrs}/>"#));
    }

    if inner.is_empty() {
        String::new()
    } else {
        format!("<w:rPr>{inner}</w:rPr>")
    }
}

/// underline(bool)/underline_style 二选一合并成最终 w:val 字符串。
/// underline_style 优先；两者都为 None 时返回 None（不写 w:u 标签）。
fn underline_val_str(run: &roe_model::writer::Run) -> Option<&'static str> {
    if let Some(style) = run.underline_style {
        return Some(match style {
            roe_model::writer::UnderlineStyle::Single => "single",
            roe_model::writer::UnderlineStyle::Double => "double",
            roe_model::writer::UnderlineStyle::Wave => "wave",
            roe_model::writer::UnderlineStyle::None => "none",
        });
    }
    match run.underline {
        Some(true) => Some("single"),
        Some(false) => Some("none"),
        None => None,
    }
}

// ─── XML 转义 ──────────────────────────────────────────────────────────────────

fn xml_escape_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn xml_escape_attr(s: &str) -> String {
    xml_escape_text(s).replace('"', "&quot;")
}

// ─── 测试 ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ooxml::docx::DocxParser;
    use roe_model::common::StyleRef;
    use roe_model::writer::{Paragraph, Run};

    fn make_doc(body: Vec<BodyElement>) -> WriterDocument {
        WriterDocument {
            body,
            ..Default::default()
        }
    }

    #[test]
    fn test_roundtrip_plain_paragraph() {
        let para = Paragraph {
            style: StyleRef("Normal".to_string()),
            runs: vec![Run {
                text: "Hello world".to_string(),
                ..Default::default()
            }],
            list_info: None,
            ..Default::default()
        };
        let doc = make_doc(vec![BodyElement::Paragraph(para)]);

        let bytes = DocxWriter::write(&doc).expect("write failed");
        let parsed = DocxParser::parse(&bytes).expect("roundtrip parse failed");

        assert_eq!(parsed.body.len(), 1);
        match &parsed.body[0] {
            BodyElement::Paragraph(p) => {
                assert_eq!(p.style.0, "Normal");
                assert_eq!(p.runs.len(), 1);
                assert_eq!(p.runs[0].text, "Hello world");
            }
            _ => panic!("expected Paragraph"),
        }
    }

    #[test]
    fn test_roundtrip_formula_latex() {
        // v224: 公式存盘往返。Run.formula.latex 经 LaTeX->OMML->LaTeX 后应一致。
        let cases = [
            r"x=\frac{a+b}{c+d}",
            r"x^{2}+y_{i}",
            r"\sqrt{x+1}",
            r"\sum_{i=1}^{n} a_{i}",
        ];
        for latex in cases {
            let para = Paragraph {
                style: StyleRef("Normal".to_string()),
                runs: vec![Run {
                    formula: Some(roe_model::writer::Formula {
                        latex: latex.to_string(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                list_info: None,
                ..Default::default()
            };
            let doc = make_doc(vec![BodyElement::Paragraph(para)]);
            let bytes = DocxWriter::write(&doc).expect("write failed");
            let parsed = DocxParser::parse(&bytes).expect("roundtrip parse failed");
            let got: Vec<String> = parsed
                .body
                .iter()
                .filter_map(|el| match el {
                    BodyElement::Paragraph(p) => Some(p),
                    _ => None,
                })
                .flat_map(|p| p.runs.iter())
                .filter_map(|r| r.formula.as_ref().map(|f| f.latex.clone()))
                .collect();
            assert_eq!(got, vec![latex.to_string()], "往返后 latex 不一致: {}", latex);
        }
    }

    #[test]
    fn test_roundtrip_real_docx_formulas() {
        // v224: 用真实 Word 样本做公式往返：读入 -> 写出 -> 再读入，逐个对比 latex。
        let path = "/home/yong/small__sw_ooxmlexport_data__math-mso2k7.docx";
        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(_) => {
                eprintln!("[ROUNDTRIP] 样本不存在，跳过: {}", path);
                return;
            }
        };
        macro_rules! formulas {
            ($d:expr) => {{
                $d.body
                    .iter()
                    .filter_map(|el| match el {
                        BodyElement::Paragraph(p) => Some(p),
                        _ => None,
                    })
                    .flat_map(|p| p.runs.iter())
                    .filter_map(|r| r.formula.as_ref().map(|f| f.latex.clone()))
                    .collect::<Vec<String>>()
            }};
        }
        let first = DocxParser::parse(&data).expect("parse sample failed");
        let before = formulas!(first);
        let bytes = DocxWriter::write(&first).expect("write failed");
        let second = DocxParser::parse(&bytes).expect("reparse failed");
        let after = formulas!(second);
        eprintln!("[ROUNDTRIP] 读入公式数={} 写出再读入公式数={}", before.len(), after.len());
        let mut bad = 0;
        for i in 0..before.len().max(after.len()) {
            let a = before.get(i).cloned().unwrap_or_else(|| "<缺失>".to_string());
            let b = after.get(i).cloned().unwrap_or_else(|| "<缺失>".to_string());
            if a != b {
                bad += 1;
                eprintln!("[ROUNDTRIP] #{} 不一致\n    原: {}\n    回: {}", i, a, b);
            }
        }
        assert!(before.len() > 0, "样本里没读到任何公式");
        assert_eq!(bad, 0, "共 {} 个公式往返后不一致", bad);
    }

    #[test]
    fn test_roundtrip_multiple_paragraphs() {
        let p1 = Paragraph {
            style: StyleRef("Heading1".to_string()),
            runs: vec![Run { text: "Title".to_string(), ..Default::default() }],
            list_info: None,
            ..Default::default()
        };
        let p2 = Paragraph {
            style: StyleRef("Normal".to_string()),
            runs: vec![Run { text: "Body text here".to_string(), ..Default::default() }],
            list_info: None,
            ..Default::default()
        };
        let doc = make_doc(vec![
            BodyElement::Paragraph(p1),
            BodyElement::Paragraph(p2),
        ]);

        let bytes = DocxWriter::write(&doc).expect("write failed");
        let parsed = DocxParser::parse(&bytes).expect("roundtrip parse failed");

        assert_eq!(parsed.body.len(), 2);
        match &parsed.body[0] {
            BodyElement::Paragraph(p) => {
                assert_eq!(p.style.0, "Heading1");
                assert_eq!(p.runs[0].text, "Title");
            }
            _ => panic!("expected Paragraph"),
        }
        match &parsed.body[1] {
            BodyElement::Paragraph(p) => {
                assert_eq!(p.style.0, "Normal");
                assert_eq!(p.runs[0].text, "Body text here");
            }
            _ => panic!("expected Paragraph"),
        }
    }

    #[test]
    fn test_roundtrip_multiple_runs_in_paragraph() {
        // 注：docx.rs parser 用 quick-xml trim_text(true)，文本节点首尾空白
        // 会被自动去除。这里两个 run 的文字本身不含首尾空格，避免测试踩中
        // 这个已知的 parser 行为（trim_text 是 parser 现状，不是 writer 的
        // bug，这次不在 docx writer 任务范围内修改 parser 配置）。
        let para = Paragraph {
            style: StyleRef("Normal".to_string()),
            runs: vec![
                Run { text: "Hello".to_string(), ..Default::default() },
                Run { text: "World".to_string(), ..Default::default() },
            ],
            list_info: None,
            ..Default::default()
        };
        let doc = make_doc(vec![BodyElement::Paragraph(para)]);

        let bytes = DocxWriter::write(&doc).expect("write failed");
        let parsed = DocxParser::parse(&bytes).expect("roundtrip parse failed");

        match &parsed.body[0] {
            BodyElement::Paragraph(p) => {
                // parser 会把同一个 <w:p> 内多个 <w:r> 分别解析成多个 Run
                assert_eq!(p.runs.len(), 2);
                assert_eq!(p.runs[0].text, "Hello");
                assert_eq!(p.runs[1].text, "World");
            }
            _ => panic!("expected Paragraph"),
        }
    }

    // 1x1 透明像素 PNG 的最小合法字节序列，用于图片写出测试。不追求
    // 真实图片内容，只用于验证"写出的字节能被 docx.rs parser 认成
    // image/png 并原样读回"这个链路本身是否正确。
    fn tiny_png_bytes() -> Vec<u8> {
        vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // PNG signature
            0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, // IHDR chunk
            0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01,
            0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
            0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, // IDAT chunk
            0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
            0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, // IEND chunk
            0x42, 0x60, 0x82,
        ]
    }

    #[test]
    fn test_roundtrip_single_section_page_layout() {
        // 未分节文档（doc.sections 恒为空）时，body 末尾的"最终 sectPr"
        // 应该用 WriterDocument 顶层字段合成，验证 write_final_section_pr_xml
        // 的兜底路径。用顶层 page_size/margin 与默认值明显不同的值，
        // 避免和 A4 默认值撞车导致测试即使写出逻辑有bug也侥幸通过。
        let mut doc = make_doc(vec![BodyElement::Paragraph(Paragraph {
            style: StyleRef("Normal".to_string()),
            runs: vec![Run { text: "Body text".to_string(), ..Default::default() }],
            list_info: None,
            ..Default::default()
        })]);
        doc.page_size = (400.0, 600.0);
        doc.margin_top = 50.0;
        doc.margin_bottom = 60.0;
        doc.margin_left = 70.0;
        doc.margin_right = 80.0;

        let bytes = DocxWriter::write(&doc).expect("write failed");
        let parsed = DocxParser::parse(&bytes).expect("roundtrip parse failed");

        // parser 端 page_size/margin_* 的真正读取路径是"扫描所有 w:sectPr
        // 收集 layout Vec，取最后一个覆盖顶层字段"（见 docx.rs 6190行附近
        // 的 read_page_layouts 类函数），与 doc.sections 是否被填充无关，
        // 这里直接断言 WriterDocument 顶层字段，是最贴近 parser 实际行为
        // 的验证方式。
        assert_eq!(parsed.page_size.0, 400.0);
        assert_eq!(parsed.page_size.1, 600.0);
        assert_eq!(parsed.margin_top, 50.0);
        assert_eq!(parsed.margin_bottom, 60.0);
        assert_eq!(parsed.margin_left, 70.0);
        assert_eq!(parsed.margin_right, 80.0);
    }

    #[test]
    fn test_roundtrip_multi_section() {
        // 两节文档：body 里 para1 + Section路标(取doc.sections[0]) + para2，
        // sections 数组只放1个元素（对应第一节的属性；第二节/最终节走
        // write_final_section_pr_xml 用顶层字段兜底）。验证：
        // 1) Section 路标写出为独立的哑段落，不影响前后两个真实段落的文字
        // 2) 第一节的 page_size 通过 sectPr 正确写入 XML（用直接检查
        //    write_document_xml_with_media 生成的原始 XML 字符串验证，
        //    因为 doc.sections 本身是否被 parser 正确填充回是另一件事——
        //    parser 侧 read_header_footer_refs/read_page_layouts 只负责
        //    "收集所有 sectPr"，调用方是否真的把 Vec 存进 doc.sections
        //    是 docx.rs 主循环里的事，不在本批 docx_writer 任务范围内，
        //    这里只验证"写出"这一半：sectPr 内容本身对不对）。
        use roe_model::writer::Section;

        let para1 = Paragraph {
            style: StyleRef("Normal".to_string()),
            runs: vec![Run { text: "Section one text".to_string(), ..Default::default() }],
            list_info: None,
            ..Default::default()
        };
        let para2 = Paragraph {
            style: StyleRef("Normal".to_string()),
            runs: vec![Run { text: "Section two text".to_string(), ..Default::default() }],
            list_info: None,
            ..Default::default()
        };

        let section1 = Section {
            name: "Section 1".to_string(),
            content: Vec::new(),
            page_size: Some((300.0, 500.0)),
            margin_top: Some(40.0),
            margin_bottom: None,
            margin_left: None,
            margin_right: None,
            header: None,
            footer: None,
            title_page: Some(true),
            even_and_odd_headers: None,
            section_type: Some("evenPage".to_string()),
            columns: None,
        };

        let mut doc = make_doc(vec![
            BodyElement::Paragraph(para1),
            BodyElement::Section(section1),
            BodyElement::Paragraph(para2),
        ]);
        doc.sections = vec![]; // 本测试直接检查 XML，不依赖 sections 数组内容

        let bytes = DocxWriter::write(&doc).expect("write failed");
        let parsed = DocxParser::parse(&bytes).expect("roundtrip parse failed");

        let paras: Vec<&Paragraph> = parsed
            .body
            .iter()
            .filter_map(|b| if let BodyElement::Paragraph(p) = b { Some(p) } else { None })
            .collect();
        let texts: Vec<String> = paras
            .iter()
            .map(|p| p.runs.iter().map(|r| r.text.as_str()).collect::<String>())
            .collect();

        assert!(texts.contains(&"Section one text".to_string()));
        assert!(texts.contains(&"Section two text".to_string()));
        // Section 路标写出的哑段落应该也在，且没有文字（沿用图片壳段落
        // 同样的验证思路）
        assert!(
            texts.iter().any(|t| t.is_empty()),
            "应存在Section路标对应的空文字哑段落，实际texts={:?}",
            texts
        );

        // 直接检查 write_section_pr_xml 本身的输出是否正确（绕开
        // "doc.sections是否被parser正确填充回"这个不在本批范围内的
        // 环节，与 test_roundtrip_table_style_and_borders 的验证思路
        // 一致：写出正确性优先用直接断言XML验证，reparse只用来验证
        // "不会把文档写坏、body结构不会崩"）。
        let section_for_xml_check = Section {
            name: String::new(),
            content: Vec::new(),
            page_size: Some((300.0, 500.0)),
            margin_top: Some(40.0),
            margin_bottom: None,
            margin_left: None,
            margin_right: None,
            header: None,
            footer: None,
            title_page: Some(true),
            even_and_odd_headers: None,
            section_type: Some("evenPage".to_string()),
            columns: None,
        };
        let xml = write_section_pr_xml(&section_for_xml_check);
        assert!(xml.contains("<w:titlePg/>"), "应包含titlePg，实际XML: {xml}");
        assert!(
            xml.contains(r#"<w:type w:val="evenPage"/>"#),
            "应包含section_type，实际XML: {xml}"
        );
        assert!(
            xml.contains(r#"<w:pgSz w:w="6000" w:h="10000"/>"#),
            "300pt=6000twips, 500pt=10000twips，实际XML: {xml}"
        );
        assert!(
            xml.contains(r#"w:top="800""#),
            "40pt=800twips，实际XML: {xml}"
        );
        // margin_bottom/left/right 均为None，不应出现对应属性
        assert!(!xml.contains("w:bottom="), "margin_bottom为None不应写出，实际XML: {xml}");
    }

    #[test]
    fn test_roundtrip_simple_image() {
        // 验证图片写出链路：word/media/imageN.png 物理文件、document.xml.rels
        // 关系条目、document.xml 里 r:embed 引用、wp:extent 尺寸，四者
        // 全部对得上，且 write→reparse 后能被识别为 BodyElement::Image。
        use roe_model::common::ImageData;
        use roe_model::writer::InlineImage;

        let png = tiny_png_bytes();
        let img = InlineImage {
            image: ImageData {
                data: png.clone(),
                mime: "image/png".to_string(),
                ..Default::default()
            },
            width_pt: 50.0,
            height_pt: 30.0,
            dist_t_pt: 0.0,
            dist_b_pt: 0.0,
            dist_l_pt: 0.0,
            dist_r_pt: 0.0,
            rotation: None,
        };
        let doc = make_doc(vec![BodyElement::Image(img)]);

        let bytes = DocxWriter::write(&doc).expect("write failed");
        let parsed = DocxParser::parse(&bytes).expect("roundtrip parse failed");

        let images: Vec<_> = parsed
            .body
            .iter()
            .filter_map(|b| if let BodyElement::Image(i) = b { Some(i) } else { None })
            .collect();
        assert_eq!(images.len(), 1, "expected exactly 1 image, body={:?}", parsed.body);

        let parsed_img = images[0];
        assert_eq!(parsed_img.image.mime, "image/png");
        assert_eq!(parsed_img.image.data, png, "图片字节应原样roundtrip");
        assert!(
            (parsed_img.width_pt - 50.0).abs() < 0.5,
            "width_pt={}",
            parsed_img.width_pt
        );
        assert!(
            (parsed_img.height_pt - 30.0).abs() < 0.5,
            "height_pt={}",
            parsed_img.height_pt
        );
    }

    #[test]
    fn test_roundtrip_paragraph_alignment() {
        use roe_model::style::Alignment;
        for (alignment, expected_jc) in [
            (Alignment::Left, "left"),
            (Alignment::Center, "center"),
            (Alignment::Right, "right"),
            (Alignment::Justify, "both"),
        ] {
            let mut p = roe_model::writer::Paragraph::default();
            p.alignment = Some(alignment);
            p.runs.push(roe_model::writer::Run { text: "x".to_string(), ..Default::default() });
            let doc = make_doc(vec![BodyElement::Paragraph(p)]);
            let bytes = DocxWriter::write(&doc).expect("write_docx failed");
            let reparsed = DocxParser::parse(&bytes).expect("reparse failed");
            let para = reparsed.body.iter().find_map(|b| {
                if let BodyElement::Paragraph(p) = b { Some(p) } else { None }
            }).expect("no paragraph found");
            let ok = match (&para.alignment, &alignment) {
                (Some(Alignment::Left), Alignment::Left) => true,
                (Some(Alignment::Center), Alignment::Center) => true,
                (Some(Alignment::Right), Alignment::Right) => true,
                (Some(Alignment::Justify), Alignment::Justify) => true,
                _ => false,
            };
            assert!(ok, "expected_jc={expected_jc}, got para.alignment={:?}", para.alignment);
        }
    }


    #[test]
    fn test_roundtrip_paragraph_style_definition() {
        // 验证 write_styles_xml 第一版覆盖的六个字段：
        // name/based_on/bold/italic/font_size/alignment。
        // 这是 write -> reparse 的端到端测试：真正写出 word/styles.xml
        // 字节流，再走完整 DocxParser::parse（含 read_styles ->
        // build_style_registry 链路，见 docx.rs 第125行赋值处），
        // 不是只在内存里构造中间结构，避免重演 ROE_Progress_v172
        // 记录过的"只测一半链路"问题。
        use roe_model::style::{Alignment, ParagraphStyle, StyleRegistry};

        let mut doc = make_doc(vec![]);
        let base_style = ParagraphStyle {
            name: "BaseStyle".to_string(),
            bold: false,
            italic: false,
            font_size: Some(10.0),
            ..Default::default()
        };
        let derived_style = ParagraphStyle {
            name: "DerivedStyle".to_string(),
            based_on: Some("BaseStyle".to_string()),
            bold: true,
            italic: true,
            font_size: Some(14.0),
            alignment: Some(Alignment::Center),
            ..Default::default()
        };
        let mut registry = StyleRegistry::default();
        registry.paragraph_styles.insert("BaseStyle".to_string(), base_style);
        registry.paragraph_styles.insert("DerivedStyle".to_string(), derived_style);
        doc.styles = registry;

        let bytes = DocxWriter::write(&doc).expect("write_docx failed");
        let reparsed = DocxParser::parse(&bytes).expect("reparse failed");

        let base = reparsed
            .styles
            .paragraph_styles
            .get("BaseStyle")
            .expect("BaseStyle not found after reparse");
        assert_eq!(base.name, "BaseStyle");
        assert_eq!(base.based_on, None);
        assert!(!base.bold);
        assert!(!base.italic);
        assert!(
            (base.font_size.expect("font_size missing") - 10.0).abs() < 0.01,
            "font_size={:?}",
            base.font_size
        );

        let derived = reparsed
            .styles
            .paragraph_styles
            .get("DerivedStyle")
            .expect("DerivedStyle not found after reparse");
        assert_eq!(derived.name, "DerivedStyle");
        assert_eq!(derived.based_on.as_deref(), Some("BaseStyle"));
        assert!(derived.bold);
        assert!(derived.italic);
        assert!(
            (derived.font_size.expect("font_size missing") - 14.0).abs() < 0.01,
            "font_size={:?}",
            derived.font_size
        );
        assert!(
            matches!(derived.alignment, Some(Alignment::Center)),
            "alignment={:?}",
            derived.alignment
        );
    }


    #[test]
    fn test_roundtrip_char_style_definition() {
        // CharStyle 写出层第一小步验证：八个基础字段（不含 background，
        // 后者留给独立的第二小步，见交接文档）。字段选择依据 docx.rs
        // read_styles 的 in_rpr 分支摸底结论：w:rFonts(566行)/w:sz/
        // w:color(599行)/w:u(617行)/w:vertAlign(632行)/w:strike(645行)
        // 全部支持，可以走完整 write -> reparse 验证，不需要像 background
        // 那样退回直接断言 XML。
        use roe_model::common::Color;
        use roe_model::style::{CharStyle, StyleRegistry};
        use roe_model::writer::ScriptPosition;

        let mut doc = make_doc(vec![]);
        let base_style = CharStyle {
            name: "BaseCharStyle".to_string(),
            bold: true,
            italic: false,
            underline: true,
            font_size: Some(9.0),
            color: Some(Color { r: 0x12, g: 0x34, b: 0x56, a: 255 }),
            font_family: Some("SimSun".to_string()),
            font_family_ascii: Some("Times New Roman".to_string()),
            // background：本次会话新接通的字段，走 w:shd（任意RGB），不是
            // w:highlight（16种预设色名）。故意选一个不在16种预设色里的
            // RGB值，确保测的是 w:shd 路径而不是巧合落在某个预设色上。
            background: Some(Color { r: 0x7A, g: 0x3C, b: 0x9E, a: 255 }),
            ..Default::default()
        };
        let derived_style = CharStyle {
            name: "DerivedCharStyle".to_string(),
            based_on: Some("BaseCharStyle".to_string()),
            bold: false,
            italic: true,
            underline: false,
            strikethrough: Some(true),
            script: Some(ScriptPosition::Superscript),
            font_size: Some(7.0),
            ..Default::default()
        };
        let mut registry = StyleRegistry::default();
        registry.char_styles.insert("BaseCharStyle".to_string(), base_style);
        registry.char_styles.insert("DerivedCharStyle".to_string(), derived_style);
        doc.styles = registry;

        let bytes = DocxWriter::write(&doc).expect("write_docx failed");
        let reparsed = DocxParser::parse(&bytes).expect("reparse failed");

        let base = reparsed
            .styles
            .char_styles
            .get("BaseCharStyle")
            .expect("BaseCharStyle not found after reparse");
        assert_eq!(base.name, "BaseCharStyle");
        assert_eq!(base.based_on, None);
        assert!(base.bold);
        assert!(!base.italic);
        assert!(base.underline);
        assert!(
            (base.font_size.expect("font_size missing") - 9.0).abs() < 0.01,
            "font_size={:?}",
            base.font_size
        );
        let base_color = base.color.expect("color missing");
        assert_eq!((base_color.r, base_color.g, base_color.b), (0x12, 0x34, 0x56));
        assert_eq!(base.font_family.as_deref(), Some("SimSun"));
        assert_eq!(base.font_family_ascii.as_deref(), Some("Times New Roman"));
        let base_bg = base.background.expect("background missing");
        assert_eq!(
            (base_bg.r, base_bg.g, base_bg.b),
            (0x7A, 0x3C, 0x9E),
            "background应该走w:shd精确读回，不是丢失或被映射成某个预设色"
        );

        let derived = reparsed
            .styles
            .char_styles
            .get("DerivedCharStyle")
            .expect("DerivedCharStyle not found after reparse");
        assert_eq!(derived.based_on.as_deref(), Some("BaseCharStyle"));
        assert!(!derived.bold);
        assert!(derived.italic);
        assert!(!derived.underline);
        assert_eq!(derived.strikethrough, Some(true));
        assert!(
            matches!(derived.script, Some(ScriptPosition::Superscript)),
            "script={:?}",
            derived.script
        );
        assert!(
            (derived.font_size.expect("font_size missing") - 7.0).abs() < 0.01,
            "font_size={:?}",
            derived.font_size
        );
    }


    #[test]
    fn test_roundtrip_run_char_style_reference() {
        // 验证 Run.char_style -> <w:rStyle w:val="..."/> 写出层缺口修复
        // （本次会话新发现：解析层 w:rStyle 读取早就存在，写出层此前
        // 从未实现，导致定义了 CharStyle 也没法在文档里被引用）。
        // 只验证"引用关系"本身能写出并读回，不涉及 CharStyle 定义内容
        // 本身（那是 test_roundtrip_char_style_definition 已经覆盖的）。
        use roe_model::common::StyleRef;
        use roe_model::style::{CharStyle, StyleRegistry};

        let mut doc = make_doc(vec![]);
        let mut registry = StyleRegistry::default();
        registry.char_styles.insert(
            "Emphasis2".to_string(),
            CharStyle {
                name: "Emphasis2".to_string(),
                bold: true,
                ..Default::default()
            },
        );
        doc.styles = registry;

        let mut para = roe_model::writer::Paragraph::default();
        para.runs.push(roe_model::writer::Run {
            text: "styled run".to_string(),
            char_style: Some(StyleRef("Emphasis2".to_string())),
            ..Default::default()
        });
        para.runs.push(roe_model::writer::Run {
            text: "plain run".to_string(),
            ..Default::default()
        });
        doc.body.push(roe_model::writer::BodyElement::Paragraph(para));

        let bytes = DocxWriter::write(&doc).expect("write_docx failed");
        let reparsed = DocxParser::parse(&bytes).expect("reparse failed");

        let roe_model::writer::BodyElement::Paragraph(reparsed_para) = &reparsed.body[0] else {
            panic!("expected a Paragraph body element");
        };
        assert_eq!(reparsed_para.runs.len(), 2);
        assert_eq!(
            reparsed_para.runs[0].char_style.as_ref().map(|s| s.0.as_str()),
            Some("Emphasis2"),
            "styled run 应该带有 char_style 引用"
        );
        assert_eq!(
            reparsed_para.runs[1].char_style, None,
            "plain run 不应该带有 char_style 引用"
        );
    }


    #[test]
    fn test_roundtrip_paragraph_style_indent_and_color() {
        // 验证 indent_left/indent_first/color 三个字段：styles.xml 的
        // ParagraphStyle 解析路径（docx.rs 469行 b"w:ind" if in_ppr、
        // 599行 b"w:color" if in_rpr）确认支持这三个字段，走完整
        // write -> reparse 验证。indent_first 用负值测试 w:hanging 分支
        // （正值 w:firstLine 已经在正文段落测试里覆盖过，这里换一个分支
        // 增加覆盖面，而不是重复测同一个分支）。
        use roe_model::common::Color;
        use roe_model::style::{ParagraphStyle, StyleRegistry};

        let mut doc = make_doc(vec![]);
        let style = ParagraphStyle {
            name: "IndentColorStyle".to_string(),
            indent_left: Some(36.0),
            indent_first: Some(-18.0), // 负值 -> w:hanging 分支
            color: Some(Color { r: 0x12, g: 0x34, b: 0x56, a: 255 }),
            ..Default::default()
        };
        let mut registry = StyleRegistry::default();
        registry.paragraph_styles.insert("IndentColorStyle".to_string(), style);
        doc.styles = registry;

        let bytes = DocxWriter::write(&doc).expect("write_docx failed");
        let reparsed = DocxParser::parse(&bytes).expect("reparse failed");

        let got = reparsed
            .styles
            .paragraph_styles
            .get("IndentColorStyle")
            .expect("IndentColorStyle not found after reparse");

        assert!(
            (got.indent_left.expect("indent_left missing") - 36.0).abs() < 0.01,
            "indent_left={:?}",
            got.indent_left
        );
        assert!(
            (got.indent_first.expect("indent_first missing") - (-18.0)).abs() < 0.01,
            "indent_first={:?}",
            got.indent_first
        );
        let color = got.color.expect("color missing");
        assert_eq!((color.r, color.g, color.b), (0x12, 0x34, 0x56));
    }

    #[test]
    fn test_write_paragraph_style_spacing_xml_direct() {
        // line_height/space_before/space_after：styles.xml 的 ParagraphStyle
        // 解析路径完全没有 b"w:spacing" if in_ppr（非_body）分支——本次摸底
        // 新发现的 parser 缺口（与v170记录的"正文段落级不解析w:ind"同类，
        // 这次是"样式定义级不解析w:spacing"）。责任在解析层，写出逻辑本身
        // 是对的，但无法走 reparse 验证，改为直接断言 XML 字符串，与
        // test_write_paragraph_indent_first_line_xml_direct 等既有测试的
        // 处理方式一致。
        use roe_model::style::ParagraphStyle;

        let style = ParagraphStyle {
            name: "SpacingStyle".to_string(),
            space_before: Some(12.0),
            space_after: Some(6.0),
            line_height: Some(1.5),
            ..Default::default()
        };

        let xml = write_one_paragraph_style_xml(&style);

        // space_before: 12pt * 20 = 240 twips
        assert!(xml.contains(r#"w:before="240""#), "xml={xml}");
        // space_after: 6pt * 20 = 120 twips
        assert!(xml.contains(r#"w:after="120""#), "xml={xml}");
        // line_height: 1.5倍 * 240 = 360
        assert!(xml.contains(r#"w:line="360""#), "xml={xml}");
    }

    #[test]
    fn test_write_paragraph_indent_first_line_xml_direct() {
        // 正文段落级w:ind解析端不支持（见write_paragraph_xml函数头注释），
        // 故直接断言写出的XML字符串，不能走write→reparse。
        let mut p = roe_model::writer::Paragraph::default();
        p.indent_left = Some(36.0);
        p.indent_first = Some(18.0);
        p.runs.push(roe_model::writer::Run { text: "x".to_string(), ..Default::default() });
        let xml = write_paragraph_xml(&p);
        assert!(xml.contains(r#"w:left="720""#), "xml={xml}");
        assert!(xml.contains(r#"w:firstLine="360""#), "xml={xml}");
        assert!(!xml.contains("w:hanging"), "xml={xml}");
    }

    #[test]
    fn test_write_paragraph_indent_hanging_xml_direct() {
        // 同上，直接断言XML，不走write→reparse。
        let mut p = roe_model::writer::Paragraph::default();
        p.indent_left = Some(36.0);
        p.indent_first = Some(-18.0);
        p.runs.push(roe_model::writer::Run { text: "x".to_string(), ..Default::default() });
        let xml = write_paragraph_xml(&p);
        assert!(xml.contains(r#"w:left="720""#), "xml={xml}");
        assert!(xml.contains(r#"w:hanging="360""#), "xml={xml}");
        assert!(!xml.contains("w:firstLine"), "xml={xml}");
    }

    #[test]
    fn test_roundtrip_paragraph_spacing_and_line_height() {
        let mut p = roe_model::writer::Paragraph::default();
        p.space_before = Some(12.0);
        p.space_after = Some(6.0);
        p.line_height = Some(1.5);
        p.runs.push(roe_model::writer::Run { text: "x".to_string(), ..Default::default() });
        let doc = make_doc(vec![BodyElement::Paragraph(p)]);
        let bytes = DocxWriter::write(&doc).expect("write_docx failed");
        let reparsed = DocxParser::parse(&bytes).expect("reparse failed");
        let para = reparsed.body.iter().find_map(|b| {
            if let BodyElement::Paragraph(p) = b { Some(p) } else { None }
        }).expect("no paragraph found");
        assert!((para.space_before.unwrap() - 12.0).abs() < 0.01, "space_before={:?}", para.space_before);
        assert!((para.space_after.unwrap() - 6.0).abs() < 0.01, "space_after={:?}", para.space_after);
        assert!((para.line_height.unwrap() - 1.5).abs() < 0.01, "line_height={:?}", para.line_height);
    }

    #[test]
    fn test_roundtrip_image_and_paragraph_mixed() {
        // 图片与普通段落混排在同一个 body 里，验证两者互不干扰
        // （尤其是 rid_counter 独立于段落计数、body_xml 拼接顺序保留）。
        use roe_model::common::ImageData;
        use roe_model::writer::InlineImage;

        let para1 = Paragraph {
            style: StyleRef("Normal".to_string()),
            runs: vec![Run { text: "Before image".to_string(), ..Default::default() }],
            list_info: None,
            ..Default::default()
        };
        let img = InlineImage {
            image: ImageData {
                data: tiny_png_bytes(),
                mime: "image/png".to_string(),
                ..Default::default()
            },
            width_pt: 20.0,
            height_pt: 20.0,
            dist_t_pt: 0.0,
            dist_b_pt: 0.0,
            dist_l_pt: 0.0,
            dist_r_pt: 0.0,
            rotation: None,
        };
        let para2 = Paragraph {
            style: StyleRef("Normal".to_string()),
            runs: vec![Run { text: "After image".to_string(), ..Default::default() }],
            list_info: None,
            ..Default::default()
        };
        let doc = make_doc(vec![
            BodyElement::Paragraph(para1),
            BodyElement::Image(img),
            BodyElement::Paragraph(para2),
        ]);

        let bytes = DocxWriter::write(&doc).expect("write failed");
        let parsed = DocxParser::parse(&bytes).expect("roundtrip parse failed");

        let paras: Vec<&Paragraph> = parsed
            .body
            .iter()
            .filter_map(|b| if let BodyElement::Paragraph(p) = b { Some(p) } else { None })
            .collect();
        let images: Vec<_> = parsed
            .body
            .iter()
            .filter(|b| matches!(b, BodyElement::Image(_)))
            .collect();

        // 注：docx.rs parser 对 w:p 的解析是无条件 push 成
        // BodyElement::Paragraph（见 docx.rs 4716行附近，不区分该段落
        // 是否只含图片 run），而 write_image_drawing_xml 为满足 OOXML
        // "drawing 必须包在 w:p/w:r 内"的语法要求，给每张图片包了一层
        // 外壳段落。所以 write→reparse 后，body 里图片会同时产生一个
        // "壳段落"（该段落本身没有可见文字 run）和一个独立的
        // BodyElement::Image——这是本批次写出设计的已知、合理副作用，
        // 不是 bug：三段落计数 = before + 图片壳 + after。
        assert_eq!(paras.len(), 3, "图片外壳段落也会被解析成一个Paragraph，见上方注释");
        assert_eq!(images.len(), 1, "图片本身应作为独立BodyElement::Image保留");

        let texts: Vec<String> = paras
            .iter()
            .map(|p| p.runs.iter().map(|r| r.text.as_str()).collect::<String>())
            .collect();
        assert!(texts.contains(&"Before image".to_string()));
        assert!(texts.contains(&"After image".to_string()));
        // 图片壳段落应该没有文字 run（图片走 w:drawing，不产生 w:t 文字）
        assert!(
            texts.iter().any(|t| t.is_empty()),
            "应存在一个空文字的图片壳段落，实际texts={:?}",
            texts
        );
    }

    #[test]
    fn test_roundtrip_simple_table() {
        // 最基础场景：2行2列，每个 cell 一段纯文字，不涉及列宽/行高/
        // 样式/边框，验证 w:tbl/w:tr/w:tc 骨架本身能正确写出+读回。
        use roe_model::common::{CellContent, Table, TableCell, TableRow};

        fn make_cell(text: &str) -> TableCell {
            TableCell {
                content: vec![CellContent::Paragraph(Paragraph {
                    style: StyleRef("Normal".to_string()),
                    runs: vec![Run { text: text.to_string(), ..Default::default() }],
                    list_info: None,
                    ..Default::default()
                })],
                col_span: 1,
                row_span: 1,
                style: None,
                hidden: false,
                bg_color: None,
                border_color: None,
                border_width: None,
                pad_left: None,
                pad_right: None,
                pad_top: None,
                pad_bottom: None,
            }
        }

        let tbl = Table {
            rows: vec![
                TableRow { cells: vec![make_cell("A1"), make_cell("B1")], row_height: None },
                TableRow { cells: vec![make_cell("A2"), make_cell("B2")], row_height: None },
            ],
            col_widths: Vec::new(),
            style_id: None,
            float_pos: None,
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
        };
        let doc = make_doc(vec![BodyElement::Table(tbl)]);

        let bytes = DocxWriter::write(&doc).expect("write failed");
        let parsed = DocxParser::parse(&bytes).expect("roundtrip parse failed");

        assert_eq!(parsed.body.len(), 1);
        let BodyElement::Table(t) = &parsed.body[0] else {
            panic!("expected Table, got {:?}", parsed.body[0]);
        };
        assert_eq!(t.rows.len(), 2);
        assert_eq!(t.rows[0].cells.len(), 2);
        assert_eq!(t.rows[1].cells.len(), 2);

        fn cell_text(cell: &roe_model::common::TableCell) -> String {
            cell.content
                .iter()
                .filter_map(|c| {
                    if let roe_model::common::CellContent::Paragraph(p) = c {
                        Some(p.runs.iter().map(|r| r.text.as_str()).collect::<String>())
                    } else {
                        None
                    }
                })
                .collect()
        }

        assert_eq!(cell_text(&t.rows[0].cells[0]), "A1");
        assert_eq!(cell_text(&t.rows[0].cells[1]), "B1");
        assert_eq!(cell_text(&t.rows[1].cells[0]), "A2");
        assert_eq!(cell_text(&t.rows[1].cells[1]), "B2");
    }

    #[test]
    fn test_roundtrip_table_col_widths_row_height() {
        // 列宽/行高的 pt -> twips -> pt 换算 roundtrip，容许小数舍入误差
        // （pt*20取整再/20，理论上pt为整数或.05的倍数时可精确还原）。
        use roe_model::common::{CellContent, Table, TableCell, TableRow};

        fn make_cell(text: &str) -> TableCell {
            TableCell {
                content: vec![CellContent::Paragraph(Paragraph {
                    style: StyleRef("Normal".to_string()),
                    runs: vec![Run { text: text.to_string(), ..Default::default() }],
                    list_info: None,
                    ..Default::default()
                })],
                col_span: 1,
                row_span: 1,
                style: None,
                hidden: false,
                bg_color: None,
                border_color: None,
                border_width: None,
                pad_left: None,
                pad_right: None,
                pad_top: None,
                pad_bottom: None,
            }
        }

        let tbl = Table {
            rows: vec![TableRow {
                cells: vec![make_cell("X"), make_cell("Y")],
                row_height: Some(30.0),
            }],
            col_widths: vec![100.0, 200.0],
            style_id: None,
            float_pos: None,
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
        };
        let doc = make_doc(vec![BodyElement::Table(tbl)]);

        let bytes = DocxWriter::write(&doc).expect("write failed");
        let parsed = DocxParser::parse(&bytes).expect("roundtrip parse failed");

        let BodyElement::Table(t) = &parsed.body[0] else {
            panic!("expected Table");
        };
        assert_eq!(t.col_widths.len(), 2);
        assert!((t.col_widths[0] - 100.0).abs() < 0.5, "col_widths[0]={}", t.col_widths[0]);
        assert!((t.col_widths[1] - 200.0).abs() < 0.5, "col_widths[1]={}", t.col_widths[1]);

        // 注：docx.rs 解析端目前不读 w:trHeight（TableRow.row_height 在
        // parser 里永远硬编码 None，见 docx.rs 2843行），这是解析端的
        // 已知缺口、不在本批 docx_writer 任务范围内，因此不能用
        // write→reparse 验证 row_height 写出是否正确，改为直接断言
        // write_table_xml 生成的原始 XML 字符串包含预期的 w:trHeight。
        // 30pt * 20 = 600 twips。
        let raw_doc = std::str::from_utf8(&bytes).ok();
        let xml_contains_trheight = {
            // bytes 是 zip 包，不能直接当字符串搜；改为直接调用
            // write_table_xml 生成 XML 片段来断言，绕开 zip 解包。
            true
        };
        let _ = (raw_doc, xml_contains_trheight);

        let tbl_for_xml_check = roe_model::common::Table {
            rows: vec![TableRow {
                cells: vec![],
                row_height: Some(30.0),
            }],
            col_widths: vec![100.0, 200.0],
            style_id: None,
            float_pos: None,
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
        };
        let xml = write_table_xml(&tbl_for_xml_check);
        assert!(
            xml.contains(r#"<w:trHeight w:val="600"/>"#),
            "生成的XML应包含600 twips的trHeight，实际XML: {xml}"
        );
        assert!(
            xml.contains(r#"<w:gridCol w:w="2000"/>"#),
            "生成的XML应包含100pt=2000twips的gridCol，实际XML: {xml}"
        );
        assert!(
            xml.contains(r#"<w:gridCol w:w="4000"/>"#),
            "生成的XML应包含200pt=4000twips的gridCol，实际XML: {xml}"
        );
    }

    #[test]
    fn test_roundtrip_table_style_and_borders() {
        // 表格样式绑定六个bool开关 + 表格级/cell级边框颜色宽度，验证
        // w:tblStyle/w:tblLook/w:tblBorders/w:tcBorders/w:shd 全链路。
        use roe_model::common::{CellContent, Color, Table, TableCell, TableRow};

        let styled_cell = TableCell {
            content: vec![CellContent::Paragraph(Paragraph {
                style: StyleRef("Normal".to_string()),
                runs: vec![Run { text: "Styled".to_string(), ..Default::default() }],
                list_info: None,
                ..Default::default()
            })],
            col_span: 1,
            row_span: 1,
            style: None,
            hidden: false,
            bg_color: Some(Color { r: 0xFF, g: 0xCC, b: 0x00, a: 255 }),
            border_color: Some(Color { r: 0x11, g: 0x22, b: 0x33, a: 255 }),
            border_width: Some(1.5),
            pad_left: None,
            pad_right: None,
            pad_top: None,
            pad_bottom: None,
        };

        let tbl = Table {
            rows: vec![TableRow { cells: vec![styled_cell], row_height: None }],
            col_widths: Vec::new(),
            style_id: Some("TableGrid".to_string()),
            float_pos: None,
            first_row: true,
            first_col: false,
            last_row: false,
            last_col: false,
            band_row: true,
            band_col: false,
            default_border_color: Some(Color { r: 0x00, g: 0x00, b: 0x00, a: 255 }),
            default_border_width: Some(1.0),
            inside_h_color: Some(Color { r: 0x80, g: 0x80, b: 0x80, a: 255 }),
            inside_h_width: Some(0.5),
            inside_v_color: None,
            inside_v_width: None,
        };
        let doc = make_doc(vec![BodyElement::Table(tbl)]);

        let bytes = DocxWriter::write(&doc).expect("write failed");
        let parsed = DocxParser::parse(&bytes).expect("roundtrip parse failed");

        let BodyElement::Table(t) = &parsed.body[0] else {
            panic!("expected Table");
        };

        // 注：docx.rs 解析端目前不读 w:tblStyle/w:tblLook（Table.style_id/
        // first_row/band_row 等六个bool在parser里永远硬编码
        // None/false，见 docx.rs 2761行），这是解析端的已知缺口、不在
        // 本批 docx_writer 任务范围内。cell 级 bg_color/border_color/
        // border_width 则确认能 roundtrip（parser 有读取这几个字段，
        // 见 docx.rs w:shd/w:tcBorders 解析逻辑），按预期断言。
        let cell = &t.rows[0].cells[0];
        assert!(cell.bg_color.is_some(), "bg_color 应被解析回来");
        assert!(cell.border_color.is_some(), "border_color 应被解析回来");
        assert!(cell.border_width.is_some(), "border_width 应被解析回来");

        // 表格级 style_id/first_row/band_row 等：直接断言 write_table_xml
        // 生成的原始 XML，绕开"解析端不读"这个已知缺口。
        let tbl_for_xml_check = roe_model::common::Table {
            rows: vec![],
            col_widths: Vec::new(),
            style_id: Some("TableGrid".to_string()),
            float_pos: None,
            first_row: true,
            first_col: false,
            last_row: false,
            last_col: false,
            band_row: true,
            band_col: false,
            default_border_color: Some(Color { r: 0x00, g: 0x00, b: 0x00, a: 255 }),
            default_border_width: Some(1.0),
            inside_h_color: Some(Color { r: 0x80, g: 0x80, b: 0x80, a: 255 }),
            inside_h_width: Some(0.5),
            inside_v_color: None,
            inside_v_width: None,
        };
        let xml = write_table_xml(&tbl_for_xml_check);
        assert!(
            xml.contains(r#"<w:tblStyle w:val="TableGrid"/>"#),
            "生成的XML应包含tblStyle引用，实际XML: {xml}"
        );
        assert!(
            xml.contains(r#"w:firstRow="1""#),
            "tblLook应标记firstRow=1，实际XML: {xml}"
        );
        assert!(
            xml.contains(r#"w:noHBand="0""#),
            "band_row=true应对应noHBand=0，实际XML: {xml}"
        );
        assert!(
            xml.contains("w:insideH") && xml.contains("808080"),
            "应包含inside_h颜色808080，实际XML: {xml}"
        );
    }

    #[test]
    fn test_roundtrip_empty_body() {
        let doc = make_doc(vec![]);
        let bytes = DocxWriter::write(&doc).expect("write failed");
        let parsed = DocxParser::parse(&bytes).expect("roundtrip parse failed");
        assert_eq!(parsed.body.len(), 0);
    }

    #[test]
    fn test_xml_escape_in_text() {
        let para = Paragraph {
            style: StyleRef("Normal".to_string()),
            runs: vec![Run {
                text: "A & B < C > D".to_string(),
                ..Default::default()
            }],
            list_info: None,
            ..Default::default()
        };
        let doc = make_doc(vec![BodyElement::Paragraph(para)]);

        let bytes = DocxWriter::write(&doc).expect("write failed");
        let parsed = DocxParser::parse(&bytes).expect("roundtrip parse failed");

        match &parsed.body[0] {
            BodyElement::Paragraph(p) => {
                assert_eq!(p.runs[0].text, "A & B < C > D");
            }
            _ => panic!("expected Paragraph"),
        }
    }
    // ─── 真实文件 round-trip 测试（docx round-trip 任务）───────────────────

    /// 比较两个 BodyElement 序列在 writer 支持范围内的字段：仅比较
    /// Paragraph 的 style + runs[].text（trim 后），非 Paragraph 类型
    /// （Table/Section/Image/Frame/Shape/InlineChart）不比较内容，只
    /// 确认数量上"被跳过"这件事本身符合预期——因为 writer 文件头注释
    /// 明确声明这些类型不写，expected（首次 parse 真实文件）里可能有，
    /// actual（write→reparse）里必然没有，这是已知的范围内取舍，不是
    /// bug，比较时应主动跳过而非断言相等。
    ///
    /// trim 文本原因：docx.rs parser 用 quick-xml trim_text(true)，纯
    /// 空白 run 文字在 parse 阶段就已经被去除首尾空白，这是 parser 现状
    /// 而非 writer 引入的差异，比较时对齐这个已知行为，避免误报。
    fn assert_paragraphs_match_within_writer_scope(
        expected: &[BodyElement],
        actual: &[BodyElement],
        path: &str,
    ) {
        let exp_paras: Vec<&Paragraph> = expected
            .iter()
            .filter_map(|b| if let BodyElement::Paragraph(p) = b { Some(p) } else { None })
            .collect();
        let act_paras: Vec<&Paragraph> = actual
            .iter()
            .filter_map(|b| if let BodyElement::Paragraph(p) = b { Some(p) } else { None })
            .collect();

        assert_eq!(
            exp_paras.len(), act_paras.len(),
            "{path}: paragraph 数量不一致（可能是某个 Paragraph 被误判为其他类型，\
             或者 writer 支持范围外的类型意外产生了 Paragraph）"
        );

        for (i, (exp, act)) in exp_paras.iter().zip(act_paras.iter()).enumerate() {
            let p_path = format!("{path} paragraph[{i}]");

            assert_eq!(
                exp.style.0, act.style.0,
                "{p_path}: style 不一致 exp={:?} act={:?}", exp.style.0, act.style.0
            );

            let exp_text: String = exp.runs.iter().map(|r| r.text.as_str()).collect();
            let act_text: String = act.runs.iter().map(|r| r.text.as_str()).collect();
            assert_eq!(
                exp_text.trim(), act_text.trim(),
                "{p_path}: 拼接后的文字内容不一致（按 run 边界拼接以规避\
                 quick-xml trim_text 对纯空白 run 的已知裁剪行为）\nexp={exp_text:?}\nact={act_text:?}"
            );
        }
    }

    fn run_real_file_roundtrip_docx(path: &str) {
        let data = std::fs::read(path)
            .unwrap_or_else(|e| panic!("读取样本文件失败 {path}: {e}"));
        let expected = DocxParser::parse(&data)
            .unwrap_or_else(|e| panic!("首次 parse 失败 {path}: {e:?}"));

        let bytes = DocxWriter::write(&expected)
            .unwrap_or_else(|e| panic!("write 失败 {path}: {e:?}"));
        let actual = DocxParser::parse(&bytes)
            .unwrap_or_else(|e| panic!("write→reparse 失败 {path}: {e:?}"));

        assert_paragraphs_match_within_writer_scope(&expected.body, &actual.body, path);
    }

    #[test]
    fn test_real_file_roundtrip_comment_done() {
        // 探测脚本 scan_docx_roundtrip_candidates 挑选：3 paragraphs /
        // 314 runs，全 clean（无 table/section/image/frame/shape/chart）。
        // 文件名暗示原本用于测试批注（comments），但 comments 不在
        // docx_writer 范围内，实际效果是"退化成纯文字 run"的高 run 密度
        // 样本，用于压测大量 run 拼接/写出/读回的正确性。
        run_real_file_roundtrip_docx(
            "/home/yong/Projects/roe/test_samples/curated/docx/mid__sw_ooxmlexport_data__CommentDone.docx",
        );
    }

    #[test]
    fn test_real_file_roundtrip_tdf133924() {
        // 探测脚本挑选：3 paragraphs / 6 runs / 0 empty，全 clean，styled
        // paragraphs 全覆盖，作为中等复杂度的主力基线样本。
        run_real_file_roundtrip_docx(
            "/home/yong/Projects/roe/test_samples/curated/docx/small__sw_ooxmlexport_data__tdf133924.docx",
        );
    }

    #[test]
    fn test_real_file_roundtrip_fdo72942_insert() {
        // 探测脚本挑选：3 paragraphs / 10 runs / 1 empty paragraph，全
        // clean，专门覆盖"空段落"（zero-length text）这个边界场景。
        run_real_file_roundtrip_docx(
            "/home/yong/Projects/roe/test_samples/curated/docx/small__sw_uiwriter_data__fdo72942-insert.docx",
        );
    }

    // ─── Run 内联格式写出测试（2026-09-16会话）─────────────────────────────
    //
    // 分两类，与函数头注释的判断标准一致：
    // 1) docx.rs 正文run级解析端支持的字段（bold/italic/strike/color/
    //    background/font_family/font_size/underline bool）：用
    //    write→reparse 验证，真实验证"写出的docx能被自己的parser读回来"。
    // 2) docx.rs 正文run级解析端不支持的字段（underline_style枚举/
    //    script上标下标）：用直接断言 write_run_props_xml 生成的XML
    //    字符串验证"写出"这一半的正确性，不依赖reparse（reparse会因为
    //    parser端缺口而拿不到这些字段，不代表写出代码有问题）。

    #[test]
    fn test_roundtrip_run_bold_italic_strike() {
        use roe_model::writer::*;
        let doc = WriterDocument {
            body: vec![BodyElement::Paragraph(Paragraph {
                runs: vec![
                    Run { text: "bold".into(), bold: Some(true), ..Default::default() },
                    Run { text: "italic".into(), italic: Some(true), ..Default::default() },
                    Run { text: "strike".into(), strikethrough: Some(true), ..Default::default() },
                    Run { text: "plain".into(), ..Default::default() },
                ],
                ..Default::default()
            })],
            ..Default::default()
        };
        let bytes = DocxWriter::write(&doc).expect("write failed");
        let reparsed = DocxParser::parse(&bytes).expect("reparse failed");
        let BodyElement::Paragraph(p) = &reparsed.body[0] else { panic!("expected paragraph") };
        assert_eq!(p.runs[0].text, "bold");
        assert_eq!(p.runs[0].bold, Some(true));
        assert_eq!(p.runs[1].text, "italic");
        assert_eq!(p.runs[1].italic, Some(true));
        assert_eq!(p.runs[2].text, "strike");
        assert_eq!(p.runs[2].strikethrough, Some(true));
        assert_eq!(p.runs[3].text, "plain");
        assert_eq!(p.runs[3].bold, None);
        assert_eq!(p.runs[3].italic, None);
    }

    #[test]
    fn test_roundtrip_run_color_background_fontsize() {
        use roe_model::writer::*;
        use roe_model::common::Color;
        let doc = WriterDocument {
            body: vec![BodyElement::Paragraph(Paragraph {
                runs: vec![
                    Run {
                        text: "styled".into(),
                        color: Some(Color { r: 0x12, g: 0x34, b: 0x56, a: 0xff }),
                        background: Some(Color { r: 0xaa, g: 0xbb, b: 0xcc, a: 0xff }),
                        font_size: Some(14.0),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            })],
            ..Default::default()
        };
        let bytes = DocxWriter::write(&doc).expect("write failed");
        let reparsed = DocxParser::parse(&bytes).expect("reparse failed");
        let BodyElement::Paragraph(p) = &reparsed.body[0] else { panic!("expected paragraph") };
        let r0 = &p.runs[0];
        assert_eq!(r0.text, "styled");
        assert_eq!(r0.color, Some(Color { r: 0x12, g: 0x34, b: 0x56, a: 0xff }));
        assert_eq!(r0.background, Some(Color { r: 0xaa, g: 0xbb, b: 0xcc, a: 0xff }));
        assert_eq!(r0.font_size, Some(14.0));
    }

    #[test]
    fn test_roundtrip_run_font_family() {
        use roe_model::writer::*;
        let doc = WriterDocument {
            body: vec![BodyElement::Paragraph(Paragraph {
                runs: vec![
                    Run {
                        text: "fonted".into(),
                        font_family: Some("宋体".into()),
                        font_family_ascii: Some("Times New Roman".into()),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            })],
            ..Default::default()
        };
        let bytes = DocxWriter::write(&doc).expect("write failed");
        let reparsed = DocxParser::parse(&bytes).expect("reparse failed");
        let BodyElement::Paragraph(p) = &reparsed.body[0] else { panic!("expected paragraph") };
        let r0 = &p.runs[0];
        // docx.rs 解析端 font_family 最终取值是 east_asia.or(ascii)，
        // 所以读回来的 font_family 应该是东亚字体"宋体"。
        assert_eq!(r0.font_family, Some("宋体".to_string()));
        assert_eq!(r0.font_family_ascii, Some("Times New Roman".to_string()));
    }

    #[test]
    fn test_write_run_underline_style_xml_direct() {
        // underline_style 枚举：docx.rs 正文run级解析端不支持四选一还原
        // （只能读回bool），因此不用write→reparse，直接断言生成的XML。
        use roe_model::writer::*;
        let run_single = Run { text: "x".into(), underline_style: Some(UnderlineStyle::Single), ..Default::default() };
        let xml = write_run_props_xml(&run_single);
        assert!(xml.contains(r#"<w:u w:val="single"/>"#), "actual xml: {xml}");

        let run_double = Run { text: "x".into(), underline_style: Some(UnderlineStyle::Double), ..Default::default() };
        let xml = write_run_props_xml(&run_double);
        assert!(xml.contains(r#"<w:u w:val="double"/>"#), "actual xml: {xml}");

        let run_wave = Run { text: "x".into(), underline_style: Some(UnderlineStyle::Wave), ..Default::default() };
        let xml = write_run_props_xml(&run_wave);
        assert!(xml.contains(r#"<w:u w:val="wave"/>"#), "actual xml: {xml}");

        let run_none = Run { text: "x".into(), underline_style: Some(UnderlineStyle::None), ..Default::default() };
        let xml = write_run_props_xml(&run_none);
        assert!(xml.contains(r#"<w:u w:val="none"/>"#), "actual xml: {xml}");

        // underline_style 为 None 时退回 underline(bool)
        let run_bool_true = Run { text: "x".into(), underline: Some(true), ..Default::default() };
        let xml = write_run_props_xml(&run_bool_true);
        assert!(xml.contains(r#"<w:u w:val="single"/>"#), "actual xml: {xml}");

        let run_bool_false = Run { text: "x".into(), underline: Some(false), ..Default::default() };
        let xml = write_run_props_xml(&run_bool_false);
        assert!(xml.contains(r#"<w:u w:val="none"/>"#), "actual xml: {xml}");

        // 两者都为 None 时不写 w:u
        let run_neither = Run { text: "x".into(), ..Default::default() };
        let xml = write_run_props_xml(&run_neither);
        assert!(!xml.contains("<w:u "), "actual xml: {xml}");
    }

    #[test]
    fn test_write_run_script_xml_direct() {
        // ScriptPosition 上标/下标：docx.rs 正文run级完全不解析内联
        // w:vertAlign（只有styles.xml字符样式定义那条独立路径会读），
        // 因此不用write→reparse，直接断言生成的XML。
        use roe_model::writer::*;
        let run_super = Run { text: "x".into(), script: Some(ScriptPosition::Superscript), ..Default::default() };
        let xml = write_run_props_xml(&run_super);
        assert!(xml.contains(r#"<w:vertAlign w:val="superscript"/>"#), "actual xml: {xml}");

        let run_sub = Run { text: "x".into(), script: Some(ScriptPosition::Subscript), ..Default::default() };
        let xml = write_run_props_xml(&run_sub);
        assert!(xml.contains(r#"<w:vertAlign w:val="subscript"/>"#), "actual xml: {xml}");

        let run_normal = Run { text: "x".into(), script: Some(ScriptPosition::Normal), ..Default::default() };
        let xml = write_run_props_xml(&run_normal);
        assert!(!xml.contains("w:vertAlign"), "actual xml: {xml}");

        let run_none = Run { text: "x".into(), script: None, ..Default::default() };
        let xml = write_run_props_xml(&run_none);
        assert!(!xml.contains("w:vertAlign"), "actual xml: {xml}");
    }

    // ─── FloatFrame（图片浮动环绕）写出测试（2026-09-16会话）───────────────
    //
    // 全部字段docx.rs解析端正文路径均有正向+反向验证（含rotation专门的
    // Some/None两态测试），因此全部走write→reparse，不需要"直接断言XML"
    // 这条路径（与Run批次的underline_style/script不同）。

    #[test]
    fn test_roundtrip_frame_pos_offset_basic() {
        use roe_model::writer::*;
        let frame = FloatFrame {
                origin_para_body_index: None,
                image: roe_model::common::ImageData {
                data: vec![0xffu8, 0xd8, 0xff, 0xd9],
                mime: "image/jpeg".to_string(),
                ..Default::default()
            },
                width_pt: 100.0,
                height_pt: 50.0,
                x_pt: 10.0,
                y_pt: 20.0,
                anchor_type_h: Some("page".to_string()),
                anchor_type_v: Some("paragraph".to_string()),
                align_h: None,
                align_v: None,
                wrap_type: None,
                dist_t_pt: 0.0,
                dist_b_pt: 0.0,
                dist_l_pt: 0.0,
                dist_r_pt: 0.0,
                rotation: None,
            };
        let doc = make_doc(vec![BodyElement::Frame(frame)]);
        let bytes = DocxWriter::write(&doc).expect("write failed");
        let reparsed = DocxParser::parse(&bytes).expect("reparse failed");
        let frames: Vec<_> = reparsed.body.iter().filter_map(|b| {
            if let BodyElement::Frame(f) = b { Some(f) } else { None }
        }).collect();
        assert_eq!(frames.len(), 1);
        let f = frames[0];
        assert_eq!(f.width_pt.round(), 100.0);
        assert_eq!(f.height_pt.round(), 50.0);
        assert_eq!(f.x_pt.round(), 10.0);
        assert_eq!(f.y_pt.round(), 20.0);
        assert_eq!(f.anchor_type_h.as_deref(), Some("page"));
        assert_eq!(f.anchor_type_v.as_deref(), Some("paragraph"));
        assert_eq!(f.align_h, None);
        assert_eq!(f.align_v, None);
    }

    #[test]
    fn test_roundtrip_frame_align() {
        use roe_model::writer::*;
        let frame = FloatFrame {
                origin_para_body_index: None,
                image: roe_model::common::ImageData {
                data: vec![0xffu8, 0xd8, 0xff, 0xd9],
                mime: "image/jpeg".to_string(),
                ..Default::default()
            },
                width_pt: 100.0,
                height_pt: 50.0,
                x_pt: 10.0,
                y_pt: 20.0,
                anchor_type_h: Some("margin".to_string()),
                anchor_type_v: Some("margin".to_string()),
                align_h: Some("center".to_string()),
                align_v: Some("top".to_string()),
                wrap_type: None,
                dist_t_pt: 0.0,
                dist_b_pt: 0.0,
                dist_l_pt: 0.0,
                dist_r_pt: 0.0,
                rotation: None,
            };
        let doc = make_doc(vec![BodyElement::Frame(frame)]);
        let bytes = DocxWriter::write(&doc).expect("write failed");
        let reparsed = DocxParser::parse(&bytes).expect("reparse failed");
        let frames: Vec<_> = reparsed.body.iter().filter_map(|b| {
            if let BodyElement::Frame(f) = b { Some(f) } else { None }
        }).collect();
        let f = frames[0];
        assert_eq!(f.anchor_type_h.as_deref(), Some("margin"));
        assert_eq!(f.anchor_type_v.as_deref(), Some("margin"));
        assert_eq!(f.align_h.as_deref(), Some("center"));
        assert_eq!(f.align_v.as_deref(), Some("top"));
    }

    #[test]
    fn test_roundtrip_frame_wrap_types() {
        use roe_model::writer::*;
        for wrap in ["none", "square", "tight", "through", "topAndBottom"] {
            let frame = FloatFrame {
                origin_para_body_index: None,
                image: roe_model::common::ImageData {
                data: vec![0xffu8, 0xd8, 0xff, 0xd9],
                mime: "image/jpeg".to_string(),
                ..Default::default()
            },
                width_pt: 100.0,
                height_pt: 50.0,
                x_pt: 10.0,
                y_pt: 20.0,
                anchor_type_h: None,
                anchor_type_v: None,
                align_h: None,
                align_v: None,
                wrap_type: Some(wrap.to_string()),
                dist_t_pt: 0.0,
                dist_b_pt: 0.0,
                dist_l_pt: 0.0,
                dist_r_pt: 0.0,
                rotation: None,
            };
            let doc = make_doc(vec![BodyElement::Frame(frame)]);
            let bytes = DocxWriter::write(&doc).expect("write failed");
            let reparsed = DocxParser::parse(&bytes).expect("reparse failed");
            let frames: Vec<_> = reparsed.body.iter().filter_map(|b| {
                if let BodyElement::Frame(f) = b { Some(f) } else { None }
            }).collect();
            assert_eq!(frames[0].wrap_type.as_deref(), Some(wrap), "wrap_type mismatch for {wrap}");
        }
    }

    #[test]
    fn test_roundtrip_frame_rotation() {
        use roe_model::writer::*;
        // Some 分支
        let frame_rot = FloatFrame {
                origin_para_body_index: None,
                image: roe_model::common::ImageData {
                data: vec![0xffu8, 0xd8, 0xff, 0xd9],
                mime: "image/jpeg".to_string(),
                ..Default::default()
            },
                width_pt: 100.0,
                height_pt: 50.0,
                x_pt: 10.0,
                y_pt: 20.0,
                anchor_type_h: None,
                anchor_type_v: None,
                align_h: None,
                align_v: None,
                wrap_type: None,
                dist_t_pt: 0.0,
                dist_b_pt: 0.0,
                dist_l_pt: 0.0,
                dist_r_pt: 0.0,
                rotation: Some(45.0),
            };
        let doc = make_doc(vec![BodyElement::Frame(frame_rot)]);
        let bytes = DocxWriter::write(&doc).expect("write failed");
        let reparsed = DocxParser::parse(&bytes).expect("reparse failed");
        let frames: Vec<_> = reparsed.body.iter().filter_map(|b| {
            if let BodyElement::Frame(f) = b { Some(f) } else { None }
        }).collect();
        let rot = frames[0].rotation.expect("rotation应为Some");
        assert!((rot - 45.0).abs() < 0.01, "actual rotation: {rot}");

        // None 分支：不应凭空产生默认值
        let frame_no_rot = FloatFrame {
                origin_para_body_index: None,
                image: roe_model::common::ImageData {
                data: vec![0xffu8, 0xd8, 0xff, 0xd9],
                mime: "image/jpeg".to_string(),
                ..Default::default()
            },
                width_pt: 100.0,
                height_pt: 50.0,
                x_pt: 10.0,
                y_pt: 20.0,
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
            };
        let doc2 = make_doc(vec![BodyElement::Frame(frame_no_rot)]);
        let bytes2 = DocxWriter::write(&doc2).expect("write failed");
        let reparsed2 = DocxParser::parse(&bytes2).expect("reparse failed");
        let frames2: Vec<_> = reparsed2.body.iter().filter_map(|b| {
            if let BodyElement::Frame(f) = b { Some(f) } else { None }
        }).collect();
        assert_eq!(frames2[0].rotation, None);
    }

    #[test]
    fn test_roundtrip_frame_dist_and_size() {
        use roe_model::writer::*;
        let frame = FloatFrame {
                origin_para_body_index: None,
                image: roe_model::common::ImageData {
                data: vec![0xffu8, 0xd8, 0xff, 0xd9],
                mime: "image/jpeg".to_string(),
                ..Default::default()
            },
                width_pt: 200.0,
                height_pt: 150.0,
                x_pt: 10.0,
                y_pt: 20.0,
                anchor_type_h: None,
                anchor_type_v: None,
                align_h: None,
                align_v: None,
                wrap_type: None,
                dist_t_pt: 5.0,
                dist_b_pt: 6.0,
                dist_l_pt: 7.0,
                dist_r_pt: 8.0,
                rotation: None,
            };
        let doc = make_doc(vec![BodyElement::Frame(frame)]);
        let bytes = DocxWriter::write(&doc).expect("write failed");
        let reparsed = DocxParser::parse(&bytes).expect("reparse failed");
        let frames: Vec<_> = reparsed.body.iter().filter_map(|b| {
            if let BodyElement::Frame(f) = b { Some(f) } else { None }
        }).collect();
        let f = frames[0];
        assert_eq!(f.width_pt.round(), 200.0);
        assert_eq!(f.height_pt.round(), 150.0);
        assert!((f.dist_t_pt - 5.0).abs() < 0.1, "dist_t_pt: {}", f.dist_t_pt);
        assert!((f.dist_b_pt - 6.0).abs() < 0.1, "dist_b_pt: {}", f.dist_b_pt);
        assert!((f.dist_l_pt - 7.0).abs() < 0.1, "dist_l_pt: {}", f.dist_l_pt);
        assert!((f.dist_r_pt - 8.0).abs() < 0.1, "dist_r_pt: {}", f.dist_r_pt);
    }

    // ─── Shape 写出测试（2026-09-16会话新增） ─────────────────────────────

    #[test]
    fn test_roundtrip_shape_inline_rect_solid_fill() {
        use roe_model::common::{Color, FillStyle, ShapeGeometry, ShapeStyle};
        use roe_model::writer::Shape;
        let shape = Shape {
            geometry: ShapeGeometry::Rect,
            style: ShapeStyle {
                fill: Some(FillStyle::Solid(Color { r: 0xFF, g: 0x00, b: 0x00, a: 255 })),
                stroke: None,
                shadow: None,
                head_arrow: None,
                tail_arrow: None,
            },
            text_content: vec![],
            width_pt: 100.0,
            height_pt: 50.0,
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
        rotation: None, };
        let doc = make_doc(vec![BodyElement::Shape(shape)]);
        let bytes = DocxWriter::write(&doc).expect("write failed");
        let reparsed = DocxParser::parse(&bytes).expect("reparse failed");
        let shapes: Vec<_> = reparsed.body.iter().filter_map(|b| {
            if let BodyElement::Shape(s) = b { Some(s) } else { None }
        }).collect();
        assert_eq!(shapes.len(), 1, "expected one Shape, body={:?}", reparsed.body);
        let s = shapes[0];
        // 注：docx.rs 解析端对 prst="rect" 走统一的 find_preset_shape 查表
        // 路径（与 v170 摸底记录的 triangle 处理方式一致，非writer bug），
        // 因此 reparse 回来的是 PresetPath{preset_name:"rect"} 而不是
        // ShapeGeometry::Rect 变体本身。ShapeGeometry::Rect 只在 prst
        // 缺失/未识别时由 None 兜底路径产出，不会在 prst="rect" 时产出。
        match &s.geometry {
            roe_model::common::ShapeGeometry::PresetPath(data) => {
                assert_eq!(data.preset_name, "rect");
            }
            other => panic!("expected PresetPath(rect), got {:?}", other),
        }
        assert_eq!(s.width_pt.round(), 100.0);
        assert_eq!(s.height_pt.round(), 50.0);
        match &s.style.fill {
            Some(roe_model::common::FillStyle::Solid(color)) => {
                assert_eq!(color.r, 0xFF);
                assert_eq!(color.g, 0x00);
                assert_eq!(color.b, 0x00);
            }
            other => panic!("expected Solid fill, got {:?}", other),
        }
    }

    #[test]
    fn test_roundtrip_shape_anchor_ellipse_with_stroke() {
        use roe_model::common::{Color, ShapeGeometry, ShapeStyle, StrokeStyle};
        use roe_model::writer::Shape;
        let shape = Shape {
            geometry: ShapeGeometry::Ellipse,
            style: ShapeStyle {
                fill: None,
                stroke: Some(StrokeStyle { color: Color { r: 0x00, g: 0x00, b: 0xFF, a: 255 }, width: 2.0 }),
                shadow: None,
                head_arrow: None,
                tail_arrow: None,
            },
            text_content: vec![],
            width_pt: 80.0,
            height_pt: 80.0,
            x_pt: 15.0,
            y_pt: 25.0,
            anchor_type_h: Some("column".to_string()),
            anchor_type_v: Some("paragraph".to_string()),
            align_h: None,
            align_v: None,
            wrap_type: Some("square".to_string()),
            dist_t_pt: 0.0,
            dist_b_pt: 0.0,
            dist_l_pt: 0.0,
            dist_r_pt: 0.0,
            flip_v: None,
        rotation: None, };
        let doc = make_doc(vec![BodyElement::Shape(shape)]);
        let bytes = DocxWriter::write(&doc).expect("write failed");
        let reparsed = DocxParser::parse(&bytes).expect("reparse failed");
        let shapes: Vec<_> = reparsed.body.iter().filter_map(|b| {
            if let BodyElement::Shape(s) = b { Some(s) } else { None }
        }).collect();
        assert_eq!(shapes.len(), 1, "expected one Shape, body={:?}", reparsed.body);
        let s = shapes[0];
        assert!(matches!(s.geometry, roe_model::common::ShapeGeometry::Ellipse), "geometry={:?}", s.geometry);
        assert_eq!(s.x_pt.round(), 15.0);
        assert_eq!(s.y_pt.round(), 25.0);
        assert_eq!(s.anchor_type_h.as_deref(), Some("column"));
        assert_eq!(s.anchor_type_v.as_deref(), Some("paragraph"));
        match &s.style.stroke {
            Some(stroke) => {
                assert_eq!(stroke.color.b, 0xFF);
                assert!((stroke.width - 2.0).abs() < 0.1, "width={}", stroke.width);
            }
            None => panic!("expected stroke, got None"),
        }
    }

    #[test]
    fn test_roundtrip_shape_preset_path_prst_roundtrip() {
        use roe_model::common::{PresetPathData, ShapeGeometry, ShapeStyle};
        use roe_model::writer::Shape;
        let shape = Shape {
            geometry: ShapeGeometry::PresetPath(PresetPathData {
                commands: vec![],
                preset_name: "heart".to_string(),
                adj_overrides: vec![],
            }),
            style: ShapeStyle {
                fill: None,
                stroke: None,
                shadow: None,
                head_arrow: None,
                tail_arrow: None,
            },
            text_content: vec![],
            width_pt: 40.0,
            height_pt: 40.0,
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
        rotation: None, };
        let doc = make_doc(vec![BodyElement::Shape(shape)]);
        let bytes = DocxWriter::write(&doc).expect("write failed");
        let reparsed = DocxParser::parse(&bytes).expect("reparse failed");
        let shapes: Vec<_> = reparsed.body.iter().filter_map(|b| {
            if let BodyElement::Shape(s) = b { Some(s) } else { None }
        }).collect();
        assert_eq!(shapes.len(), 1, "expected one Shape, body={:?}", reparsed.body);
        match &shapes[0].geometry {
            roe_model::common::ShapeGeometry::PresetPath(data) => {
                assert_eq!(data.preset_name, "heart");
            }
            other => panic!("expected PresetPath, got {:?}", other),
        }
    }

    #[test]
    fn test_write_shape_flip_v_xml_direct() {
        use roe_model::common::{ShapeGeometry, ShapeStyle};
        use roe_model::writer::Shape;
        let shape = Shape {
            geometry: ShapeGeometry::Rect,
            style: ShapeStyle { fill: None, stroke: None, shadow: None, head_arrow: None, tail_arrow: None },
            text_content: vec![],
            width_pt: 10.0,
            height_pt: 10.0,
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
            flip_v: Some(true),
        rotation: None, };
        let xml = write_shape_xml(&shape);
        assert!(xml.contains(r#"flipV="1""#), "expected flipV attr, xml={xml}");
    }

    #[test]
    fn test_roundtrip_shape_with_wrap_anchor_fields() {
        // 摸底发现（Writer接口开发设计文档3.4节起点）：write_shape_xml 的
        // anchor/wrap写出逻辑（is_anchor判断+anchor_type_h/v+align_h/v+
        // wrap_type+dist_t/b/l/r_pt）和docx.rs解析端wp:anchor分支（与
        // FloatFrame共享同一套anchor_relative_from_h/v等变量）实际上早已
        // 存在，此前只是从未有一个测试真正验证过这条roundtrip路径——已有
        // 三个Shape测试（PresetPath/flip_v/textbox_content）全部把环绕
        // 字段设为None/0.0，从未覆盖anchor场景。本测试补上这个验证空白，
        // 而非新增功能代码。参数组合参照FloatFrame同类roundtrip测试
        // （7514行附近，anchor_type_h=Some("page")那组）。
        use roe_model::common::{ShapeGeometry, ShapeStyle};
        use roe_model::writer::Shape;
        let shape = Shape {
            geometry: ShapeGeometry::Rect,
            style: ShapeStyle { fill: None, stroke: None, shadow: None, head_arrow: None, tail_arrow: None },
            text_content: vec![],
            width_pt: 100.0,
            height_pt: 50.0,
            x_pt: 72.0,
            y_pt: 36.0,
            anchor_type_h: Some("page".to_string()),
            anchor_type_v: Some("paragraph".to_string()),
            align_h: None,
            align_v: None,
            wrap_type: Some("square".to_string()),
            dist_t_pt: 5.0,
            dist_b_pt: 5.0,
            dist_l_pt: 10.0,
            dist_r_pt: 10.0,
            flip_v: None,
        rotation: None, };
        let doc = make_doc(vec![BodyElement::Shape(shape)]);
        let bytes = DocxWriter::write(&doc).expect("write failed");
        let reparsed = DocxParser::parse(&bytes).expect("reparse failed");
        let shapes: Vec<_> = reparsed.body.iter().filter_map(|b| {
            if let BodyElement::Shape(s) = b { Some(s) } else { None }
        }).collect();
        assert_eq!(shapes.len(), 1, "expected one Shape, body={:?}", reparsed.body);
        let s = &shapes[0];
        assert_eq!(s.anchor_type_h.as_deref(), Some("page"), "anchor_type_h roundtrip failed");
        assert_eq!(s.anchor_type_v.as_deref(), Some("paragraph"), "anchor_type_v roundtrip failed");
        assert_eq!(s.wrap_type.as_deref(), Some("square"), "wrap_type roundtrip failed");
        assert!((s.dist_t_pt - 5.0).abs() < 0.5, "dist_t_pt roundtrip failed, got {}", s.dist_t_pt);
        assert!((s.dist_b_pt - 5.0).abs() < 0.5, "dist_b_pt roundtrip failed, got {}", s.dist_b_pt);
        assert!((s.dist_l_pt - 10.0).abs() < 0.5, "dist_l_pt roundtrip failed, got {}", s.dist_l_pt);
        assert!((s.dist_r_pt - 10.0).abs() < 0.5, "dist_r_pt roundtrip failed, got {}", s.dist_r_pt);
        assert!((s.x_pt - 72.0).abs() < 0.5, "x_pt roundtrip failed, got {}", s.x_pt);
        assert!((s.y_pt - 36.0).abs() < 0.5, "y_pt roundtrip failed, got {}", s.y_pt);
        assert!((s.width_pt - 100.0).abs() < 0.5, "width_pt roundtrip failed, got {}", s.width_pt);
        assert!((s.height_pt - 50.0).abs() < 0.5, "height_pt roundtrip failed, got {}", s.height_pt);
    }

    #[test]
    fn test_roundtrip_shape_with_textbox_content() {
        use roe_model::common::{ShapeGeometry, ShapeStyle};
        use roe_model::writer::{Shape, Paragraph, Run};
        use roe_model::common::StyleRef;
        let para = Paragraph {
            style: StyleRef("Normal".to_string()),
            runs: vec![Run { text: "Hello Shape".to_string(), ..Default::default() }],
            list_info: None,
            ..Default::default()
        };
        let shape = Shape {
            geometry: ShapeGeometry::Rect,
            style: ShapeStyle { fill: None, stroke: None, shadow: None, head_arrow: None, tail_arrow: None },
            text_content: vec![para],
            width_pt: 120.0,
            height_pt: 60.0,
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
        rotation: None, };
        let doc = make_doc(vec![BodyElement::Shape(shape)]);
        let bytes = DocxWriter::write(&doc).expect("write failed");
        let reparsed = DocxParser::parse(&bytes).expect("reparse failed");
        let shapes: Vec<_> = reparsed.body.iter().filter_map(|b| {
            if let BodyElement::Shape(s) = b { Some(s) } else { None }
        }).collect();
        assert_eq!(shapes.len(), 1, "expected one Shape, body={:?}", reparsed.body);
        let s = shapes[0];
        assert_eq!(s.text_content.len(), 1, "text_content={:?}", s.text_content);
        let joined: String = s.text_content[0].runs.iter().map(|r| r.text.clone()).collect();
        assert_eq!(joined, "Hello Shape");
    }

}


#[cfg(test)]
mod doc_defaults_write_tests {
    use super::write_doc_defaults_xml;
    use roe_model::style::StyleRegistry;

    #[test]
    fn doc_defaults_xml_has_spacing_and_size() {
        let mut s = StyleRegistry::default();
        s.doc_default_space_after = Some(10.0);
        s.doc_default_line_height = Some(1.15);
        s.doc_default_font_size = Some(11.0);
        let x = write_doc_defaults_xml(&s);
        assert!(x.starts_with("<w:docDefaults>"), "{x}");
        assert!(x.contains(r#"w:after="200""#), "{x}");
        assert!(x.contains(r#"w:line="276""#), "{x}");
        assert!(x.contains(r#"<w:sz w:val="22"/>"#), "{x}");
        assert_eq!(write_doc_defaults_xml(&StyleRegistry::default()), "");
    }
}
