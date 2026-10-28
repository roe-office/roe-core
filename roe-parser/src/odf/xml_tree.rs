//! 共享 XNode 树形 XML 解析工具，供 fodp.rs / fods.rs 等共用

use std::collections::HashMap;
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use crate::ParseError;

#[derive(Default)]
pub struct XNode {
    pub tag: String,
    pub attrs: HashMap<String, String>,
    pub children: Vec<XNode>,
    pub text: String,
}

impl XNode {
    pub fn attr(&self, key: &str) -> Option<&str> {
        self.attrs.get(key).map(|s| s.as_str())
    }
    pub fn first_child_tag(&self, tag: &str) -> Option<&XNode> {
        self.children.iter().find(|c| c.tag == tag)
    }
    pub fn children_tag(&self, tag: &str) -> Vec<&XNode> {
        self.children.iter().filter(|c| c.tag == tag).collect()
    }
    pub fn descendants_tag<'a>(&'a self, tag: &str, out: &mut Vec<&'a XNode>) {
        for c in &self.children {
            if c.tag == tag { out.push(c); }
            c.descendants_tag(tag, out);
        }
    }

    pub fn all_text(&self) -> String {
        let mut s = self.text.clone();
        for c in &self.children {
            s.push_str(&c.all_text());
        }
        s
    }
}

pub fn make_node(e: &quick_xml::events::BytesStart) -> XNode {
    let tag = String::from_utf8_lossy(e.name().as_ref()).into_owned();
    let mut attrs = HashMap::new();
    for a in e.attributes().flatten() {
        let key = String::from_utf8_lossy(a.key.as_ref()).into_owned();
        let val = String::from_utf8_lossy(&a.value).into_owned();
        attrs.insert(key, val);
    }
    XNode { tag, attrs, children: Vec::new(), text: String::new() }
}

pub fn parse_tree(xml: &str) -> Result<XNode, ParseError> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut stack: Vec<XNode> = vec![XNode {
        tag: "#root".to_string(),
        ..Default::default()
    }];
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => { stack.push(make_node(e)); }
            Ok(Event::Empty(ref e)) => {
                let node = make_node(e);
                if let Some(parent) = stack.last_mut() { parent.children.push(node); }
            }
            Ok(Event::End(_)) => {
                if stack.len() > 1 {
                    let node = stack.pop().unwrap();
                    if let Some(parent) = stack.last_mut() { parent.children.push(node); }
                }
            }
            Ok(Event::Text(ref e)) => {
                if let Some(top) = stack.last_mut() {
                    let s = e.unescape().unwrap_or_default();
                    top.text.push_str(&s);
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(ParseError::Xml(e)),
            _ => {}
        }
        buf.clear();
    }
    Ok(stack.pop().unwrap())
}
