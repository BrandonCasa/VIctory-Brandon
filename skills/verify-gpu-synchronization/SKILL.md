---
name: verify-gpu-synchronization
description: Use in VIctory-Brandon when changing a GPU wait, barrier, queue handoff, or resource reuse depends on a specific ordering or visibility guarantee. Skip transfer-volume questions alone.
---

Find the accesses that can conflict, their queues, and the resource range they share. Trace submission, signal, and wait to the first dependent access or reuse. Distinguish CPU submission from GPU completion, and check memory visibility and resource state where required by the backend.

Identify which dependency the existing synchronization enforces. Narrow it only when the replacement covers all affected paths, including frames in flight. If completion is ambiguous, state the missing guarantee.
