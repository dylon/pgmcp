# The recency-decayed usage estimator behind the adaptive tool surface

**Companion to ADR-016.** This document derives, from first principles, the
scoring rule that selects each client's `Learned` default tool set
(`src/mcp/tool_policy.rs`). It is written so the estimator can be reconstructed
and reasoned about independently of the code.

## 0. What this is — and what it is not

The adaptive tool surface needs, per client *c* and tool *t*, a scalar that says
"how much does *c* currently rely on *t*?" so that `list_tools` can expose only
the tools a client actually uses. We compute that scalar as an **exponentially
time-decayed count of usage events** and threshold it.

It is **not** a trained or parametric machine-learning model: nothing is fit by
optimizing a loss, there are no learned weights, and there is no train/inference
split. The two constants (decay time-constant τ and inclusion threshold θ) are
fixed *a priori*, not estimated. The word "learned" in the code (`ToolSurface::
Learned`, "learned defaults") denotes **online adaptation to observed usage** —
the same sense in which an LFU-with-aging cache "learns" an access pattern — not
statistical learning. The precise classification is a **recency-weighted
frequency estimator** (an exponentially-weighted moving sum of the usage impulse
train). §8 places it among known methods.

## 1. The model

Fix a client *c* and tool *t*. Let its usage events (successful `mcp_tool_calls`
rows) within the lookback window have timestamps

    S = { s₁, s₂, …, sₙ },   sᵢ ≤ T,   T − sᵢ ≤ W

where *T* is the evaluation time (a cron tick) and *W* the lookback window. The
**score** is

    ┌─────────────────────────────────────────────┐
    │   w_{c,t}(T)  =  Σᵢ  exp( −(T − sᵢ) / τ )   │   (1)
    └─────────────────────────────────────────────┘

with **decay time-constant** τ > 0 (in the same time unit as the ages). Each use
deposits one unit of "mass" that decays exponentially as it ages; the score is
the total surviving mass. The tool enters *c*'s learned default set iff

    w_{c,t}(T)  ≥  θ                                    (2)

for the **inclusion threshold** θ > 0.

In the implementation (SQL, `recompute_and_persist`):

```sql
SUM( exp( -EXTRACT(EPOCH FROM (now() - ts)) / (τ_days * 86400) ) )   -- = (1)
WHERE ts > now() - make_interval(days => W_days) AND outcome = 'ok'
GROUP BY client_name, tool
```

Defaults (`ToolPolicyConfig`): **τ = 14 days, θ = 0.5, W = 90 days, N = 25**
(N is the cold-start prior size, §5). Ages are measured in seconds and divided by
`τ·86400`, i.e. (1) with the day as the unit.

## 2. Interpretation

**Decay kernel.** Write k(a) = exp(−a/τ) for the weight of an event of age
*a = T − sᵢ*. k(0) = 1 (a use *now* counts fully) and k decays smoothly:

    weight
     1.0 ┤●
         │ ●●
         │   ●●●                      k(a) = e^(−a/τ),  τ = 14 d
     0.5 ┤······●●●·······   ← half-life t½ = τ·ln2 ≈ 9.70 d
         │        ⋮ ●●●●
         │        ⋮     ●●●●●●●
     0.0 ┤        ⋮            ●●●●●●●●●●●●●●●●●●●●
         └────────┼────────┬────────┬────────┬─────→ age a (days)
                 9.7      14       28       42

**Half-life.** k(t½) = ½ ⟺ exp(−t½/τ) = ½ ⟺

    t½ = τ · ln 2 ≈ 0.693 τ        (τ = 14 d ⇒ t½ ≈ 9.70 d)        (3)

A single use is "worth half a use" after one half-life. After ~5τ it is
negligible.

**Recursive (streaming/EWMA) form.** Although we recompute (1) as a batch sum,
it is *identically* an exponentially-weighted moving sum maintained online.
Processing events in time order, keep `(w, s_last)`; on a new event at time *s*:

    w ← w · exp( −(s − s_last)/τ ) + 1 ;   s_last ← s              (4)

and at query time *T*, w_query = w · exp(−(T − s_last)/τ). Equation (4) is
"forward-decay" exponential smoothing of the usage impulse train — the same
recurrence as an EWMA, with the "+1" being the unit impulse of each event. Batch
(1) and streaming (4) agree exactly; we use the batch form because it needs no
persisted per-pair state and decays naturally by recomputing from raw timestamps.

