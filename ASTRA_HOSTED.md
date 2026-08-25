# Astra Family ABI v9 hosted boundary

The hosted branch is pinned to RFVP `0.5.0` at
`3b5ea6c96a925c12f95aef8554905e8fecbc77c3` and the AstraEngine ABI contract at
`635527831e89e5ff9b87ac165b5b5532e28356c6`.

The fork owns only the RFVP-to-host seam required by the `Ported + SingleLayer`
family mode:

- bounded VFS and writable-file calls supplied by the host;
- typed input, audio, video, wait, and control DTOs;
- direct software rendering into the Host-owned writable surface lease;
- damage derived from retained command identity and resource generations, with
  no framebuffer readback or content hash;
- a synchronous text-event handoff used by the provider Hook before surface
  acquisition; RFVP performs fallback, shaping, layout, and rasterization.

Evidence logging has a bounded diagnostic ring. Overflow is reported as a
typed WARN and never blocks or changes the RFVP step result; input, stride,
ownership, path, and allocation failures remain fail-fast.

Game-native save slots stay inside RFVP. Their bytes are serialized by the
core and persisted only through the bound per-game writable-file root; this
fork adds no Manager/Extension save-slot request and exposes no save/restore
ABI lifecycle.

The hosted core does not expose scene/texture capture, snapshot or restore
entry points, canonical state bytes, runtime semantic hashes, text leases, or
policy step budgets. Pixel payloads are never retained by the hosted bridge;
owned surface memory is moved into the software renderer and moved back to the
provider without a copy or an unsafe const-cast.

The RFVP core remains free of Astra product types, package paths, EngineCore
contracts, and native graphics/audio handles. The `rfvp-astra-provider` crate
is the only place that binds the Family ABI v9 DTOs, Hook, surface, and
writable-file ports.
