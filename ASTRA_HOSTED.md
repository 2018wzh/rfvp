# Astra hosted patch policy

`astra-hosted` is rebased onto RFVP release `0.5.0`
(`3b5ea6c96a925c12f95aef8554905e8fecbc77c3`). The RFVP source tree, original
product hosts, and platform code retain upstream layout. Later upstream changes
are deliberately not inherited until they are reviewed as standalone patches.

The branch may add only RFVP-generic hosted-core seams:

- bounded resource reads and responses;
- session-owned execution state and snapshots;
- render, audio, and video command deltas;
- observer hooks and a bounded evidence crash-trace ring. Shipping sessions
  leave that ring disabled, so opcode dispatch has no extra read or trace write.

It must not contain Astra product types, package paths, EngineCore contracts,
or native graphics and audio handles.  Astra adapters consume the pinned branch
from their own repository.

The patch manifest is the ordered `0.5.0..astra-hosted` commit range. Each
patch records its upstream parent and is independently rebaseable.
The hosted trace is diagnostic-only: it is neither a save/replay format nor an
embedding serialization contract.
