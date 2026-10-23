#![cfg(all(not(feature = "no_std"), feature = "zlib-flate2"))]
use std::io::Write;
use flate2::{write::ZlibEncoder, Compression};
use rfvp::script::parser::Nls;
use rfvp::subsystem::resources::{texture::{NvsgTexture, TextureType}, vfs::Vfs};

fn image(bgra: &[u8; 8]) -> Vec<u8> {
    let mut bytes = vec![0; 44];
    bytes[..4].copy_from_slice(b"hzc1");
    bytes[4..8].copy_from_slice(&8u32.to_le_bytes());
    bytes[8..12].copy_from_slice(&32u32.to_le_bytes());
    bytes[12..16].copy_from_slice(b"NVSG");
    bytes[18..20].copy_from_slice(&1u16.to_le_bytes());
    bytes[20..22].copy_from_slice(&2u16.to_le_bytes());
    bytes[22..24].copy_from_slice(&1u16.to_le_bytes());
    bytes[32..36].copy_from_slice(&1u32.to_le_bytes());
    let mut compressed = ZlibEncoder::new(Vec::new(), Compression::default());
    compressed.write_all(bgra).unwrap();
    bytes.extend(compressed.finish().unwrap());
    bytes
}
fn pack(payload: &[u8]) -> Vec<u8> {
    let name = b"public\0";
    let offset = 8 + 12 + name.len();
    let mut bytes = Vec::new();
    for value in [1u32, name.len() as u32, 0, offset as u32, payload.len() as u32] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes.extend_from_slice(name);
    bytes.extend_from_slice(payload);
    bytes
}
fn decoded(bytes: &[u8]) -> Vec<u8> {
    let mut image = NvsgTexture::new("graph/public");
    image.read_texture(bytes, TextureType::is_picture).unwrap();
    assert_eq!((image.get_width(), image.get_height(), image.get_entry_count()), (2,1,1));
    image.get_texture(0).unwrap().into_rgba8().into_raw()
}

#[test]
fn ordinary_remapped_files_override_bin_without_replacing_the_native_reader() {
    let original = image(&[3,2,1,255,6,5,4,128]);
    let translated = image(&[30,20,10,255,60,50,40,64]);
    let mut vfs = Vfs::new(Nls::ShiftJIS).unwrap();
    vfs.add_pack_bytes("graph", pack(&original)).unwrap();
    assert_eq!(decoded(&vfs.read_file("graph/public").unwrap()), [1,2,3,255,4,5,6,128]);
    vfs.add_loose_file("graph/public", translated);
    assert_eq!(decoded(&vfs.read_file("GRAPH/PUBLIC").unwrap()), [10,20,30,255,40,50,60,64]);
}

#[test]
fn truncated_pack_payload_is_rejected_at_native_entry_bounds() {
    let payload = image(&[3,2,1,255,6,5,4,128]);
    let mut truncated = pack(&payload);
    truncated.pop();
    let mut vfs = Vfs::new(Nls::ShiftJIS).unwrap();
    vfs.add_pack_bytes("graph", truncated).unwrap();
    assert!(vfs.read_file("graph/public").is_err());
}
