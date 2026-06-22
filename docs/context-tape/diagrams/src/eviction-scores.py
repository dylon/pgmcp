#!/usr/bin/env python3
"""Eviction-score curves for the two pgmcp-native policies, vs logical age.

Renders a two-panel figure: importance_weighted (the default) and cost_aware,
each showing how the eviction score (higher → evicted first) grows with a page's
logical age for a few representative pages. argv[1] = output SVG path.
"""
import sys

import numpy as np

from _palette import C, apply_style

plt = apply_style()

age = np.linspace(0, 100, 256)
use_count = 1  # (use_count + 1) == 2 in both denominators

fig, (ax0, ax1) = plt.subplots(1, 2, figsize=(9.4, 3.8))

# importance_weighted = age / (importance.max(1e-3) * (use_count + 1))
for imp, color, label in [(0.1, C["evicted"], "importance 0.1"),
                          (1.0, C["control"], "importance 1.0"),
                          (10.0, C["data"], "importance 10")]:
    ax0.plot(age, age / (imp * (use_count + 1)), color=color, lw=2.2, label=label)
ax0.set_title("importance_weighted  (default)")
ax0.set_xlabel("logical age  =  clock − last_access_ord")
ax0.set_ylabel("eviction score  (higher → evict first)")
ax0.set_yscale("log")
ax0.legend(title="(use_count = 1)")

# cost_aware = (age * (est_tokens + 1)) / (use_count + 1)
for tok, color, label in [(10, C["resident"], "10 tokens"),
                          (100, C["dirty"], "100 tokens"),
                          (500, C["untrusted"], "500 tokens")]:
    ax1.plot(age, age * (tok + 1) / (use_count + 1), color=color, lw=2.2, label=label)
ax1.set_title("cost_aware")
ax1.set_xlabel("logical age")
ax1.set_ylabel("eviction score")
ax1.legend(title="(use_count = 1)")

fig.suptitle("Logical-clock eviction scores (no wall-time)", fontweight="bold")
fig.tight_layout()
fig.savefig(sys.argv[1] if len(sys.argv) > 1 else "eviction-scores.svg")