## 3. Statistical properties

Model a tool's usage by client *c* as a **Poisson process of rate λ** (uses per
day) — the standard memoryless model for "events that recur with some long-run
frequency." Treat the lookback as effectively infinite (justified in §3.4). Then
by **Campbell's theorem** (for a Poisson process of intensity λ, the shot-noise
sum Σ f(ageᵢ) has mean λ∫f and variance λ∫f²):

### 3.1 Steady-state mean

    E[w] = λ ∫₀^∞ e^(−a/τ) da = λτ                                  (5)

The expected score is **λτ** — the usage rate times the memory length. This is
the single most useful identity: the score is a *smoothed, unit-bearing estimate
of the usage rate*, scaled by τ.

### 3.2 Variance and stability

    Var[w] = λ ∫₀^∞ e^(−2a/τ) da = λτ/2                              (6)

    CoV[w] = √Var / E = 1 / √(2λτ)                                   (7)

So frequently-used tools (λτ ≫ 1) have a **stable** score (small coefficient of
variation), and the noise is largest exactly for rarely-used tools hovering near
the threshold — which motivates the hysteresis enhancement in §9.

### 3.3 Threshold ⇔ rate gate (the interpretable result)

Combining (2) and (5), in expectation a tool is *learned* iff

    λτ ≥ θ   ⟺   λ ≥ θ/τ                                            (8)

With θ = 0.5, τ = 14: **λ ≥ 1/28 per day ≈ "used at least about once a month."**
The threshold is therefore not an opaque knob — it is a **minimum sustained usage
rate** of θ/τ uses per day. Raise θ (or lower τ) to demand more frequent use;
lower θ to keep a longer tail.

### 3.4 Window-truncation error

The implementation truncates the sum at age *W* (the `ts > now() − W` filter).
The expected mass discarded by truncation, for a steady stream, is

    E[dropped] = λ ∫_W^∞ e^(−a/τ) da = λτ · e^(−W/τ)                 (9)

i.e. a **relative** error of e^(−W/τ). With W = 90 d, τ = 14 d:
e^(−90/14) ≈ e^(−6.43) ≈ **0.0016 (0.16 %)** — negligible. The design rule is
**W ≳ 5τ** (here 90 ≈ 6.4τ), so the windowed sum tracks the infinite-horizon sum.

## 4. Convergence and the feedback loop

Because every native call *and* every `enable_tools` resolution is itself logged
to `mcp_tool_calls`, the estimator closes a loop:

1. A client `enable_tools`-es a tool → it is used → those uses raise w.
2. The next cron tick recomputes w; if the sustained rate clears (8), the tool
   joins the learned defaults and appears in `tools/list` natively.
3. If usage stops, w decays with half-life t½; once it falls below θ the tool
   drops out (its mass has aged away).

The fixed point is each client's true sustained working set: tools used at rate
≥ θ/τ persist; one-off explorations decay out. Convergence to within ε of the
steady-state mean takes O(τ·ln(1/ε)) time — a few half-lives.

## 5. Cold-start prior

A brand-new client has no history, so `learned_defaults(c)` would be empty and it
would see only `mandatory_core`. To bridge that, an unseen client falls back to
the **global prior**: the top-N tools by total mass across all clients,

    score_global(t) = Σ_c w_{c,t}(T),   prior = top-N by score_global   (10)

This is an empirical-Bayes-flavored shrinkage: absent client-specific evidence,
assume the population's revealed preference, then let the client's own data take
over as it accrues. N = 25 by default.

## 6. Computation

- **Batch recompute** (`recompute_and_persist`): one transaction does
  `DELETE FROM client_tool_policy` then re-aggregates (1) over the window for all
  `(client, tool)` pairs. Full recompute (rather than incremental) means a pair
  that has fallen out of the window simply does not reappear — decay-to-zero is
  automatic, with no stale state to expire.
- **Snapshot** (`ToolPolicySnapshot`): the thresholded sets (2) plus the global
  prior (10) are materialized into an in-memory map and published via an
  `ArcSwap` on `SystemContext`, so `list_tools` is an O(1) hash lookup, never a
  query. The `tool-policy-refresh` cron hot-swaps it every `tool_policy_interval_secs`
  (default 6 h); startup seeds it from the persisted `client_tool_policy` table.
