use serde_json::Value;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeToken(pub u64);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SuspendReason {
    FontMissing   { font_name: String, fallbacks: Vec<String> },
    PasswordRequired { document_id: String },
    FormatConflict   { detail: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum KernelEvent {
    RenderUpdate(Vec<u8>),   // tile diff，格式待定
    IntentSuspended {
        reason: SuspendReason,
        context: Value,
        resume_token: ResumeToken,
    },
}
