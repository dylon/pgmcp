"""Shared palette + Matplotlib style for the context-tape diagrams.

Imported by the plotting scripts (``from _palette import C, apply_style``) so
every rendered plot uses the same per-concept colours pinned in ``render.sh``.
This file has a leading underscore so ``render.sh`` skips it (it is a library,
not a diagram source — it defines no ``__main__`` render).
"""

# Per-concept palette (kept byte-identical to render.sh's header).
C = {
    "data": "#0d9488",       # data plane — teal
    "control": "#4f46e5",    # control plane — indigo
    "surface": "#d97706",    # verb surface — amber
    "corpus": "#475569",     # corpus / postgres — slate
    "trusted": "#16a34a",    # trusted zone — green
    "untrusted": "#dc2626",  # untrusted — red
    "resident": "#16a34a",   # resident / clean — green
    "dirty": "#d97706",      # dirty — amber
    "evicted": "#6b7280",    # evicted — grey
    "spilled": "#2563eb",    # out-of-core — blue
    "pinned": "#1d4ed8",     # pinned — blue
    "summary": "#7c3aed",    # summary node — purple
    "treatment": "#0d9488",  # experiment arms
    "control_arm": "#6b7280",
    "baseline": "#d97706",
}


def apply_style():
    """Apply the shared Matplotlib rcParams (call before plotting)."""
    import matplotlib

    matplotlib.use("svg")
    import matplotlib.pyplot as plt

    plt.rcParams.update({
        "svg.fonttype": "none",            # keep text as text (smaller, selectable)
        "font.family": "sans-serif",
        "font.sans-serif": ["Inter", "Helvetica", "Arial", "DejaVu Sans"],
        "font.size": 12,
        "axes.titlesize": 14,
        "axes.titleweight": "bold",
        "axes.edgecolor": "#475569",
        "axes.labelcolor": "#1e293b",
        "axes.grid": True,
        "grid.color": "#e2e8f0",
        "grid.linewidth": 0.8,
        "xtick.color": "#475569",
        "ytick.color": "#475569",
        "figure.facecolor": "white",
        "axes.facecolor": "white",
        "legend.frameon": False,
    })
    return plt
