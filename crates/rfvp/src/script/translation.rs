//! Read-only original-HCB string identities, paired with the final script hash.
use alloc::{string::String, vec::Vec};
use anyhow::{ensure, Result};
use super::parser::Parser;

#[derive(Debug)]
pub struct TranslationTable {
    pub source_hcb_sha256: [u8; 32],
    records: Vec<(u32, String)>,
}

impl TranslationTable {
    pub(super) fn parse(bytes: &[u8], parser: &Parser, patched_hash: &[u8]) -> Result<Self> {
        ensure!(bytes.len() >= 76 && &bytes[..8] == b"AFVPTR1\0", "RFVP_TRANSLATION_HEADER_INVALID");
        ensure!(&bytes[40..72] == patched_hash, "RFVP_TRANSLATION_HCB_MISMATCH");
        let count = u32::from_le_bytes(bytes[72..76].try_into()?) as usize;
        ensure!(count <= (bytes.len() - 76) / 8, "RFVP_TRANSLATION_COUNT_INVALID");
        let mut records = Vec::with_capacity(count);
        let mut cursor = 76usize;
        let mut code = 4usize;
        let code_end = parser.sys_desc_offset as usize;
        for _ in 0..count {
            let header = bytes.get(cursor..cursor.checked_add(8).ok_or_else(|| anyhow::anyhow!("RFVP_TRANSLATION_BOUNDS"))?)
                .ok_or_else(|| anyhow::anyhow!("RFVP_TRANSLATION_TRUNCATED"))?;
            let offset = u32::from_le_bytes(header[..4].try_into()?);
            let len = u32::from_le_bytes(header[4..8].try_into()?) as usize;
            ensure!(records.last().is_none_or(|(last, _)| *last < offset), "RFVP_TRANSLATION_ORDER_INVALID");
            cursor += 8;
            let end = cursor.checked_add(len).ok_or_else(|| anyhow::anyhow!("RFVP_TRANSLATION_BOUNDS"))?;
            let text = bytes.get(cursor..end).ok_or_else(|| anyhow::anyhow!("RFVP_TRANSLATION_TRUNCATED"))?;
            ensure!(!text.contains(&0), "RFVP_TRANSLATION_NUL");
            let text = core::str::from_utf8(text).map_err(|_| anyhow::anyhow!("RFVP_TRANSLATION_UTF8_INVALID"))?;
            // A preceding 0x0e byte alone is not proof: walk real instruction boundaries.
            let mut found = false;
            while code < code_end {
                let opcode = parser.read_u8(code)?;
                let size = instruction_size(parser, code, code_end)?;
                let payload = code + 2;
                code += size;
                if opcode == 14 && payload == offset as usize { found = true; break; }
                ensure!(code <= offset as usize, "RFVP_TRANSLATION_OFFSET_INVALID");
            }
            ensure!(found, "RFVP_TRANSLATION_OFFSET_INVALID");
            records.push((offset, String::from(text)));
            cursor = end;
        }
        ensure!(cursor == bytes.len(), "RFVP_TRANSLATION_TRAILING_DATA");
        Ok(Self { source_hcb_sha256: bytes[8..40].try_into()?, records })
    }

    pub fn get(&self, offset: u32) -> Option<&str> {
        self.records.binary_search_by_key(&offset, |(key, _)| *key).ok()
            .map(|index| self.records[index].1.as_str())
    }
}

