"""Shared palette + Matplotlib style for the CSM (Communicating State Machine) diagrams.

Imported by any plotting script (``from _palette import C, apply_style``) so every
rendered plot uses the same per-concept colours pinned in ``render.sh``. The leading
underscore makes ``render.sh`` skip it (it is a library, not a diagram source).

One concept = one colour ("intuitive colorization per concept", doc guideline 14).
Kept byte-identical to render.sh's header.
"""

# Per-concept palette (main hex). Light fills / dark strokes live in render.sh.
C = {
    "role": "#4f46e5",        # role / participant — indigo
    "send": "#059669",        # Send · internal-choice ⊕ — emerald
    "recv": "#0284c7",        # Recv · external-choice & — sky
    "push": "#7c3aed",        # Push · Call (Σ_call) — violet
    "pop": "#c026d3",         # Pop · Return (Σ_ret) — fuchsia
    "internal": "#475569",    # Internal · neutral (Σ_int) — slate
    "text": "#16a34a",        # medium: Text — green
    "latent": "#d97706",      # medium: Latent — amber
    "global": "#3730a3",      # GlobalType / type layer — indigo-deep
    "local": "#0d9488",       # LocalMachine / runtime — teal
    "accept": "#16a34a",      # conformance accept — green
    "reject": "#dc2626",      # conformance reject — red
    "critic": "#ca8a04",      # Critic gate / verification — gold
    "mailbox": "#0891b2",     # mailbox plane — cyan
    "task": "#ea580c",        # task plane — orange
    "rlm": "#7c3aed",         # RLM frame / pushdown store — violet
    "pi": "#16a34a",          # pi (file work) — green
    "pgmcp": "#4f46e5",       # pgmcp (analytical) — indigo
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
