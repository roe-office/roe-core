/// 字体注册中心，桌面端加载系统字体，WASM 端由前端注入 blob
pub struct FontRegistry {
    // cosmic_text::FontSystem 在 roe-writer 里持有，
    // 这里只管 fallback 链和 blob 注册接口
    pub fallback_chain: Vec<String>,
}

impl Default for FontRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl FontRegistry {
    pub fn new() -> Self {
        Self { fallback_chain: Vec::new() }
    }

    /// WASM 端：前端注入字体 blob
    pub fn register_font_blob(&mut self, name: &str, _data: Vec<u8>) {
        self.fallback_chain.push(name.to_string());
        // TODO: 接入 cosmic-text FontSystem
    }
}
