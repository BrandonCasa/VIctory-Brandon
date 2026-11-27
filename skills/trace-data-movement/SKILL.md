---
name: trace-data-movement
description: Use in VIctory-Brandon when a decision depends on the volume, frequency, or necessity of a CPU copy, conversion, GPU upload, or readback. Skip generic FPS reviews without a candidate transfer.
---

Follow one resource from its last writer to its consumer. Identify the backing storage on each side, the byte range, and the event that triggers each movement. Check whether a map, bind, or descriptor update actually moves bytes on this path.

Compare update frequency with transfer frequency. Before proposing reuse, identify what invalidates the existing contents. Describe the work the change removes or shifts. Quantify speedup only from measurements available within the task.
