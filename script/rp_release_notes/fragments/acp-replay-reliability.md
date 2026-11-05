## ACP replay reliability

- Makes ACP thread replay reliably resolve tool locations before opening remote worktree buffers.
- Deduplicates replay location resolution so restored threads do not open the same location more than once.
