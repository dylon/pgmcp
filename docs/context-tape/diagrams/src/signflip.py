#!/usr/bin/env python3
"""Why enc_i64 flips the sign bit.

A `chunk_id` is a signed `i64`, but the PathMap key must sort in *numeric* order
under plain lexicographic byte comparison. Interpreting the raw two's-complement
big-endian bytes as unsigned puts every NEGATIVE value ABOVE every positive one
(the sign bit is 1) — wrong order. XOR-ing the top bit (`v ^ 2^63`) maps the whole
signed range monotonically onto `[0, 2^64)`, so byte order == numeric order for all
`i64`. argv[1] = output SVG path.
"""
import sys

from _palette import C, apply_style

plt = apply_style()

vals = [-(2 ** 63), -(2 ** 62), -(2 ** 32), -1, 0, 1, 2 ** 32, 2 ** 62, 2 ** 63 - 1]
labels = ["i64::MIN", "-2^62", "-2^32", "-1", "0", "+1", "2^32", "2^62", "i64::MAX"]


def as_u64(v):
    return int(v) & (2 ** 64 - 1)


def raw(v):       # raw two's-complement bytes read as unsigned (the WRONG order)
    return as_u64(v) / 2 ** 64


def flipped(v):   # enc_i64: XOR the sign bit (the CORRECT, monotone order)
    return (as_u64(v) ^ (1 << 63)) / 2 ** 64


idx = list(range(len(vals)))
fig, ax = plt.subplots(figsize=(7.4, 3.9))
ax.plot(idx, [raw(v) for v in vals], "s--", color=C["untrusted"], lw=2.0,
        label="raw big-endian (two's complement) — non-monotone")
ax.plot(idx, [flipped(v) for v in vals], "o-", color=C["data"], lw=2.6,
        label="sign-flipped  enc_i64(v) = v ⊕ 2⁶³  — monotone")
ax.set_xticks(idx)
ax.set_xticklabels(labels, rotation=35, ha="right", fontsize=9)
ax.set_xlabel("signed chunk_id  (numerically increasing →)")
ax.set_ylabel("unsigned key value / 2⁶⁴\n(plain lexicographic byte order)")
ax.set_title("enc_i64 makes lexicographic byte order == numeric order")
ax.legend(loc="center left", fontsize=9)
fig.tight_layout()
fig.savefig(sys.argv[1] if len(sys.argv) > 1 else "signflip.svg")
