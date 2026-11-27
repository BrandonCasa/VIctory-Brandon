# Resolve long virtual aliases once per physical run

## Problem

`crates/victory-hle/src/modules/dstorage.rs:1651–1675` notifies `xmem::pages_written` after writing physical destinations. `crates/victory-hle/src/modules/xmem.rs:540–600,916–966` implements `Shown`, `shown_at` and both GPU-notification branches.

`shown_at` clones a section reference for each destination page and calls `Shown::at` per page. Each call searches all long views, even those of unrelated sections. It materializes page-sized alias pieces, sorts them and joins neighbors. One contiguous write through one long mapping can produce just one final notification.

The title's scatter mapping call at EXE RVA `0xf2c470` registers individual 64 KiB views in `Shown::pages`. Contiguous guest addresses alone do not make these long views.

## Change

Intersect long views once per retained physical run and use the short-alias buckets for individual pages. Preserve the existing overlap and adjacency rules when sorting and joining notifications. Keep unknown page positions for callers that still supply only IDs. Scatter-mapped 64 KiB views enter the short buckets even when their addresses are contiguous.

## Expected result

For P pages across R runs and L long mappings, long-view probes fall from P × L to R × L. Retaining the completed write’s runs avoids P more physical-page lookups and per-page section reference clones. Short aliases still require page-bucket probes, and overlap can still require sorting page-sized pieces. The mapping writer needs no new long-view index.

[Related correctness finding](../finding-xmem-notify-long-aliases-by-run.md)
