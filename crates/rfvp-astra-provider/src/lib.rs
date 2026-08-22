//! RFVP-owned Astra Family ABI provider implementation.

mod archive;
mod factory;
pub mod ffi_bridge;
mod hcb;
mod hosted;
pub mod hosted_host;
pub mod hosted_runtime;
pub mod hosted_worker;
mod media_decode;
mod provider;
mod vfs_audit;

pub use archive::*;
pub use factory::*;
pub use hcb::*;
pub use hosted::*;
pub use media_decode::*;
pub use provider::*;

pub const RFVP_REFERENCE_REVISION: &str = "3b5ea6c96a925c12f95aef8554905e8fecbc77c3";
pub const FVP_FAMILY_ID: &str = "fvp";
pub const FVP_PROVIDER_ID: &str = "astra.emu.family.fvp";

pub fn release_syscall_ids() -> Vec<String> {
    let mut ids = rfvp_hosted::subsystem::components::syscalls::generated::SYSCALL_SPECS
        .iter()
        .filter(|spec| spec.name != "BREAKPOINT")
        .map(|spec| spec.name.to_owned())
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids
}

pub fn release_syscall_catalog_hash() -> astra_core::Hash256 {
    let ids = release_syscall_ids();
    astra_core::Hash256::from_sha256(format!("{}\n", ids.join("\n")).as_bytes())
}

pub fn release_opcode_ids() -> Vec<String> {
    (0_u8..=0x27)
        .map(|opcode| format!("0x{opcode:02x}"))
        .collect()
}