- **Cost**: one grouped aggregation over a bounded (90-day) slice of
  `mcp_tool_calls` per interval — sub-second, no GPU, its own light cron lock.

## 7. Hyperparameters and tuning

| Symbol | Field              | Default | Meaning / effect                                                                        |
|--------|--------------------|---------|-----------------------------------------------------------------------------------------|
| τ      | `decay_tau_days`   | 14 d    | Memory length. Half-life τ·ln2 ≈ 9.7 d. ↑τ ⇒ longer memory, smoother, slower to forget. |
| θ      | `weight_threshold` | 0.5     | Inclusion gate. Equivalent rate θ/τ ≈ 1/28 d⁻¹ (8). ↑θ ⇒ smaller, higher-rate sets.     |
| W      | `lookback_days`    | 90 d    | Truncation horizon. Keep W ≳ 5τ so (9) is negligible.                                   |
| N      | `global_top_n`     | 25      | Cold-start prior size for unseen clients (10).                                          |

**Data-driven tuning.** Because E[w]=λτ and the gate is a rate θ/τ, θ can be set
to hit a target surface size: pick the θ whose induced median learned-set
cardinality matches a desired token budget for `tools/list`. τ trades
responsiveness (small τ adapts fast but is noisier per (7)) against stability.

## 8. Relationship to known methods

The estimator is a well-trodden construction under several names:

- **Exponential smoothing / EWMA** (Brown 1956; Holt 1957): (4) is exponential
  smoothing of the usage impulse train.
- **Time-decayed stream aggregates**: exponentially-decayed counts over data
  streams — Cohen & Strauss, *Maintaining time-decaying stream aggregates*
  (PODS 2003); Cormode, Shkapenyuk, Srivastava, Xu, *Forward decay* (ICDE 2009).
- **Cache admission with aging**: LFU-with-Dynamic-Aging (LFUDA), the GreedyDual
  family, and Redis' decayed-LFU eviction use the same "frequency that decays
  with time" idea.
- **Hawkes processes**: the background/triggering kernel μ(t)=Σᵢ exp(−(t−sᵢ)/τ) is
  exactly our score — i.e. a Hawkes self-exciting kernel with **fixed** decay and
  **no** estimated branching ratio. Fitting that branching ratio by MLE *would*
  be a learned model; we deliberately do not.
- **Recency-frequency recommenders / RFM**: exponential recency weighting of the
  "F" (frequency) signal is standard in recency-aware collaborative filtering.

The distinction throughout: these are *closed-form weightings with fixed
constants*, not fitted models.

## 9. Limitations and the genuinely-learned future path

- **No hysteresis.** A tool whose rate sits right at θ/τ can flap in/out across
  cron ticks (the variance (7) is largest there). Mitigation (future): a
  Schmitt-trigger band — separate enable threshold θ_in > disable threshold
  θ_out — so a tool must clearly rise to enter and clearly fall to leave.
- **No cost weighting.** A heavy-input-schema tool and a light one each deposit
  unit mass, though they cost different numbers of tokens in `tools/list`.
  A refinement weights each event (or the threshold) by the tool's schema token
  cost, optimizing tokens-saved rather than tool-count.
- **No sequence/context model.** The estimator is *marginal* per tool; it cannot
  predict that tool *t'* tends to follow tool *t*. The genuinely model-based
  enhancement noted in ADR-016 — co-occurrence or a sequence model that
  *pre-enables* the likely next tool — is where an actual trained model (and the
  word "machine-learned") would be warranted.

## 10. Worked example

Client *c*, today *T*. Tool **A** was used 4 times: 0, 3, 8, 20 days ago. Tool
**B** was used once, 30 days ago. τ = 14 d, θ = 0.5.

    w_A = e^0 + e^(−3/14) + e^(−8/14) + e^(−20/14)
        = 1 + 0.807 + 0.565 + 0.240  =  2.61   ≥ θ  →  A is learned
    w_B = e^(−30/14) = 0.117                    <  θ  →  B drops out

A's implied rate (5): λ̂_A = w_A/τ ≈ 0.187/day (~once every 5 days) — comfortably
above the gate θ/τ ≈ 0.036/day. B's single month-old use has decayed below the
"about once a month" bar and is correctly excluded.
