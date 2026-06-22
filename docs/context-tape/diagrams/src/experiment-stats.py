#!/usr/bin/env python3
"""The geometry of the four gated clauses of the frozen acceptance criterion.

ILLUSTRATIVE ONLY — the data here is synthetic (fixed-seed) and shows the SHAPE of
each clause's decision rule, NOT a measured run (the 3x3x5 execution is dataset-
gated). argv[1] = output SVG path.
"""
import sys

import numpy as np

from _palette import C, apply_style

plt = apply_style()
rng = np.random.default_rng(42)  # fixed seed ⇒ deterministic figure

fig, axes = plt.subplots(2, 2, figsize=(9.6, 7.0))
fig.suptitle("Frozen acceptance criterion — clause geometry (illustrative, not measured)",
             fontweight="bold")

# (a) accuracy: Welch-t, treatment > control
ctrl = rng.normal(0.60, 0.02, 400)
treat = rng.normal(0.79, 0.02, 400)
ax = axes[0, 0]
ax.hist(ctrl, bins=30, color=C["control_arm"], alpha=0.7, label="control (RLM, no tape)")
ax.hist(treat, bins=30, color=C["treatment"], alpha=0.8, label="treatment (tape)")
ax.set_title("accuracy: Welch-t, treatment > control (α=0.05)")
ax.set_xlabel("accuracy"); ax.legend(fontsize=8)

# (b) cost: TOST equivalence band ±20%
ax = axes[0, 1]
band = 0.20
ax.axvspan(-band, band, color=C["treatment"], alpha=0.18, label="±20% equivalence band")
ax.axvline(0, color="#475569", lw=1)
est = 0.02  # treatment cost ~2% above control: inside the band
ax.errorbar([est], [0], xerr=[0.08], fmt="o", color=C["data"], capsize=4,
            label="treatment − control (90% CI)")
ax.set_xlim(-0.4, 0.4); ax.set_yticks([])
ax.set_title("cost: TOST-equivalent within ±20%")
ax.set_xlabel("relative cost difference vs control"); ax.legend(fontsize=8, loc="upper left")

# (c) p95 latency CDF with the 30 000 ms SLO line
ax = axes[1, 0]
lat = np.sort(rng.gamma(2.0, 1500.0, 2000))
cdf = np.arange(1, len(lat) + 1) / len(lat)
ax.plot(lat, cdf, color=C["data"], lw=2.2)
p95 = np.percentile(lat, 95)
ax.axvline(30000, color=C["untrusted"], lw=2, ls="--", label="SLO 30 000 ms")
ax.axhline(0.95, color="#94a3b8", lw=1, ls=":")
ax.plot([p95], [0.95], "o", color=C["summary"], label=f"treatment p95 ≈ {p95:,.0f} ms")
ax.set_title("p95 latency: treatment p95 ≤ 30 000 ms SLO")
ax.set_xlabel("end-to-end latency (ms)"); ax.set_ylabel("CDF"); ax.legend(fontsize=8)

# (d) max-context: treatment ≥ 2× baseline
ax = axes[1, 1]
baseline, treatment = 128_000, 1_000_000
ax.bar(["baseline\n(long-ctx)", "treatment\n(tape)"], [baseline, treatment],
       color=[C["baseline"], C["treatment"]])
ax.axhline(2 * baseline, color=C["untrusted"], lw=2, ls="--", label="2× baseline gate")
ax.set_title("max-context-handled: treatment ≥ 2× baseline")
ax.set_ylabel("max context (tokens)"); ax.legend(fontsize=8)
ax.ticklabel_format(axis="y", style="sci", scilimits=(0, 0))

fig.tight_layout(rect=(0, 0, 1, 0.96))
fig.savefig(sys.argv[1] if len(sys.argv) > 1 else "experiment-stats.svg")
