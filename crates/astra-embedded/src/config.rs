use astra_emu_abi::*;
use rfvp::script::parser::Nls;

const fn text(value: &'static [u8]) -> Bytes { Bytes { data: value.as_ptr(), len: value.len() } }
struct StaticBytes<const N: usize>([Bytes; N]);
// All pointers reference immutable library-lifetime byte strings.
unsafe impl<const N: usize> Sync for StaticBytes<N> {}
static ENCODINGS: StaticBytes<3> = StaticBytes([text(b"shift_jis"), text(b"gbk"), text(b"utf8")]);
static FORMATS: StaticBytes<1> = StaticBytes([text(b"fvp.hcb")]);
#[cfg(windows)]
static SURFACES: StaticBytes<3> = StaticBytes([text(b"cpu"), text(b"d3d12"), text(b"cpu_d3d12")]);
#[cfg(not(windows))]
static SURFACES: StaticBytes<1> = StaticBytes([text(b"cpu")]);
struct Schema([ConfigField; 4]);
// Schema pointers reference only immutable static storage above.
unsafe impl Sync for Schema {}
static SCHEMA: Schema = Schema([
    ConfigField {
        id: text(b"render_scale"), label: text(b"Render scale"), group: text(b"Rendering"),
        default_value: ConfigValue { kind: VALUE_NUMBER, reserved: 0, integer: 0, number: 1.0, text: Bytes::EMPTY },
        integer_min: 0, integer_max: 0, number_min: 0.25, number_max: 4.0,
        max_bytes: 0, reserved: 0, choices: std::ptr::null(), choice_count: 0,
    },
    ConfigField {
        id: text(b"script_encoding"), label: text(b"Script encoding"), group: text(b"Script"),
        default_value: ConfigValue { kind: VALUE_ENUM, reserved: 0, integer: 0, number: 0.0, text: text(b"shift_jis") },
        integer_min: 0, integer_max: 0, number_min: 0.0, number_max: 0.0,
        max_bytes: 0, reserved: 0, choices: ENCODINGS.0.as_ptr(), choice_count: 3,
    },
    ConfigField {
        id: text(b"surface"), label: text(b"Frame delivery"), group: text(b"Rendering"),
        default_value: ConfigValue { kind: VALUE_ENUM, reserved: 0, integer: 0, number: 0.0, text: text(b"cpu") },
        integer_min: 0, integer_max: 0, number_min: 0.0, number_max: 0.0,
        max_bytes: 0, reserved: 0, choices: SURFACES.0.as_ptr(), choice_count: SURFACES.0.len(),
    },
    ConfigField {
        id: text(b"translation_table"), label: text(b"Original-offset translations"), group: text(b"Script"),
        default_value: ConfigValue { kind: VALUE_STRING, reserved: 0, integer: 0, number: 0.0, text: Bytes::EMPTY },
        integer_min: 0, integer_max: 0, number_min: 0.0, number_max: 0.0,
        max_bytes: 4096, reserved: 0, choices: std::ptr::null(), choice_count: 0,
    },
]);

pub fn descriptor() -> Descriptor {
    Descriptor { header: Header::of::<Descriptor>(), family_id: text(b"fvp"),
        plugin_id: text(b"astra.emu.fvp"), version: text(b"0.6.0"),
        capabilities: CAP_CPU_SURFACE | if cfg!(windows) { CAP_GPU_SURFACE } else { 0 },
        formats: FORMATS.0.as_ptr(), format_count: FORMATS.0.len(),
        configuration: SCHEMA.0.as_ptr(), configuration_count: SCHEMA.0.len() }
}

pub struct EmbeddedConfig {
    pub nls: Nls,
    pub render_scale: f64,
    pub surface: String,
    pub translation_table: String,
}

pub unsafe fn parse(request: &OpenRequest) -> anyhow::Result<EmbeddedConfig> {
    let values = unsafe { borrowed_array(request.configuration, request.configuration_count, MAX_CONFIG_FIELDS) }
        .map_err(|_| anyhow::anyhow!("RFVP_CONFIG_STORAGE_INVALID"))?;
    unsafe { validate_config(&SCHEMA.0, values) }
        .map_err(|error| anyhow::anyhow!("RFVP_CONFIG_INVALID entry {}: {}", error.index, error.reason))?;
    let mut nls = Nls::ShiftJIS;
    let mut scale = 1.0;
    let mut surface = "cpu".to_owned();
    let mut translation_table = String::new();
    for entry in values {
        match unsafe { entry.id.text(128) }.map_err(|_| anyhow::anyhow!("RFVP_CONFIG_ID_INVALID"))? {
            "render_scale" => scale = entry.value.number,
            "script_encoding" => nls = unsafe { entry.value.text.text(128) }
                .map_err(|_| anyhow::anyhow!("RFVP_CONFIG_ENCODING_INVALID"))?.parse()?,
            "surface" => surface = unsafe { entry.value.text.text(128) }
                .map_err(|_| anyhow::anyhow!("RFVP_CONFIG_SURFACE_INVALID"))?.to_owned(),
            "translation_table" => translation_table = unsafe { entry.value.text.text(4096) }
                .map_err(|_| anyhow::anyhow!("RFVP_CONFIG_TRANSLATION_INVALID"))?.to_owned(),
            _ => unreachable!("validated configuration key"),
        }
    }
    Ok(EmbeddedConfig { nls, render_scale: scale, surface, translation_table })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translation_table_is_optional_bounded_string() {
        let field = &SCHEMA.0[3];
        assert_eq!(unsafe { field.id.text(128) }.unwrap(), "translation_table");
        assert_eq!(field.max_bytes, 4096);
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
        assert_eq!(unsafe { descriptor.plugin_id.text(128) }.unwrap(), "astra.emu.fvp");
        assert_eq!(unsafe { SCHEMA.0[1].id.text(128) }.unwrap(), "script_encoding");
        assert_eq!(unsafe { SCHEMA.0[1].default_value.text.text(128) }.unwrap(), "shift_jis");
        unsafe { validate_config(&SCHEMA.0, &[]) }.unwrap();
    }

    #[test]
    fn render_configuration_rejects_unknown_duplicate_and_invalid_values() {
        let value = ConfigValue { kind: VALUE_NUMBER, reserved: 0, integer: 0, number: 1.5, text: Bytes::EMPTY };
        let valid = ConfigEntry { id: text(b"render_scale"), value };
        unsafe { validate_config(&SCHEMA.0, &[valid]) }.unwrap();
        assert!(unsafe { validate_config(&SCHEMA.0, &[valid, valid]) }.is_err());
        assert!(unsafe { validate_config(&SCHEMA.0, &[ConfigEntry { id: text(b"unknown"), value }]) }.is_err());
        for number in [f64::NAN, 0.0, 4.1] {
            let invalid = ConfigEntry { id: valid.id, value: ConfigValue { number, ..value } };
            assert!(unsafe { validate_config(&SCHEMA.0, &[invalid]) }.is_err());
        }
    }
}
