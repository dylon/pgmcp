#!/usr/bin/env python3
"""The semantic axis: cosine similarity between a query vector and a page vector.

Vectors are stored L2-normalized, so the stored similarity is a plain dot product.
argv[1] = output SVG path.
"""
import sys

import numpy as np
import matplotlib.patches as mpatches

from _palette import C, apply_style

plt = apply_style()

q = np.array([0.92, 0.40])
v = np.array([0.30, 0.95])
cos = float(q.dot(v) / (np.linalg.norm(q) * np.linalg.norm(v)))

fig, ax = plt.subplots(figsize=(5.4, 5.0))
for vec, color, label in [(q, C["control"], "query  q"), (v, C["data"], "page  v")]:
    ax.annotate("", xy=vec, xytext=(0, 0),
                arrowprops=dict(arrowstyle="-|>", color=color, lw=2.6))
    ax.text(vec[0] * 1.04, vec[1] * 1.04, label, color=color, fontsize=11, weight="bold")

a_q = np.degrees(np.arctan2(q[1], q[0]))
a_v = np.degrees(np.arctan2(v[1], v[0]))
ax.add_patch(mpatches.Arc((0, 0), 0.46, 0.46, theta1=min(a_q, a_v), theta2=max(a_q, a_v),
                          color=C["summary"], lw=2.2))
ax.text(0.27, 0.26, "θ", color=C["summary"], fontsize=15, weight="bold")
ax.text(-0.05, 1.16, r"sim(q, v) = (q·v) / (‖q‖ ‖v‖) = cos θ = " + f"{cos:.2f}",
        fontsize=11)

ax.axhline(0, color="#cbd5e1", lw=0.8)
ax.axvline(0, color="#cbd5e1", lw=0.8)
ax.set_xlim(-0.12, 1.25)
ax.set_ylim(-0.12, 1.25)
ax.set_aspect("equal")
ax.set_xticks([0, 0.5, 1.0])
ax.set_yticks([0, 0.5, 1.0])
ax.set_title("Semantic axis: cosine similarity (higher = nearer)")
fig.tight_layout()
fig.savefig(sys.argv[1] if len(sys.argv) > 1 else "cosine.svg")
