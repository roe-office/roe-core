use std::io::{Read, Write};
use crate::ParseError;

pub struct KaiCodec;

impl KaiCodec {
    pub fn decompress(data: &[u8]) -> Result<Vec<u8>, ParseError> {
        let mut decoder = zstd::Decoder::new(data)?;
        let mut out = Vec::new();
        decoder.read_to_end(&mut out)?;
        Ok(out)
    }

    pub fn compress(data: &[u8]) -> Result<Vec<u8>, ParseError> {
        let mut encoder = zstd::Encoder::new(Vec::new(), 19)?;
        encoder.write_all(data)?;
        Ok(encoder.finish()?)
    }

    pub fn detect_subtype(path: &str) -> Option<KaiSubtype> {
        match path.rsplit('.').next()? {
            "wai" => Some(KaiSubtype::Wai),
            "sai" => Some(KaiSubtype::Sai),
            "pai" => Some(KaiSubtype::Pai),
            "kai" => Some(KaiSubtype::Kai),
            _     => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum KaiSubtype { Wai, Sai, Pai, Kai }
