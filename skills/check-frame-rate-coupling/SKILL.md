---
name: check-frame-rate-coupling
description: Use in VIctory-Brandon when changing presentation cadence could alter simulation, timers, animation, or input behavior. Skip low FPS alone unless there is evidence of a timing dependency.
---

Trace one affected update to its clock and call site. Identify its units and whether it advances per simulation tick, elapsed interval, or presented frame. Compare equal wall-clock intervals at two render rates while keeping the intended simulation cadence fixed.

Preserve intentional fixed-step updates and presentation-only interpolation. Address the first unintended coupling. Describe a comparison reasoned from code as a prediction unless the task includes measured evidence.
