use astra_emu_abi::*;
use rfvp::script::parser::Nls;

const fn text(value: &'static [u8]) -> Bytes { Bytes { data: value.as_ptr(), len: value.len() } }
struct StaticBytes<const N: usize>([Bytes; N]);
// All pointers reference immutable library-lifetime byte strings.
unsafe impl<const N: usize> Sync for StaticBytes<N> {}
static ENCODINGS: StaticBytes<3> = StaticBytes([text(b"shift_jis"), text(b"gbk"), text(b"utf8")]);
static ENCODING_LABELS: StaticBytes<3> = StaticBytes([
    text("Shift-JIS（日文原版）".as_bytes()), text("GBK（简体中文）".as_bytes()), text(b"UTF-8"),
]);
static FORMATS: StaticBytes<1> = StaticBytes([text(b"fvp.hcb")]);
struct Schema([ConfigField; 3]);
// Schema pointers reference only immutable static storage above.
unsafe impl Sync for Schema {}
const fn path_field(id: &'static [u8], label: &'static str, description: &'static str) -> ConfigField {
    ConfigField {
        id: text(id), label: text(label.as_bytes()), group: text("语言".as_bytes()),
        default_value: ConfigValue { kind: VALUE_STRING, reserved: 0, integer: 0, number: 0.0, text: Bytes::EMPTY },
        integer_min: 0, integer_max: 0, number_min: 0.0, number_max: 0.0,
        max_bytes: 4096, flags: CONFIG_FLAG_FILE_PATH | CONFIG_FLAG_GAME_RELATIVE,
        choices: std::ptr::null(), choice_count: 0, description: text(description.as_bytes()),
        choice_labels: std::ptr::null(), step: 0.0, unit: Bytes::EMPTY,
    }
}
static SCHEMA: Schema = Schema([
    ConfigField {
        id: text(b"script_encoding"), label: text("脚本编码".as_bytes()), group: text("脚本".as_bytes()),
        default_value: ConfigValue { kind: VALUE_ENUM, reserved: 0, integer: 0, number: 0.0, text: text(b"shift_jis") },
        integer_min: 0, integer_max: 0, number_min: 0.0, number_max: 0.0,
        max_bytes: 0, flags: 0, choices: ENCODINGS.0.as_ptr(), choice_count: 3,
        description: text("读取游戏脚本文本时使用的字符编码，日文原版选 Shift-JIS，简体中文汉化版通常选 GBK。".as_bytes()),
        choice_labels: ENCODING_LABELS.0.as_ptr(), step: 0.0, unit: Bytes::EMPTY,
    },
    path_field(b"translation_table", "翻译对照表",
        "按原脚本偏移替换对白文本的翻译表文件，路径相对游戏目录，留空则显示原文。"),
    path_field(b"subtitle_map", "视频字幕映射",
        "为游戏视频叠加 ASS 字幕的 JSON 映射文件，路径相对游戏目录，留空则不显示字幕。"),
]);

pub const RENDER: RenderExtentSupport = RenderExtentSupport {
    min: Extent { width: 160, height: 90 },
    max: Extent { width: 8192, height: 8192 },
    default: Extent { width: 0, height: 0 },
    flags: RENDER_PRESERVE_ASPECT,
    reserved: 0,
};
pub const CAPABILITIES: u64 = CAP_CPU_SURFACE | CAP_RENDER_EXTENT;

pub fn descriptor() -> Descriptor {
    Descriptor { header: Header::of::<Descriptor>(), family_id: text(b"fvp"),
        plugin_id: text(b"astra.emu.fvp"), version: text(b"0.6.0"),
        capabilities: CAPABILITIES,
        formats: FORMATS.0.as_ptr(), format_count: FORMATS.0.len(),
        configuration: SCHEMA.0.as_ptr(), configuration_count: SCHEMA.0.len(),
        display_name: text(b"RFVP"), family_name: text(b"FVP"), render: RENDER }
}

pub struct EmbeddedConfig {
    pub nls: Nls,
    pub translation_table: String,
    pub subtitle_map: String,
}

pub unsafe fn parse(request: &OpenRequest) -> anyhow::Result<EmbeddedConfig> {
    let values = unsafe { borrowed_array(request.configuration, request.configuration_count, MAX_CONFIG_FIELDS) }
        .map_err(|_| anyhow::anyhow!("RFVP_CONFIG_STORAGE_INVALID"))?;
    unsafe { validate_config(&SCHEMA.0, values) }
        .map_err(|error| anyhow::anyhow!("RFVP_CONFIG_INVALID entry {}: {}", error.index, error.reason))?;
    let mut nls = Nls::ShiftJIS;
    let mut translation_table = String::new();
    let mut subtitle_map = String::new();
    for entry in values {
        match unsafe { entry.id.text(128) }.map_err(|_| anyhow::anyhow!("RFVP_CONFIG_ID_INVALID"))? {
            "script_encoding" => nls = unsafe { entry.value.text.text(128) }
                .map_err(|_| anyhow::anyhow!("RFVP_CONFIG_ENCODING_INVALID"))?.parse()?,
            "translation_table" => translation_table = unsafe { entry.value.text.text(4096) }
                .map_err(|_| anyhow::anyhow!("RFVP_CONFIG_TRANSLATION_INVALID"))?.to_owned(),
            "subtitle_map" => subtitle_map = unsafe { entry.value.text.text(4096) }
                .map_err(|_| anyhow::anyhow!("RFVP_CONFIG_SUBTITLE_INVALID"))?.to_owned(),
            _ => unreachable!("validated configuration key"),
        }
    }
    Ok(EmbeddedConfig { nls, translation_table, subtitle_map })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translation_table_is_optional_bounded_game_relative_file() {
        let field = &SCHEMA.0[1];
        assert_eq!(unsafe { field.id.text(128) }.unwrap(), "translation_table");
        assert_eq!(field.max_bytes, 4096);
        assert_eq!(field.flags, CONFIG_FLAG_FILE_PATH | CONFIG_FLAG_GAME_RELATIVE);
        assert_eq!(unsafe { field.default_value.text.text(4096) }.unwrap(), "");
        let path = vec![b'a'; 4096];
        let entry = ConfigEntry { id: text(b"translation_table"), value: ConfigValue {
            kind: VALUE_STRING, reserved: 0, integer: 0, number: 0.0,
            text: Bytes { data: path.as_ptr(), len: path.len() },
        }};
        unsafe { validate_config(&SCHEMA.0, &[entry]) }.unwrap();
        let oversized = vec![b'a'; 4097];
        let invalid = ConfigEntry { value: ConfigValue { text: Bytes {
            data: oversized.as_ptr(), len: oversized.len(),
        }, ..entry.value }, ..entry };
        assert!(unsafe { validate_config(&SCHEMA.0, &[invalid]) }.is_err());
    }

    #[test]
    fn persisted_identity_and_encoding_remain_native() {
        let descriptor = descriptor();
        unsafe { descriptor.validate() }.unwrap();
        assert_eq!(unsafe { descriptor.plugin_id.text(128) }.unwrap(), "astra.emu.fvp");
        assert_eq!(unsafe { descriptor.display_name.text(64) }.unwrap(), "RFVP");
        assert_eq!(unsafe { descriptor.family_name.text(64) }.unwrap(), "FVP");
        assert_eq!(unsafe { SCHEMA.0[0].id.text(128) }.unwrap(), "script_encoding");
        assert_eq!(unsafe { SCHEMA.0[0].default_value.text.text(128) }.unwrap(), "shift_jis");
        let labels = unsafe { std::slice::from_raw_parts(SCHEMA.0[0].choice_labels, SCHEMA.0[0].choice_count) };
        assert_eq!(unsafe { labels[1].text(64) }.unwrap(), "GBK（简体中文）");
        unsafe { validate_config(&SCHEMA.0, &[]) }.unwrap();
    }

    #[test]
    fn removed_render_fields_are_unknown() {
        let value = ConfigValue { kind: VALUE_NUMBER, reserved: 0, integer: 0, number: 1.5, text: Bytes::EMPTY };
        for id in [b"render_scale".as_slice(), b"surface"] {
            assert!(unsafe { validate_config(&SCHEMA.0, &[ConfigEntry { id: text(id), value }]) }.is_err());
        }
    }

    #[test]
    fn render_extent_declaration_bounds_requests() {
        RENDER.validate(CAPABILITIES).unwrap();
        RENDER.validate_request(CAPABILITIES, Extent { width: 0, height: 0 }).unwrap();
        RENDER.validate_request(CAPABILITIES, Extent { width: 1920, height: 1080 }).unwrap();
        assert_eq!(RENDER.validate_request(CAPABILITIES, Extent { width: 100, height: 1080 }), Err(INVALID_ARGUMENT));
        assert_eq!(RENDER.validate_request(CAPABILITIES, Extent { width: 1920, height: 8193 }), Err(INVALID_ARGUMENT));
    }

    #[test]
    fn render_extent_maps_to_clamped_uniform_scale() {
        use rfvp::app::embedded::raster_size;
        assert_eq!(raster_size((800, 600), (0, 0)).unwrap(), (800, 600));
        assert_eq!(raster_size((800, 600), (1920, 1080)).unwrap(), (1440, 1080));
        assert_eq!(raster_size((800, 600), (160, 90)).unwrap(), (200, 150));
        assert_eq!(raster_size((1280, 720), (8192, 8192)).unwrap(), (5120, 2880));
        assert_eq!(raster_size((640, 480), (8192, 8192)).unwrap(), (2560, 1920));
        assert!(raster_size((40000, 40000), (160, 90)).is_err());
    }
}