fn instruction_size(parser: &Parser, at: usize, end: usize) -> Result<usize> {
    let opcode = parser.read_u8(at)?;
    let size = match opcode {
        1 | 3 | 11 | 15 | 17 | 21 | 23 => 3,
        2 | 6 | 7 | 10 | 13 => 5,
        12 | 16 | 18 | 22 | 24 => 2,
        14 => {
            ensure!(at + 2 <= end, "RFVP_TRANSLATION_CODE_TRUNCATED");
            let len = parser.read_u8(at + 1)? as usize;
            ensure!(len > 0 && at + 2 + len <= end, "RFVP_TRANSLATION_STRING_BOUNDS");
            ensure!(parser.read_u8(at + 1 + len)? == 0, "RFVP_TRANSLATION_STRING_TERMINATOR");
            2 + len
        }
        0..=39 => 1,
        _ => anyhow::bail!("RFVP_TRANSLATION_OPCODE_INVALID"),
    };
    ensure!(at + size <= end, "RFVP_TRANSLATION_CODE_TRUNCATED");
    Ok(size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use sha2::{Digest, Sha256};
    use crate::script::{context::Context, parser::Nls, Variant};

    fn hcb() -> Vec<u8> {
        let mut bytes = vec![15, 0, 0, 0, 14, 3, 0x82, 0xa0, 0, 14, 3, 0x82, 0xa2, 0, 4];
        bytes.extend_from_slice(&4u32.to_le_bytes());
        bytes.extend_from_slice(&[0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0]);
        bytes
    }

    fn table(hcb: &[u8], records: &[(u32, &[u8])]) -> Vec<u8> {
        let mut bytes = b"AFVPTR1\0".to_vec();
        bytes.extend_from_slice(&Sha256::digest(hcb));
        bytes.extend_from_slice(&Sha256::digest(hcb));
        bytes.extend_from_slice(&(records.len() as u32).to_le_bytes());
        for (offset, text) in records {
            bytes.extend_from_slice(&offset.to_le_bytes());
            bytes.extend_from_slice(&(text.len() as u32).to_le_bytes());
            bytes.extend_from_slice(text);
        }
        bytes
    }

    #[test]
    fn hit_is_utf8_with_original_identity_and_miss_remains_shift_jis() {
        let hcb = hcb();
        let mut parser = Parser::from_bytes(hcb.clone(), Nls::ShiftJIS).unwrap();
        parser.set_translation_table(&table(&hcb, &[(6, "译文".as_bytes())])).unwrap();
        let mut context = Context::new(4, 0);
        context.push_string(&mut parser).unwrap();
        context.push_string(&mut parser).unwrap();
        let snapshot = context.capture_snapshot_v1();
        assert!(matches!(&snapshot.stack[snapshot.cur_stack_base], Variant::ConstString(text, 6) if text == "译文"));
        assert!(matches!(&snapshot.stack[snapshot.cur_stack_base + 1], Variant::ConstString(text, 11) if text == "い"));
        assert_eq!(snapshot.cursor, 14);
        assert!(parser.set_translation_table(&table(&hcb, &[])).is_err());
    }

    #[test]
    fn rejects_wrong_final_hash_duplicates_bad_utf8_nul_and_non_payload_offsets() {
        let hcb = hcb();
        for records in [
            vec![(6, &b"a"[..]), (6, &b"b"[..])],
            vec![(11, &b"a"[..]), (6, &b"b"[..])],
            vec![(7, &b"a"[..])],
            vec![(6, &b"\xff"[..])],
            vec![(6, &b"a\0b"[..])],
        ] {
            let mut parser = Parser::from_bytes(hcb.clone(), Nls::ShiftJIS).unwrap();
            assert!(parser.set_translation_table(&table(&hcb, &records)).is_err());
        }
        let mut wrong_hash = table(&hcb, &[(6, b"x")]);
        wrong_hash[40] ^= 1;
        let mut parser = Parser::from_bytes(hcb.clone(), Nls::ShiftJIS).unwrap();
        assert!(parser.set_translation_table(&wrong_hash).is_err());
        let valid = table(&hcb, &[(6, b"x")]);
        for len in 0..valid.len() {
            assert!(parser.set_translation_table(&valid[..len]).is_err());
        }
        let mut trailing = valid;
        trailing.push(0);
        assert!(parser.set_translation_table(&trailing).is_err());
    }

    #[test]
    fn rejects_fake_push_string_inside_an_operand() {
        let mut hcb = hcb();
        hcb[4..9].copy_from_slice(&[10, 14, 2, b'x', 0]);
        let mut parser = Parser::from_bytes(hcb.clone(), Nls::ShiftJIS).unwrap();
        assert!(parser.set_translation_table(&table(&hcb, &[(7, b"fake")])).is_err());
    }
}

#[cfg(test)]
mod final_identity_tests {
    use super::*;
    use alloc::vec;
    use sha2::{Digest, Sha256};
    use crate::script::parser::Nls;

    #[test]
    fn appended_script_uses_final_hash_without_moving_original_payloads() {
        let mut original = vec![9, 0, 0, 0, 14, 3, b'a', b'b', 0];
        original.extend_from_slice(&4u32.to_le_bytes());
        original.extend_from_slice(&[0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0]);
        let mut final_hcb = original.clone();
        final_hcb.extend_from_slice(&[6, 4, 0, 0, 0]);
        let mut table = b"AFVPTR1\0".to_vec();
        table.extend_from_slice(&Sha256::digest(&original));
        table.extend_from_slice(&Sha256::digest(&final_hcb));
        table.extend_from_slice(&1u32.to_le_bytes());
        table.extend_from_slice(&6u32.to_le_bytes());
        table.extend_from_slice(&3u32.to_le_bytes());
        table.extend_from_slice(b"new");
        let mut patched = Parser::from_bytes(final_hcb, Nls::ShiftJIS).unwrap();
        patched.set_translation_table(&table).unwrap();
        assert_eq!(patched.translated_string(6), Some("new"));
        let mut unpatched = Parser::from_bytes(original, Nls::ShiftJIS).unwrap();
        assert!(unpatched.set_translation_table(&table).is_err());
    }
}

#[cfg(test)]
mod warning_opcode_tests {
    use alloc::{string::String, vec, vec::Vec};
    use crate::script::{context::Context, parser::{Nls, Parser, Syscall}, Variant, VmSyscall};
    #[cfg(feature = "hosted")]
    use crate::script::global::Global;

    #[derive(Default)]
    struct Recorder {
        events: Vec<(String, Vec<Variant>)>,
        #[cfg(feature = "hosted")]
        globals: Global,
    }
    impl VmSyscall for Recorder {
        fn do_syscall(&mut self, name: &str, args: Vec<Variant>) -> anyhow::Result<Variant> {
            self.events.push((String::from(name), args));
            Ok(Variant::Nil)
        }
        #[cfg(feature = "hosted")]
        fn with_globals<R>(&mut self, callback: impl FnOnce(&mut Global) -> R) -> R {
            callback(&mut self.globals)
        }
    }

    #[test]
    fn existing_opcodes_replay_call_wait_exactly_3000_and_return() {
        let mut original = vec![0; 4];
        original.extend_from_slice(&[2, 10, 0, 0, 0, 0]);
        let original_helper = original.len() as u32;
        original.extend_from_slice(&[1, 0, 0, 3, 0, 0, 4]);
        let warning_helper = original.len() as u32;
        original.extend_from_slice(&[1, 4, 0, 3, 1, 0, 4]);
        let descriptor = original.len() as u32;
        original[..4].copy_from_slice(&descriptor.to_le_bytes());
        original.extend_from_slice(&4u32.to_le_bytes());
        original.extend_from_slice(&[0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0]);
        let tail = original.len() as u32;
        let mut patched = original.clone();
        patched[4] = 6;
        patched[5..9].copy_from_slice(&tail.to_le_bytes());
        patched.push(2);
        patched.extend_from_slice(&original_helper.to_le_bytes());
        patched.extend_from_slice(&[11, 250, 0, 14, 8]);
        patched.extend_from_slice(b"warning\0");
        patched.extend_from_slice(&[12, 5, 8, 2]);
        patched.extend_from_slice(&warning_helper.to_le_bytes());
        patched.extend_from_slice(&[11, 184, 11, 3, 137, 0, 3, 133, 0, 6]);
        patched.extend_from_slice(&9u32.to_le_bytes());
        assert_eq!(patched.len() - original.len(), 40);
        assert_eq!(&patched[..4], &original[..4]);
        assert_eq!(&patched[9..original.len()], &original[9..]);
        let mut parser = Parser::from_bytes(patched, Nls::ShiftJIS).unwrap();
        for (id, args, name) in [(0, 0, "Original"), (1, 0, "Warning"), (137, 1, "ThreadWait"), (133, 0, "ThreadNext")] {
            parser.syscalls.insert(id, Syscall { args, name: String::from(name) });
        }
        let mut context = Context::new(4, 0);
        let mut recorder = Recorder::default();
        for _ in 0..32 {
            context.dispatch_opcode(&mut recorder, &mut parser).unwrap();
            if context.get_pc() == 9 { break; }
        }
        assert_eq!(context.get_pc(), 9);
        assert_eq!(recorder.events.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>(),
            ["Original", "Warning", "ThreadWait", "ThreadNext"]);
        assert_eq!(recorder.events[2].1.len(), 1);
        assert_eq!(recorder.events[2].1[0].as_int(), Some(3000));
    }
}
