use super::{Font, FontEnumerator};
use anyhow::{anyhow, bail, Result};

#[derive(Debug)]
pub struct SystemFontFace {
    pub family_name: String,
    pub bytes: Vec<u8>,
    pub face_index: u32,
}

#[derive(Debug, Default)]
pub struct SystemFontBindings {
    pub ms_gothic: Option<SystemFontFace>,
    pub ms_mincho: Option<SystemFontFace>,
    pub ms_pgothic: Option<SystemFontFace>,
    pub ms_pmincho: Option<SystemFontFace>,
}

fn parse(face: Option<SystemFontFace>, expected: &str, required: bool) -> Result<Option<Font>> {
    let Some(face) = face else {
        if required {
            bail!("required system font face {expected} is unavailable");
        }
        return Ok(None);
    };
    if face.family_name != expected {
        bail!("system font family does not match {expected}");
    }
    Font::from_vec_and_index(face.bytes, face.face_index)
        .map(Some)
        .map_err(|_| anyhow!("system font face {expected} is invalid"))
}

impl FontEnumerator {
    pub fn from_system_font_bindings(bindings: SystemFontBindings) -> Result<Self> {
        let mut fonts = Self::new();
        fonts.sys_ms_gothic = parse(bindings.ms_gothic, "MS Gothic", true)?
            .ok_or_else(|| anyhow!("required system font face MS Gothic is unavailable"))?;
        if let Some(font) = parse(bindings.ms_mincho, "MS Mincho", false)? {
            fonts.sys_ms_mincho = font;
        }
        if let Some(font) = parse(bindings.ms_pgothic, "MS PGothic", false)? {
            fonts.sys_ms_pgothic = font;
        }
        if let Some(font) = parse(bindings.ms_pmincho, "MS PMincho", false)? {
            fonts.sys_ms_pmincho = font;
        }
        fonts.default_font = fonts.sys_ms_gothic.clone();
        Ok(fonts)
    }
}
