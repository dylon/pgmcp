//! Recursive Language Model (RLM) orchestration (Part B).
//!
//! Brings the RLM paradigm (Zhang/Kraska/Khattab, arXiv:2512.24601) to
//! pgmcp's A2A peer mesh: a long-context query is answered by treating the
//! corpus as an *external environment* (Postgres `file_chunks`), peeking
//! into it, decomposing it into snippets, **recursively sub-calling** a
//! peer LM over each snippet (small context), then **stitching** the
//! partial answers — the full context is never inlined into any single
//! prompt.
//!
//! Faithful to the paper on the load-bearing axes (prompt-as-environment,
//! decompose/filter, recursive sub-calls, stitch). The one deliberate
//! approximation: "the code the model writes" is a parameterized
//! [`DecomposeStrategy`] over the existing tool catalog (Chunk / semantic
//! retrieve / grep = the paper's emergent strategies a+b) rather than
//! free-form Python — so no sandboxed REPL is needed; safety is by
//! construction (read-only DB queries). Part B4's MSM trajectory memory
//! biases the strategy toward what worked.

use std::time::Instant;

use futures::stream::StreamExt;
use rmcp::ErrorData as McpError;
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::a2a::client::{A2aClient, SendOptions};
use crate::a2a::types::{Part, Task};
use crate::context::SystemContext;

/// "Prompt as environment ℰ" — a reference into the indexed corpus, never
/// inlined text.
#[derive(Debug, Clone)]
pub enum RlmEnvironment {
    /// A single indexed file (decomposed by its chunks).
    File { path: String },
    /// A contiguous chunk-index sub-region of a file — the child
    /// environment a depth>1 recursion narrows to. Still never inlines
    /// content: the peer re-reads its slice from `file_chunks` itself.
    FileRegion {
        path: String,
        start_chunk: i32,
        end_chunk: i32,
    },
    /// A project corpus (decomposed by semantic retrieval / grep).
    Corpus { project: Option<String> },
    /// Phase 7 — the **live read/WRITE Store** working memory shared across the
    /// whole recursion tree (the paper's root-LM `Store`). The tree's
    /// [`context_tape::TapeStore`] (resolved from
    /// [`crate::tape::registry::TapeRegistry`] by `root_task_id`) is decomposed
    /// by listing its pages; the stitch *accumulates* sub-call answers back into
    /// it so output is unbounded. `root_task_id` is filled FROM THE FRAME (never
    /// from the environment JSON), so a crafted frame can only ever address its
    /// OWN tree's store, never a sibling tree's.
    Store { root_task_id: Uuid },
}

impl RlmEnvironment {
    /// Parse the tool's `environment` JSON: `{kind:"file",path}`,
    /// `{kind:"corpus",project?}`, `{kind:"file_region",…}`, or `{kind:"store"}`.
    ///
    /// For `kind:"store"` the `root_task_id` is **deliberately not** read from
    /// the JSON — it is a placeholder ([`Uuid::nil`]) that addresses no real
    /// tree. [`RlmEnvironment::from_frame`] binds the authoritative
    /// `root_task_id` from the trusted [`RlmFrame`]; a JSON-supplied id would let
    /// a crafted frame point the Store env at another tree's working memory, so
    /// it is ignored here by construction.
    pub fn from_json(v: &Value) -> Result<Self, String> {
        match v.get("kind").and_then(|k| k.as_str()) {
            Some("file") => {
                let path = v
                    .get("path")
                    .and_then(|p| p.as_str())
                    .ok_or("environment.path required for kind=file")?;
                Ok(RlmEnvironment::File {
                    path: path.to_string(),
                })
            }
            Some("corpus") => Ok(RlmEnvironment::Corpus {
                project: v
                    .get("project")
                    .and_then(|p| p.as_str())
                    .map(|s| s.to_string()),
            }),
            Some("file_region") => {
                let path = v
                    .get("path")
                    .and_then(|p| p.as_str())
                    .ok_or("environment.path required for kind=file_region")?;
                Ok(RlmEnvironment::FileRegion {
                    path: path.to_string(),
                    start_chunk: v.get("start_chunk").and_then(|x| x.as_i64()).unwrap_or(0) as i32,
                    end_chunk: v
                        .get("end_chunk")
                        .and_then(|x| x.as_i64())
                        .unwrap_or(i32::MAX as i64) as i32,
                })
            }
            // The `root_task_id` is a placeholder; `from_frame` fills it from the
            // trusted frame. We never read it from the (potentially crafted) JSON.
            Some("store") => Ok(RlmEnvironment::Store {
                root_task_id: Uuid::nil(),
            }),
            _ => Err(
                "environment.kind must be \"file\", \"file_region\", \"corpus\", or \"store\""
                    .into(),
            ),
        }
    }

    /// Frame-aware parse: identical to [`from_json`](Self::from_json) for every
    /// corpus/file kind, but for `kind:"store"` it binds `root_task_id` from the
    /// TRUSTED [`RlmFrame`] (`frame.root_task_id`) rather than the environment
    /// JSON. This is the entry point the dispatcher / tool use so the Store env
    /// always scopes to the frame's own recursion tree.
    pub fn from_frame(v: &Value, frame: &RlmFrame) -> Result<Self, String> {
        match Self::from_json(v)? {
            RlmEnvironment::Store { .. } => Ok(RlmEnvironment::Store {
                root_task_id: frame.root_task_id,
            }),
            other => Ok(other),
        }
    }

    fn project(&self) -> Option<&str> {
        match self {
            RlmEnvironment::Corpus { project } => project.as_deref(),
            RlmEnvironment::File { .. }
            | RlmEnvironment::FileRegion { .. }
            | RlmEnvironment::Store { .. } => None,
        }
    }

    /// Environment-kind tag for the learning loop's per-kind cohorts (E). The
    /// `store` cohort is distinct so the MSM learner scores store-decompositions
    /// separately from file/corpus ones.
    pub fn kind(&self) -> &'static str {
        match self {
            RlmEnvironment::File { .. } | RlmEnvironment::FileRegion { .. } => "file",
            RlmEnvironment::Corpus { .. } => "corpus",
            RlmEnvironment::Store { .. } => "store",
        }
    }

    fn label(&self) -> String {
        match self {
            RlmEnvironment::File { path } => format!("file:{path}"),
            RlmEnvironment::FileRegion {
                path,
                start_chunk,
                end_chunk,
            } => format!("file_region:{path}:{start_chunk}-{end_chunk}"),
            RlmEnvironment::Corpus { project } => {
                format!("corpus:{}", project.as_deref().unwrap_or("*"))
            }
            RlmEnvironment::Store { root_task_id } => format!("store:{root_task_id}"),
        }
    }
}

/// How the environment is decomposed into snippets. Maps to the paper's
/// emergent strategies; B4 biases the choice toward what worked.
#[derive(Debug, Clone)]
pub enum DecomposeStrategy {
    /// Newline/AST chunks of a file (paper strategy b).
    Chunk,
    /// Top-k semantic retrieval ("filter by priors", paper strategy a).
    SemanticRetrieve { k: usize },
    /// Regex/keyword filter (paper strategy a).
    Grep { pattern: String },
}

impl DecomposeStrategy {
    /// String tag stored in the trajectory `environment` metadata (B3/B4).
    pub fn tag(&self) -> &'static str {
        match self {
            DecomposeStrategy::Chunk => "chunk",
            DecomposeStrategy::SemanticRetrieve { .. } => "semantic",
            DecomposeStrategy::Grep { .. } => "grep",
        }
    }
}

/// Hard cap on RLM recursion depth (tree height); bounds latency + cost.
///
/// IMPORTANT — this RUNTIME bound is deliberately SMALL and is **distinct from**
/// the static conformance stack bound [`crate::csm::role::MAX_STACK_DEPTH`] (4096).
/// Each RLM level issues real LM sub-calls, so its depth is a cost/DoS bound that
/// must stay low; the conformance bound governs *cheap static* trace-checking of
/// the genuine pushdown protocol `recursive_cf`
/// ([`crate::csm::registry::ProtocolId::RecursiveCf`]), where deep nesting costs
/// nothing. The two intentionally differ: equating them (making the runtime
/// recurse to 4096) would turn the RLM into a DoS vector. A recorded RLM run is
/// still conformance-checkable against `RecursiveCf` via
/// [`crate::csm::conformance::lift_transcript`] regardless of the runtime cap.
/// (ADR-030 records this runtime-vs-static-bound distinction.)
pub const MAX_RLM_DEPTH: u32 = 4;
/// Hard cap on total sub-calls across an entire recursion tree.
pub const MAX_RLM_BUDGET: u32 = 256;

/// Phase 7 — the bounded fold window for the accumulate-in-store stitch: how
/// many `accum/*` Store pages a single reduce sub-call folds (alongside the
/// running summary) before writing back the rolling `accum/summary`. Keeping
/// this small is what makes output *unbounded* — an arbitrarily large partial
/// set is folded `RLM_STITCH_WINDOW` pages at a time, so no single reduce prompt
/// ever holds the whole set. 4 balances reduce-call count against prompt size.
pub const RLM_STITCH_WINDOW: usize = 4;

/// The recursion frame threaded across A2A peers (in `Message.metadata.rlm`)
/// so a pgmcp peer CONTINUES decomposing instead of answering as a leaf.
/// Absent ⇒ ordinary/leaf task. (Part D — true depth>1.)
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RlmFrame {
    /// Schema version (current = 1).
    pub v: u8,
    /// 1-based absolute depth of THIS node (root run = 1); recorded on steps.
    pub depth: u32,
    /// Remaining decomposition depth. A peer with `depth_remaining == 0`
    /// answers as a leaf. Decremented by 1 on each recursive sub-call.
    pub depth_remaining: u32,
    /// Upper bound on TOTAL sub-calls the subtree rooted here may issue.
    pub budget_remaining: u32,
    /// Environment to (re-)decompose; same JSON shape as the tool param.
    pub environment: Value,
    /// Strategy to continue with, or `None` to let the peer choose/learn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy: Option<String>,
    /// The original top-level query (every node answers the same question).
    pub query: String,
    /// LM peer URL used to answer LEAF snippets — resolved once at the root
    /// and carried verbatim, so a recursing peer needs no shared agent
    /// registry (names are local; URLs are portable).
    pub sub_agent_url: String,
    /// LM peer URL for the stitch/verify reduce calls (defaults to
    /// `sub_agent_url` when absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reduce_agent_url: Option<String>,
    /// Per-node decomposition fan-out cap (carried down).
    pub max_chunks: usize,
    /// Bounded sub-call concurrency (carried down).
    pub concurrency: usize,
    /// Whether every node runs the verify+self-grade rubric (carried down so
    /// the whole tree yields graded trajectories when the root opted in).
    #[serde(default)]
    pub verify: bool,
    /// Base-URLs already on this path (root → … → parent), for cycle
    /// prevention. The current node appends its own URL on entry.
    #[serde(default)]
    pub path: Vec<String>,
    /// Root task id of the whole recursion tree (stable across hops).
    pub root_task_id: Uuid,
}

impl RlmFrame {
    /// Build the root frame for a new top-level RLM run.
    #[allow(clippy::too_many_arguments)]
    pub fn new_root(
        own_url: String,
        environment: Value,
        query: String,
        sub_agent_url: String,
        reduce_agent_url: Option<String>,
        max_chunks: usize,
        concurrency: usize,
        strategy: Option<String>,
        verify: bool,
        max_depth: u32,
        budget: u32,
        root_task_id: Uuid,
    ) -> Self {
        let max_depth = max_depth.clamp(1, MAX_RLM_DEPTH);
        Self {
            v: 1,
            depth: 1,
            depth_remaining: max_depth - 1,
            budget_remaining: budget.clamp(1, MAX_RLM_BUDGET),
            environment,
            strategy,
            query,
            sub_agent_url,
            reduce_agent_url,
            max_chunks,
            concurrency,
            verify,
            path: vec![own_url],
            root_task_id,
        }
    }

    /// Adopt an inbound frame at this node: defensively clamp depth/budget to
    /// the hard caps (a crafted frame cannot exceed them) and record our URL
    /// on the path for observability. Termination is guaranteed by the
    /// strictly-decreasing depth and the telescoping budget — NOT by
    /// URL-cycle rejection — so self-revisits (true self-recursion, the
    /// paper's "model calls itself") are expected and allowed.
    pub fn entered(mut self, own_url: String) -> Self {
        self.depth_remaining = self
            .depth_remaining
            .min(MAX_RLM_DEPTH.saturating_sub(self.depth));
        self.budget_remaining = self.budget_remaining.min(MAX_RLM_BUDGET);
        self.path.push(own_url);
        self
    }
}

/// One executed step — the unit recorded for the trajectory (B3).
#[derive(Debug, Clone)]
pub struct RlmStep {
    pub ord: i32,
    pub kind: StepKind,
    pub depth: u32,
    pub latency_ms: i64,
    pub est_tokens: i64,
    pub success: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKind {
    Peek,
    Filter,
    Chunk,
    Subcall,
    Verify,
    Stitch,
    /// Phase 7 — read a page from the tree's live Store (`TapeStore::get`).
    Get,
    /// Phase 7 — write a page into the tree's live Store (`TapeStore::put`),
    /// e.g. an accumulated sub-call answer or the folded running summary.
    Put,
    /// Phase 7 — admit a page into the hot tier (a hydrate / overlay promote).
    PageIn,
    /// Phase 7 — evict/spill a page out of the hot tier.
    PageOut,
}

impl StepKind {
    pub fn as_str(self) -> &'static str {
        match self {
            StepKind::Peek => "peek",
            StepKind::Filter => "filter",
            StepKind::Chunk => "chunk",
            StepKind::Subcall => "subcall",
            StepKind::Verify => "verify",
            StepKind::Stitch => "stitch",
            StepKind::Get => "get",
            StepKind::Put => "put",
            StepKind::PageIn => "page_in",
            StepKind::PageOut => "page_out",
        }
    }
}

/// Result of an RLM run.
#[derive(Debug, Clone)]
pub struct RlmOutcome {
    pub final_answer: String,
    pub steps: Vec<RlmStep>,
    pub subcalls: u32,
    pub chunks: usize,
    pub strategy: String,
    pub verified: bool,
    /// Self-grade in `[0,1]` from the verify rubric (E); `None` if verify off.
    pub self_grade: Option<f32>,
    /// True when `self_grade` came from a rubric call (≥0.6 can label a
    /// trajectory); false for the weak non-verified heuristic (capped <0.6).
    pub graded: bool,
    /// How the decompose strategy was chosen (observability): `"hint"` |
    /// `"msm_exploit"` | `"msm_explore"` | `"aggregate"` | `"cold_start"`.
    pub choice_reason: &'static str,
    /// Max step depth reached — the true tree height of this node's subtree.
    pub depth_reached: u32,
}

struct Snippet {
    text: String,
    provenance: String,
    /// Chunk-index span (for `Chunk`/`FileRegion` decompositions), so a
    /// depth>1 child can re-decompose exactly this slice without inlining.
    start_chunk: Option<i32>,
    end_chunk: Option<i32>,
    /// Phase 7 — the originating tape [`context_tape::PageAddress`] for a snippet
    /// drawn from a `Store` env. Present ONLY for Store decompositions; `None` for
    /// every file/corpus snippet (so those paths are byte-identical). Lets a
    /// depth>1 child address exactly this page, and lets the stitch read it back.
    addr: Option<context_tape::PageAddress>,
}

/// Free the recursion tree's shared [`context_tape::TapeStore`] **iff this is the
/// root frame** (H1). `run_rlm` shares one store per tree across every node, so
/// only the root (`frame.depth == 1`) may drop it: a child has `depth > 1` and
/// MUST NOT drop, since its parent and siblings are still folding into the same
/// store. Without this, every completed recursion tree's store stayed resident
/// forever (unbounded RAM growth).
///
/// [`TapeRegistry::drop_tree`](crate::tape::registry::TapeRegistry::drop_tree) is
/// a `DashMap::remove`, so it is idempotent / a no-op if the tree is already gone
/// — this guard is therefore safe to call on every root success path (it runs at
/// most once effectively) and never double-frees.
fn drop_tree_if_root(ctx: &SystemContext, frame: &RlmFrame) {
    if frame.depth == 1 {
        ctx.tape_registry().drop_tree(&frame.root_task_id);
    }
}

/// Run the RLM loop: peek → decompose → recursive sub-call → (verify) →
/// stitch. The corpus is never inlined; only per-snippet text crosses the
/// wire to the sub-LM peer.
pub async fn run_rlm(
    ctx: &SystemContext,
    env: &RlmEnvironment,
    frame: &RlmFrame,
    parent_task_id: Uuid,
) -> Result<RlmOutcome, McpError> {
    let query = frame.query.as_str();
    let depth = frame.depth;
    let max_chunks = frame.max_chunks;
    let sub_agent_url = frame.sub_agent_url.as_str();
    let reduce_agent_url = frame.reduce_agent_url.as_deref().unwrap_or(sub_agent_url);
    // Generous capacity: peek + filter + N subcalls + verify + stitch.
    let mut steps: Vec<RlmStep> = Vec::with_capacity(max_chunks + 4);
    let mut ord = 0i32;

    // 1. PEEK — bound the environment without reading all of it.
    let t = Instant::now();
    let preview = peek(ctx, env).await;
    push_step(
        &mut steps,
        &mut ord,
        StepKind::Peek,
        depth,
        &t,
        est_tokens(&preview),
        true,
    );

    // 2. STRATEGY (E): an explicit hint/frame strategy wins; otherwise the
    //    MSM-driven chooser picks by which strategy succeeded for similar
    //    past runs (cold-start ladder: aggregate → env default).
    let (strategy, choice_reason) = match frame.strategy.as_deref() {
        Some(h) => (choose_strategy(env, Some(h), max_chunks, query), "hint"),
        None => choose_strategy_msm(ctx, env, query, max_chunks, frame.root_task_id).await,
    };
    let t = Instant::now();
    let snippets = decompose(ctx, env, query, &strategy, max_chunks).await?;
    push_step(
        &mut steps,
        &mut ord,
        StepKind::Filter,
        depth,
        &t,
        0,
        !snippets.is_empty(),
    );

    // Degenerate: nothing decomposed → a single direct sub-call.
    if snippets.is_empty() {
        let t = Instant::now();
        let answer = subcall(sub_agent_url, query, parent_task_id).await?;
        let ok = !answer.trim().is_empty();
        push_step(
            &mut steps,
            &mut ord,
            StepKind::Subcall,
            depth,
            &t,
            est_tokens(&answer),
            ok,
        );
        let depth_reached = steps.iter().map(|s| s.depth).max().unwrap_or(depth);
        // H1: the root frame frees the shared tree store on EVERY success path,
        // including this degenerate one (a root that decomposed to zero snippets);
        // children (depth>1) never drop, the parent still owns the store.
        drop_tree_if_root(ctx, frame);
        return Ok(RlmOutcome {
            final_answer: answer,
            subcalls: 1,
            chunks: 0,
            strategy: strategy.tag().to_string(),
            verified: false,
            self_grade: Some(if ok { 0.5 } else { 0.0 }),
            graded: false,
            choice_reason,
            depth_reached,
            steps,
        });
    }

    // 3. SUB-CALL fan-out (B2 async, bounded). D: when depth remains and the
    //    budget can fund children, each snippet RECURSES (the peer
    //    re-decomposes its sub-region); otherwise a leaf sub-call answers
    //    from the snippet directly. The budget telescopes so total sub-calls
    //    across the whole tree stay ≤ the root budget.
    let can_recurse = frame.depth_remaining > 0
        && (frame.budget_remaining as usize) > snippets.len().saturating_mul(2);
    let per_child: u32 = if can_recurse {
        frame.budget_remaining.saturating_sub(snippets.len() as u32)
            / (snippets.len() as u32).max(1)
    } else {
        0
    };
    // Deeper subtrees must finish inside the caller's clock → longer window.
    let timeout_secs = 60u64 * (frame.depth_remaining as u64 + 1);
    let concurrency = frame.concurrency.clamp(1, 8);
    let sub_url = sub_agent_url.to_string();
    // Recursive sub-calls re-enter THIS engine over the loopback A2A surface
    // (self-recursion, the paper's "model calls itself"); leaf sub-calls go
    // to the LM peer.
    let recurse_url = own_a2a_url(ctx);

    let jobs: Vec<SubcallJob> = snippets
        .iter()
        .enumerate()
        .map(|(i, snip)| {
            if can_recurse {
                SubcallJob::Recurse {
                    i,
                    prov: snip.provenance.clone(),
                    frame: child_frame(frame, snippet_environment(env, snip), per_child),
                }
            } else {
                SubcallJob::Leaf {
                    i,
                    prov: snip.provenance.clone(),
                    prompt: leaf_prompt(query, snip),
                }
            }
        })
        .collect();

    let mut results: Vec<(usize, String, Option<String>, i64, u32)> =
        futures::stream::iter(jobs.into_iter().map(|job| {
            let sub_url = sub_url.clone();
            let recurse_url = recurse_url.clone();
            async move {
                let t = Instant::now();
                match job {
                    SubcallJob::Leaf { i, prov, prompt } => {
                        let ans = subcall(&sub_url, &prompt, parent_task_id).await.ok();
                        (i, prov, ans, t.elapsed().as_millis() as i64, depth)
                    }
                    SubcallJob::Recurse { i, prov, frame } => {
                        let ans =
                            subcall_recursive(&recurse_url, &frame, parent_task_id, timeout_secs)
                                .await
                                .ok();
                        (i, prov, ans, t.elapsed().as_millis() as i64, depth + 1)
                    }
                }
            }
        }))
        .buffer_unordered(concurrency)
        .collect()
        .await;
    results.sort_by_key(|(i, _, _, _, _)| *i);

    let mut partials: Vec<String> = Vec::with_capacity(results.len());
    let subcalls = results.len() as u32;
    for (_, prov, answer, latency, sc_depth) in &results {
        match answer {
            Some(a) => {
                let trimmed = a.trim();
                let useful = !trimmed.is_empty() && !trimmed.contains("(not found here)");
                push_step_lat(
                    &mut steps,
                    &mut ord,
                    StepKind::Subcall,
                    *sc_depth,
                    *latency,
                    est_tokens(a),
                    useful,
                );
                if useful {
                    partials.push(format!("[{prov}]\n{trimmed}"));
                }
            }
            None => push_step_lat(
                &mut steps,
                &mut ord,
                StepKind::Subcall,
                *sc_depth,
                *latency,
                0,
                false,
            ),
        }
    }

    // 4. STITCH — reduce the partials (NOT the original context) into one
    //    answer. This is the paper's "stitch through variables" — only the
    //    sub-call outputs, never the corpus, reach the reduce prompt.
    //
    //    Phase 7 (accumulate-in-store, UNBOUNDED OUTPUT): when the env is a
    //    Store, the partials are `put` into the tree store as `accum/<ord>`
    //    Scratch pages and folded ITERATIVELY in bounded windows — so the final
    //    answer is assembled from an arbitrarily large partial set without ever
    //    holding them all in one prompt. STRICTLY gated on Store presence so the
    //    File/Corpus path below is byte-identical.
    let mut final_answer = match env {
        RlmEnvironment::Store { root_task_id } => {
            accumulate_in_store_stitch(
                ctx.tape_registry(),
                *root_task_id,
                // `parent_task_id` is THIS stitching node's task id (== the tree's
                // `root_task_id` for the root, the child task id for a recursive
                // child); it namespaces this stitch's accumulator slots so the
                // concurrent siblings/parent sharing the tree store never collide
                // (C1). The store itself is still keyed by `root_task_id` above.
                parent_task_id,
                &partials,
                query,
                depth,
                &mut steps,
                &mut ord,
                // Production reducer: each fold window is reduced by the LM peer.
                // `async move` so the assembled prompt is owned by the future.
                |prompt| async move { subcall(reduce_agent_url, &prompt, parent_task_id).await },
            )
            .await?
        }
        _ => {
            let stitch_input = if partials.is_empty() {
                "(no snippet produced a relevant answer)".to_string()
            } else {
                partials.join("\n\n")
            };
            let t = Instant::now();
            let reduce_prompt = format!(
                "Combine these partial answers into one coherent, complete final answer to the query. \
                 Resolve overlaps and cite the snippet provenance where useful.\n\n\
                 Query:\n{query}\n\nPartial answers:\n{stitch_input}\n\nFinal answer:",
            );
            let answer = subcall(reduce_agent_url, &reduce_prompt, parent_task_id).await?;
            push_step(
                &mut steps,
                &mut ord,
                StepKind::Stitch,
                depth,
                &t,
                est_tokens(&answer),
                true,
            );
            answer
        }
    };

    // 5. VERIFY + SELF-GRADE (E): a rubric verify sub-call returns
    //    `GRADE: <0..1>` then a corrected answer. The parsed grade is the
    //    reliable trajectory success signal (≥ GRADE_PASS labels success;
    //    a confident low grade labels a failure — feeding the fail cohort).
    let mut self_grade: Option<f32> = None;
    let mut graded = false;
    let mut verified = false;
    if frame.verify {
        let t = Instant::now();
        let verify_prompt = format!(
            "Grade then correct this answer to the query. The FIRST line MUST be exactly \
             `GRADE: <x>` where x is a number in [0,1] (1 = fully correct & complete, \
             0 = wrong/empty/unsupported). Then, on the following lines, give the corrected \
             final answer only.\n\nQuery:\n{query}\n\nCandidate answer:\n{final_answer}\n\nGRADE:",
        );
        if let Ok(reply) = subcall(reduce_agent_url, &verify_prompt, parent_task_id).await
            && let Some((grade, body)) = parse_grade(&reply)
        {
            verified = grade >= GRADE_PASS;
            push_step(
                &mut steps,
                &mut ord,
                StepKind::Verify,
                depth,
                &t,
                est_tokens(&reply),
                verified,
            );
            self_grade = Some(grade);
            graded = true;
            if !body.trim().is_empty() {
                final_answer = body;
            }
        }
    }
    // Non-verified runs get a weak heuristic grade, capped strictly below
    // GRADE_PASS so it can never (falsely) label a confident success.
    if self_grade.is_none() {
        let useful = partials.len() as f32;
        self_grade = Some((0.5 * useful / subcalls.max(1) as f32).min(0.5));
        graded = false;
    }

    let depth_reached = steps.iter().map(|s| s.depth).max().unwrap_or(depth);
    // H1: at root-frame completion, free the shared per-tree TapeStore so its RAM
    // is reclaimed (a child has depth>1 and must NOT drop — it shares the tree
    // with its still-running parent and siblings). Placed on the success path only;
    // `?`-propagated errors above skip it, and `drop_tree` is an idempotent
    // DashMap remove so the degenerate-path drop above can never double-free.
    drop_tree_if_root(ctx, frame);
    Ok(RlmOutcome {
        final_answer,
        subcalls,
        chunks: snippets.len(),
        strategy: strategy.tag().to_string(),
        verified,
        self_grade,
        graded,
        choice_reason,
        depth_reached,
        steps,
    })
}

/// A per-snippet sub-call: either a leaf (answer from the snippet) or a
/// recursive RLM call (the peer re-decomposes a narrowed environment).
enum SubcallJob {
    Leaf {
        i: usize,
        prov: String,
        prompt: String,
    },
    Recurse {
        i: usize,
        prov: String,
        frame: RlmFrame,
    },
}

const GRADE_PASS: f32 = 0.6;

/// Per-snippet leaf prompt (small context).
fn leaf_prompt(query: &str, snip: &Snippet) -> String {
    format!(
        "You are answering one part of a larger query.\n\nQuery:\n{query}\n\n\
         Context snippet ({prov}):\n{text}\n\n\
         Answer ONLY from this snippet; reply exactly \"(not found here)\" if it is irrelevant.",
        prov = snip.provenance,
        text = snip.text,
    )
}

/// Build a child frame for a recursive sub-call: depth+1, depth_remaining-1,
/// the child's slice of the budget, the narrowed environment; path/root carry.
fn child_frame(parent: &RlmFrame, environment: Value, budget: u32) -> RlmFrame {
    RlmFrame {
        v: 1,
        depth: parent.depth + 1,
        depth_remaining: parent.depth_remaining.saturating_sub(1),
        budget_remaining: budget,
        environment,
        strategy: None,
        query: parent.query.clone(),
        sub_agent_url: parent.sub_agent_url.clone(),
        reduce_agent_url: parent.reduce_agent_url.clone(),
        max_chunks: parent.max_chunks,
        concurrency: parent.concurrency,
        verify: parent.verify,
        path: parent.path.clone(),
        root_task_id: parent.root_task_id,
    }
}

/// The child environment for a snippet: a chunk-addressable snippet becomes a
/// `FileRegion` the child re-decomposes; a semantic snippet becomes its whole
/// file (so the child explores the full file, not just the one hit). Never
/// inlines content.
fn snippet_environment(_parent_env: &RlmEnvironment, snip: &Snippet) -> Value {
    // Phase 7: a Store snippet's child re-decomposes the SAME tree store (a
    // `{"kind":"store"}` env). `root_task_id` is left for the frame to bind via
    // `from_frame` (the child frame carries the trusted `root_task_id`), so we
    // never address another tree from snippet data. The originating page address
    // is recorded for provenance/observability.
    //
    // KNOWN LIMITATION (intentionally not fixed here): the emitted `page` field is
    // currently advisory only — `RlmEnvironment::from_json`/`from_frame` ignore it,
    // so a Store child re-decomposes the tree's WHOLE non-accumulator working set
    // rather than narrowing to just this assigned page. With C1's per-stitch
    // accumulator namespacing (`accum/<parent_task_id>/…`) this is merely WASTEFUL
    // (redundant re-reads), not a correctness bug: `is_accum_slot` excludes every
    // accumulator page from re-decomposition, so a child can never re-fold any
    // stitch's running summary, and each stitch's fold pages are private to its own
    // node — there is no cross-contamination. Honoring `page` (narrowing the child
    // env to the single addressed page) would be a pure efficiency improvement and
    // is deliberately left as a separate change.
    if let Some(addr) = &snip.addr {
        return serde_json::json!({ "kind": "store", "page": addr.to_path() });
    }
    let path = snip
        .provenance
        .rsplit_once(':')
        .map(|(p, _)| p)
        .unwrap_or(snip.provenance.as_str());
    match (snip.start_chunk, snip.end_chunk) {
        (Some(lo), Some(hi)) => serde_json::json!({
            "kind": "file_region", "path": path, "start_chunk": lo, "end_chunk": hi
        }),
        _ => serde_json::json!({ "kind": "file", "path": path }),
    }
}

/// One RECURSIVE sub-call: send the child frame to a peer that will
/// re-decompose. A pgmcp peer continues the RLM; a leaf adapter degrades to
/// answering the query. Timeout scales with remaining depth.
async fn subcall_recursive(
    url: &str,
    frame: &RlmFrame,
    parent_task_id: Uuid,
    timeout_secs: u64,
) -> Result<String, McpError> {
    let task = A2aClient::new(url.to_string())
        .with_timeout(std::time::Duration::from_secs(timeout_secs))
        .send_task_rlm(
            Some("a2a_pattern_recursive"),
            SendOptions {
                recursion_rounds: None,
                parent_task_id: Some(parent_task_id),
            },
            frame,
        )
        .await
        .map_err(|e| {
            McpError::internal_error(format!("RLM recursive sub-call failed: {e}"), None)
        })?;
    Ok(task_to_text(&task))
}

/// This daemon's own A2A JSON-RPC endpoint — the self-recursion target. A
/// recursive sub-call re-enters this same engine (over the loopback A2A
/// surface) carrying a narrowed child frame. `0.0.0.0`/empty bind hosts are
/// normalized to loopback so the daemon can reach itself.
pub(crate) fn own_a2a_url(ctx: &SystemContext) -> String {
    let cfg = ctx.config().load();
    let host = match cfg.mcp.host.as_str() {
        "0.0.0.0" | "" => "127.0.0.1",
        h => h,
    };
    format!("http://{host}:{}/a2a/jsonrpc", cfg.mcp.port)
}

/// Parse a rubric reply whose first line is `GRADE: <x>`; returns the clamped
/// grade and the answer body (with the GRADE line stripped). `None` when the
/// model didn't emit a parseable grade (caller falls back to the heuristic).
fn parse_grade(reply: &str) -> Option<(f32, String)> {
    let mut lines = reply.lines();
    let first = lines.next()?;
    let after = first
        .trim()
        .strip_prefix("GRADE:")
        .or_else(|| first.trim().strip_prefix("Grade:"))?;
    let grade: f32 = after.split_whitespace().next()?.parse().ok()?;
    let body = lines.collect::<Vec<_>>().join("\n").trim().to_string();
    Some((grade.clamp(0.0, 1.0), body))
}

/// MSM-driven strategy choice (E): score each candidate `DecomposeStrategy`
/// by the distance-weighted, Laplace-smoothed success rate of its MSM-nearest
/// past trajectories (probed by a per-strategy synthetic series within that
/// strategy's own cohort), then epsilon-greedy explore/exploit. Cold-start
/// ladder: aggregate fallback → env default. Returns `(strategy, reason)`.
async fn choose_strategy_msm(
    ctx: &SystemContext,
    env: &RlmEnvironment,
    query: &str,
    max_chunks: usize,
    seed_id: Uuid,
) -> (DecomposeStrategy, &'static str) {
    use crate::fuzzy::trajectory_index::{DEFAULT_MSM_C, TrajectoryIndex, load_msm_c};
    let Some(pool) = ctx.db().pool() else {
        return (choose_strategy(env, None, max_chunks, query), "cold_start");
    };
    let kind = env.kind();
    let candidates: &[&str] = match env {
        RlmEnvironment::Corpus { .. } => &["semantic", "grep", "chunk"],
        _ => &["chunk", "semantic", "grep"],
    };
    let cfg = ctx.config().load();
    let neighbor_k = cfg.a2a.rlm.neighbor_k.max(1);
    let epsilon = cfg.a2a.rlm.explore_epsilon.clamp(0.0, 1.0) as f64;
    let c = load_msm_c(pool).await.unwrap_or(DEFAULT_MSM_C);
    let peek_tokens = est_tokens(&peek(ctx, env).await);

    // (tag, score, observation_count)
    let mut scored: Vec<(&'static str, f64, u64)> = Vec::with_capacity(candidates.len());
    for &tag in candidates {
        let succ = cohort_for(pool, kind, tag, true).await;
        let fail = cohort_for(pool, kind, tag, false).await;
        let total = (succ.len() + fail.len()) as u64;
        let expected = expected_subcall_count(ctx, env, tag, max_chunks).await;
        let probe = synthetic_probe(expected, peek_tokens);
        let s = TrajectoryIndex::new(succ, c).nearest(&probe, neighbor_k, None);
        let f = TrajectoryIndex::new(fail, c).nearest(&probe, neighbor_k, None);
        let pos: f64 = s.iter().map(|(_, d)| 1.0 / (1.0 + d)).sum();
        let neg: f64 = f.iter().map(|(_, d)| 1.0 / (1.0 + d)).sum();
        let score = (pos + 1.0) / (pos + neg + 2.0); // Laplace-smoothed
        scored.push((tag, score, total));
    }

    // Cold start: no labeled history for any candidate.
    if scored.iter().all(|(_, _, n)| *n == 0) {
        if let Some(agg) = best_strategy_for_env(ctx, env).await {
            return (strategy_from_tag(&agg, max_chunks, query), "aggregate");
        }
        return (choose_strategy(env, None, max_chunks, query), "cold_start");
    }

    // Epsilon-greedy: explore the least-tried candidate, else exploit the best.
    let (tag, reason) = if deterministic_unit(seed_id) < epsilon {
        let least = scored
            .iter()
            .min_by_key(|(_, _, n)| *n)
            .map(|(t, _, _)| *t)
            .unwrap_or(candidates[0]);
        (least, "msm_explore")
    } else {
        let best = scored
            .iter()
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(t, _, _)| *t)
            .unwrap_or(candidates[0]);
        (best, "msm_exploit")
    };
    (strategy_from_tag(tag, max_chunks, query), reason)
}

fn strategy_from_tag(tag: &str, max_chunks: usize, query: &str) -> DecomposeStrategy {
    match tag {
        "grep" => DecomposeStrategy::Grep {
            pattern: query.to_string(),
        },
        "semantic" => DecomposeStrategy::SemanticRetrieve { k: max_chunks },
        _ => DecomposeStrategy::Chunk,
    }
}

/// Deterministic, reproducible-per-task unit draw in `[0,1)` for the
/// epsilon-greedy explore decision (seeded by the root task id).
fn deterministic_unit(seed: Uuid) -> f64 {
    let n = seed.as_u128() as u64;
    (n % 10_000) as f64 / 10_000.0
}

/// Load a strategy's labeled trajectory cohort (encoded series) for an env kind.
async fn cohort_for(
    pool: &sqlx::PgPool,
    kind: &str,
    strategy_tag: &str,
    success: bool,
) -> Vec<(i64, Vec<f64>)> {
    sqlx::query_as::<_, (i64, Vec<f64>)>(
        "SELECT id, encoded_series FROM agent_trajectories
         WHERE environment->>'kind' = $1 AND strategy = $2 AND success = $3
           AND cardinality(encoded_series) > 0",
    )
    .bind(kind)
    .bind(strategy_tag)
    .bind(success)
    .fetch_all(pool)
    .await
    .unwrap_or_default()
}

/// The expected encoded series a strategy would produce, from cheap features
/// known before decomposition — used as the MSM probe (avoids the weak
/// live-prefix probe; matches the cohort's shape).
fn synthetic_probe(expected_subcalls: usize, peek_tokens: i64) -> Vec<f64> {
    let mut v = Vec::with_capacity(expected_subcalls + 3);
    v.push(encode_step(StepKind::Peek, 1, peek_tokens, true));
    v.push(encode_step(StepKind::Filter, 1, 0, true));
    for _ in 0..expected_subcalls.max(1) {
        v.push(encode_step(StepKind::Subcall, 1, 64, true));
    }
    v.push(encode_step(StepKind::Stitch, 1, 64, true));
    v
}

/// Cheap estimate of how many snippets a strategy will produce (probe shape).
async fn expected_subcall_count(
    ctx: &SystemContext,
    env: &RlmEnvironment,
    strategy_tag: &str,
    max_chunks: usize,
) -> usize {
    match (strategy_tag, env) {
        ("chunk", RlmEnvironment::File { path })
        | ("chunk", RlmEnvironment::FileRegion { path, .. }) => ctx
            .db()
            .file_chunk_summary(path)
            .await
            .map(|s| (s.chunk_count as usize).min(max_chunks))
            .unwrap_or(max_chunks),
        // semantic / grep typically fill ≈ k=max_chunks.
        _ => max_chunks,
    }
}

/// Peek at the environment's size/shape without reading all of it (mirrors
/// the paper's `print(prompt[:100])` / `len(prompt)`).
async fn peek(ctx: &SystemContext, env: &RlmEnvironment) -> String {
    match env {
        RlmEnvironment::File { path } => match ctx.db().file_chunk_summary(path).await {
            Ok(s) => format!(
                "file {path}: {} chunks, lines {}..{}",
                s.chunk_count,
                s.first_chunk_line.unwrap_or(0),
                s.last_chunk_line.unwrap_or(0),
            ),
            Err(_) => format!("file {path}: (no chunk summary)"),
        },
        RlmEnvironment::FileRegion {
            path,
            start_chunk,
            end_chunk,
        } => format!("file_region {path}: chunks {start_chunk}..{end_chunk}"),
        RlmEnvironment::Corpus { project } => {
            format!("corpus project={}", project.as_deref().unwrap_or("*"))
        }
        // Phase 7: peek the live Store's size without reading every page (mirrors
        // the paper's `len(prompt)`): resident page count + live byte total.
        RlmEnvironment::Store { root_task_id } => {
            ctx.tape_registry().with_store(*root_task_id, |s| {
                format!(
                    "store {root_task_id}: {} pages, {} resident bytes",
                    s.len(),
                    s.resident_bytes()
                )
            })
        }
    }
}

/// Choose a decomposition strategy: explicit hint wins, else by env kind.
fn choose_strategy(
    env: &RlmEnvironment,
    hint: Option<&str>,
    max_chunks: usize,
    query: &str,
) -> DecomposeStrategy {
    match hint {
        Some("grep") => DecomposeStrategy::Grep {
            pattern: query.to_string(),
        },
        Some("chunk") => DecomposeStrategy::Chunk,
        Some("semantic") => DecomposeStrategy::SemanticRetrieve { k: max_chunks },
        _ => match env {
            // A Store env lists its pages — the natural `Chunk` consumer (the
            // decompose path special-cases the listing); File/FileRegion also
            // default to chunk decomposition.
            RlmEnvironment::File { .. }
            | RlmEnvironment::FileRegion { .. }
            | RlmEnvironment::Store { .. } => DecomposeStrategy::Chunk,
            RlmEnvironment::Corpus { .. } => DecomposeStrategy::SemanticRetrieve { k: max_chunks },
        },
    }
}

/// B4 learning loop: the decompose strategy that has most often *succeeded*
/// for this environment kind, per recorded trajectories. `None` on cold
/// start (no labeled successes yet) — the caller then falls back to the
/// env default. This is the actionable "what works" bias; the MSM
/// trajectory index (`crate::fuzzy::trajectory_index`) provides the
/// complementary similarity/retrieval analysis.
async fn best_strategy_for_env(ctx: &SystemContext, env: &RlmEnvironment) -> Option<String> {
    let pool = ctx.db().pool()?;
    let kind = env.kind();
    sqlx::query_scalar::<_, String>(
        "SELECT strategy FROM agent_trajectories
         WHERE success = TRUE AND strategy IS NOT NULL
           AND environment->>'kind' = $1
         GROUP BY strategy
         ORDER BY COUNT(*) DESC
         LIMIT 1",
    )
    .bind(kind)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
}

/// Decompose the environment into at most `max` snippets per the strategy.
async fn decompose(
    ctx: &SystemContext,
    env: &RlmEnvironment,
    query: &str,
    strategy: &DecomposeStrategy,
    max: usize,
) -> Result<Vec<Snippet>, McpError> {
    let max = max.clamp(1, 64);
    // Phase 7: a Store env is decomposed by LISTING the tree's live tape pages
    // (positional/prefix), regardless of the strategy tag the learner picked —
    // enumerating working-memory pages *is* the decomposition (the natural
    // `DecomposeStrategy::Chunk` consumer over a Store, no new strategy variant).
    // Gated here so file/corpus decompositions below are byte-identical.
    if let RlmEnvironment::Store { root_task_id } = env {
        return Ok(store_snippets(ctx.tape_registry(), *root_task_id, max));
    }
    match strategy {
        DecomposeStrategy::Chunk => {
            let (path, lo, hi) = match env {
                RlmEnvironment::File { path } => (path.as_str(), 0, i32::MAX),
                RlmEnvironment::FileRegion {
                    path,
                    start_chunk,
                    end_chunk,
                } => (path.as_str(), *start_chunk, *end_chunk),
                // A corpus has no single chunk sequence — fall back to semantic.
                RlmEnvironment::Corpus { .. } => {
                    return semantic_snippets(ctx, env, query, max).await;
                }
                // Unreachable: a Store env is handled by the early `store_snippets`
                // return above (before this strategy match), so it never reaches
                // the chunk-range path.
                RlmEnvironment::Store { root_task_id } => {
                    return Ok(store_snippets(ctx.tape_registry(), *root_task_id, max));
                }
            };
            let rows = ctx
                .db()
                .get_chunks_in_index_range(path, lo, hi)
                .await
                .map_err(|e| McpError::internal_error(format!("chunk decompose: {e}"), None))?;
            Ok(rows
                .into_iter()
                .take(max)
                .map(|r| Snippet {
                    provenance: format!("{path}:{}-{}", r.start_line, r.end_line),
                    text: r.content,
                    start_chunk: Some(r.chunk_index),
                    end_chunk: Some(r.chunk_index),
                    addr: None,
                })
                .collect())
        }
        DecomposeStrategy::SemanticRetrieve { k } => {
            semantic_snippets(ctx, env, query, (*k).min(max)).await
        }
        DecomposeStrategy::Grep { pattern } => {
            let rows = ctx
                .db()
                .grep_search_chunks(pattern, env.project(), None, None, true, max as i32, false)
                .await
                .map_err(|e| McpError::internal_error(format!("grep decompose: {e}"), None))?;
            Ok(rows
                .into_iter()
                .take(max)
                .map(|r| Snippet {
                    provenance: format!("{}:{}-{}", r.path, r.start_line, r.end_line),
                    text: r.content,
                    start_chunk: Some(r.chunk_index),
                    end_chunk: Some(r.chunk_index),
                    addr: None,
                })
                .collect())
        }
    }
}

async fn semantic_snippets(
    ctx: &SystemContext,
    env: &RlmEnvironment,
    query: &str,
    k: usize,
) -> Result<Vec<Snippet>, McpError> {
    let embedding = ctx
        .embed()
        .embed_query(query)
        .await
        .map_err(|e| McpError::internal_error(format!("embed query: {e}"), None))?;
    let ef_search = ctx.config().load().vector.ef_search;
    let results = ctx
        .db()
        .semantic_search(&embedding, k as i32, None, env.project(), ef_search, false)
        .await
        .map_err(|e| McpError::internal_error(format!("semantic decompose: {e}"), None))?;
    Ok(results
        .into_iter()
        .map(|r| Snippet {
            provenance: format!("{}:{}-{}", r.path, r.start_line, r.end_line),
            text: r.chunk_content,
            // Semantic hits don't map to a contiguous chunk region; a depth>1
            // child of such a snippet re-decomposes the whole file instead.
            start_chunk: None,
            end_chunk: None,
            addr: None,
        })
        .collect())
}

/// The slot-byte prefix that scopes the accumulate-in-store pages within a
/// tree's `Scratch` namespace. Every accumulator page's slot begins with these
/// bytes (`accum/<parent_task_id>/0`, …, `accum/<parent_task_id>/summary`), so
/// [`is_accum_slot`] can exclude them from a Store re-decomposition and they sort
/// together. The prefix is preserved verbatim ahead of the per-stitch id, so the
/// `starts_with("accum/")` exclusion in [`is_accum_slot`] still matches every
/// `accum/<uuid>/...` slot unchanged.
const ACCUM_PREFIX: &str = "accum/";

/// Build the `Scratch` slot bytes for an accumulator page from the stitching
/// node's task id and the page's logical sub-key (`"0"`, `"summary"`, …): the
/// slot is `accum/<parent_task_id>/<sub_key>`, so it always lives under the
/// [`ACCUM_PREFIX`] namespace AND is private to this stitch invocation. The human
/// path is then `scratch/<tree>/<hex("accum/<parent_task_id>/<sub_key>")>`.
///
/// **Why the per-stitch `parent_task_id` segment (C1).** `run_rlm` shares ONE
/// [`context_tape::TapeStore`] per recursion *tree* (every node carries the
/// root's `root_task_id`), and concurrent sub-calls fold into it at the same
/// time. Keying accumulator pages by ordinal alone would make a parent and its
/// concurrent children at depth>1 all write the SAME `accum/summary` (and
/// `accum/<ord>`) slots, clobbering each other's rolling fold non-
/// deterministically. Namespacing each stitch's pages by its own node task id
/// gives every concurrent stitch a private `accum/<own_task_id>/summary`, so
/// siblings and parent never collide → a deterministic fold at any depth.
fn accum_slot(parent_task_id: Uuid, sub_key: &str) -> Box<[u8]> {
    // `accum/` + `<uuid>` (36 chars) + `/` + sub_key.
    let id = parent_task_id.to_string();
    let mut slot = Vec::with_capacity(ACCUM_PREFIX.len() + id.len() + 1 + sub_key.len());
    slot.extend_from_slice(ACCUM_PREFIX.as_bytes());
    slot.extend_from_slice(id.as_bytes());
    slot.push(b'/');
    slot.extend_from_slice(sub_key.as_bytes());
    slot.into_boxed_slice()
}

/// Phase 7 — decompose a `Store` env by listing the tree's live tape pages.
///
/// Reads the per-tree [`context_tape::TapeStore`] from the
/// [`TapeRegistry`](crate::tape::registry::TapeRegistry) by `root_task_id` and
/// enumerates its `Scratch` pages via the index's path-prefix (`scratch/<tree>/`)
/// axis — addresses come back in key (address) order. Each page becomes a
/// [`Snippet`] whose `provenance` is the page's
/// [`PageAddress::to_path`](context_tape::PageAddress::to_path) and whose `addr`
/// carries the originating [`context_tape::PageAddress`] (so a depth>1 child can
/// re-decompose exactly that page and the stitch can read it back). The store's
/// own `accum/*` accumulator pages are excluded — those are the stitch's running
/// fold, not source working memory to re-answer.
fn store_snippets(
    registry: &crate::tape::registry::TapeRegistry,
    root_task_id: Uuid,
    max: usize,
) -> Vec<Snippet> {
    let scratch_prefix = format!("scratch/{root_task_id}/");
    registry.with_store(root_task_id, |store| {
        let mut out = Vec::with_capacity(max);
        for addr in store.index().resolve_path_prefix(&scratch_prefix) {
            if out.len() >= max {
                break;
            }
            // Skip the stitch's own accumulator pages (running fold, not source):
            // their `Scratch` slot bytes begin with the `accum/` key prefix. We
            // compare the raw slot bytes (no hex dependency).
            if is_accum_slot(&addr) {
                continue;
            }
            if let Some(page) = store.get(&addr) {
                out.push(Snippet {
                    provenance: addr.to_path(),
                    text: page.content.clone(),
                    // A scratch page is an atomic working-memory unit (no chunk
                    // sub-range); `addr` carries the exact page for a child env.
                    start_chunk: None,
                    end_chunk: None,
                    addr: Some(addr),
                });
            }
        }
        out
    })
}

/// Whether a `PageAddress` is one of the accumulate-in-store stitch's own
/// `accum/*` pages (its slot bytes begin with the [`ACCUM_PREFIX`] key). Used to
/// exclude the running fold from a Store re-decomposition. Non-`Scratch`
/// addresses are never accumulator pages.
///
/// The C1 per-stitch namespacing inserts the stitching node's task id AFTER the
/// prefix (`accum/<parent_task_id>/<sub_key>`), so this `starts_with("accum/")`
/// check still excludes EVERY accumulator page — of every stitch in the tree, at
/// any depth — exactly as before; children therefore never re-fold any accumulator
/// page, their own or a sibling's.
fn is_accum_slot(addr: &context_tape::PageAddress) -> bool {
    matches!(
        addr,
        context_tape::PageAddress::Scratch { slot, .. } if slot.starts_with(ACCUM_PREFIX.as_bytes())
    )
}

/// Write `content` into the tree store as a `Scratch` page at the accumulator
/// slot for `key`, marking it dirty, and record a [`StepKind::Put`] step. The
/// page carries the `Scratch` kind + the shared `len/4` token estimate. Returns
/// the page address.
///
/// `root_task_id` keys the per-tree store (the `Scratch { tree }` field, shared
/// across the whole recursion tree); `parent_task_id` is the *stitching node's*
/// task id and namespaces the accumulator slot within that tree
/// (`accum/<parent_task_id>/<key>`) so concurrent stitches at depth>1 never
/// collide (C1). For the root they are equal; for a recursive child they differ.
///
/// Takes the [`TapeRegistry`](crate::tape::registry::TapeRegistry) directly (not
/// the whole `SystemContext`) so the accumulate-in-store fold is unit-testable
/// against a bare registry with no DB / network.
fn store_put_step(
    registry: &crate::tape::registry::TapeRegistry,
    root_task_id: Uuid,
    parent_task_id: Uuid,
    key: &str,
    content: &str,
    depth: u32,
    steps: &mut Vec<RlmStep>,
    ord: &mut i32,
) -> context_tape::PageAddress {
    let addr = context_tape::PageAddress::Scratch {
        tree: root_task_id,
        slot: accum_slot(parent_task_id, key),
    };
    let t = Instant::now();
    let est = context_tape::Page::estimate_tokens(content);
    let page = context_tape::Page::new(
        addr.clone(),
        content.to_string(),
        context_tape::PageMeta {
            kind: context_tape::PageKind::Scratch,
            est_tokens: est,
            importance: 0.5,
            dirty: true,
        },
    );
    registry.with_store_mut(root_task_id, |s| {
        s.put(addr.clone(), page);
    });
    push_step(steps, ord, StepKind::Put, depth, &t, est as i64, true);
    addr
}

/// Read a page's content from the tree store, recording a [`StepKind::Get`]
/// step. Returns `None` (and still records a failed `Get`) if the page is not
/// resident — accumulator pages we just wrote always are.
fn store_get_step(
    registry: &crate::tape::registry::TapeRegistry,
    root_task_id: Uuid,
    addr: &context_tape::PageAddress,
    depth: u32,
    steps: &mut Vec<RlmStep>,
    ord: &mut i32,
) -> Option<String> {
    let t = Instant::now();
    let content = registry.with_store(root_task_id, |s| s.get(addr).map(|p| p.content.clone()));
    let est = content.as_deref().map(est_tokens).unwrap_or(0);
    push_step(steps, ord, StepKind::Get, depth, &t, est, content.is_some());
    content
}

/// Build the reduce prompt for one fold window. `running` is the rolling summary
/// (absent on the first window); `window_block` is this window's accumulator
/// pages, already situated with their provenance.
fn fold_prompt(query: &str, running: Option<&str>, window_block: &str) -> String {
    match running {
        Some(prior) => format!(
            "Fold the additional partial answers into the running summary, producing one \
             coherent, complete answer to the query so far. Preserve every distinct fact; \
             resolve overlaps.\n\nQuery:\n{query}\n\nRunning summary:\n{prior}\n\n\
             Additional partial answers:\n{window_block}\n\nUpdated summary:",
        ),
        None => format!(
            "Combine these partial answers into one coherent, complete final answer to the \
             query. Resolve overlaps and cite the snippet provenance where useful.\n\n\
             Query:\n{query}\n\nPartial answers:\n{window_block}\n\nFinal answer:",
        ),
    }
}

/// Phase 7 — the **accumulate-in-store** stitch (unbounded output), generic over
/// the reduce backend so the fold is testable without a live LM peer.
///
/// Instead of reducing `partials` purely in memory, each useful partial is
/// `put` into the tree store as a `Scratch` page (`accum/<parent_task_id>/<ord>`),
/// and the stitch then folds those pages ITERATIVELY in bounded windows of
/// [`RLM_STITCH_WINDOW`]: read up to `K` pages (each a `Get`) plus the running
/// `accum/<parent_task_id>/summary`, reduce them via `reduce` (a `Stitch`), write
/// the result back to that same `summary` slot (a `Put`), and repeat. The final
/// answer is the last rolling summary — so an arbitrarily large partial set is
/// assembled without ever holding it all in one prompt. Every store read/write is
/// recorded as a `Get`/`Put` step (the MSM-visible store-plane signature).
///
/// `root_task_id` selects the shared per-tree store; `parent_task_id` is the
/// stitching node's own task id and namespaces THIS stitch's accumulator slots
/// within that store (C1). Because recursive sub-calls of one tree run
/// concurrently against the same store, ordinal-only slots would let a parent and
/// its children clobber each other's rolling fold at depth>1; per-node namespacing
/// makes each concurrent stitch's `accum/<parent_task_id>/…` pages private, so the
/// fold is deterministic at any depth.
///
/// `reduce` maps a fully-assembled reduce prompt → a folded answer; in
/// production it is the network sub-call to the reduce agent, in tests a pure
/// deterministic reducer.
#[allow(clippy::too_many_arguments)]
async fn accumulate_in_store_stitch<F, Fut>(
    registry: &crate::tape::registry::TapeRegistry,
    root_task_id: Uuid,
    parent_task_id: Uuid,
    partials: &[String],
    query: &str,
    depth: u32,
    steps: &mut Vec<RlmStep>,
    ord: &mut i32,
    reduce: F,
) -> Result<String, McpError>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = Result<String, McpError>>,
{
    // 1. ACCUMULATE: persist every useful partial as its own Store page. This is
    //    the write half of the live read/WRITE working memory — siblings sharing
    //    the tree store can `get` these pages too.
    let mut accum_addrs: Vec<context_tape::PageAddress> = Vec::with_capacity(partials.len());
    for (i, partial) in partials.iter().enumerate() {
        let addr = store_put_step(
            registry,
            root_task_id,
            parent_task_id,
            &format!("{i}"),
            partial,
            depth,
            steps,
            ord,
        );
        accum_addrs.push(addr);
    }

    // Degenerate: nothing accumulated — one reduce over the empty marker, so the
    // step shape still ends in a Stitch (mirrors the non-store empty handling).
    if accum_addrs.is_empty() {
        let t = Instant::now();
        let reduce_prompt = fold_prompt(query, None, "(no snippet produced a relevant answer)");
        let answer = reduce(reduce_prompt).await?;
        push_step(
            steps,
            ord,
            StepKind::Stitch,
            depth,
            &t,
            est_tokens(&answer),
            true,
        );
        return Ok(answer);
    }

    // 2. ITERATIVE FOLD in bounded windows. `running` is the rolling
    //    `accum/summary` page content; each window reduces at most
    //    `RLM_STITCH_WINDOW` fresh pages alongside it, so no prompt ever holds
    //    the whole partial set (this is what makes output unbounded).
    let mut running: Option<String> = None;
    for window in accum_addrs.chunks(RLM_STITCH_WINDOW) {
        // Read this window's pages back from the store (Get steps).
        let mut window_texts: Vec<String> = Vec::with_capacity(window.len());
        for addr in window {
            if let Some(text) = store_get_step(registry, root_task_id, addr, depth, steps, ord) {
                window_texts.push(format!("[{}]\n{}", addr.to_path(), text));
            }
        }
        let window_block = window_texts.join("\n\n");
        let reduce_prompt = fold_prompt(query, running.as_deref(), &window_block);
        let t = Instant::now();
        let folded = reduce(reduce_prompt).await?;
        push_step(
            steps,
            ord,
            StepKind::Stitch,
            depth,
            &t,
            est_tokens(&folded),
            true,
        );

        // Persist the rolling summary back into the store (Put step) so the fold
        // is itself durable working memory the next window reads from.
        store_put_step(
            registry,
            root_task_id,
            parent_task_id,
            "summary",
            &folded,
            depth,
            steps,
            ord,
        );
        running = Some(folded);
    }

    Ok(running.unwrap_or_else(|| "(no snippet produced a relevant answer)".to_string()))
}

/// One recursive sub-call to a peer LM (small context). Returns the peer's
/// concatenated text artifacts.
async fn subcall(url: &str, text: &str, parent_task_id: Uuid) -> Result<String, McpError> {
    let task = A2aClient::new(url.to_string())
        .send_task_with(
            text,
            None,
            SendOptions {
                recursion_rounds: None,
                parent_task_id: Some(parent_task_id),
            },
        )
        .await
        .map_err(|e| McpError::internal_error(format!("RLM sub-call failed: {e}"), None))?;
    Ok(task_to_text(&task))
}

fn task_to_text(task: &Task) -> String {
    let mut out = String::new();
    for art in &task.artifacts {
        for p in &art.parts {
            if let Part::Text { text, .. } = p {
                out.push_str(text);
                out.push('\n');
            }
        }
    }
    out
}

/// Rough token estimate (≈4 chars/token) for the trajectory metrics.
fn est_tokens(s: &str) -> i64 {
    (s.len() / 4) as i64
}

fn push_step(
    steps: &mut Vec<RlmStep>,
    ord: &mut i32,
    kind: StepKind,
    depth: u32,
    start: &Instant,
    est_tokens: i64,
    success: bool,
) {
    push_step_lat(
        steps,
        ord,
        kind,
        depth,
        start.elapsed().as_millis() as i64,
        est_tokens,
        success,
    );
}

/// `push_step` variant with an explicit latency (used by the concurrent
/// sub-call fan-out, where latency is measured inside each future).
fn push_step_lat(
    steps: &mut Vec<RlmStep>,
    ord: &mut i32,
    kind: StepKind,
    depth: u32,
    latency_ms: i64,
    est_tokens: i64,
    success: bool,
) {
    steps.push(RlmStep {
        ord: *ord,
        kind,
        depth,
        latency_ms,
        est_tokens,
        success,
    });
    *ord += 1;
}

/// Encode one step as a single f64 (the "effort signature"); the MSM
/// trajectory index (B4) compares sequences of these. Univariate because
/// `MsmConfig::distance` is defined over `&[f64]`. Move handles cost drift
/// between similar runs; Split/Merge handle differing sub-call counts.
pub fn encode_step(kind: StepKind, depth: u32, est_tokens: i64, success: bool) -> f64 {
    let base = match kind {
        StepKind::Peek => 1.0,
        StepKind::Filter => 2.0,
        StepKind::Chunk => 3.0,
        StepKind::Subcall => 4.0,
        StepKind::Verify => 5.0,
        StepKind::Stitch => 6.0,
        // Phase 7 store-plane steps continue the numbering (a new MSM signature
        // band so store-decompositions are scored apart from file/corpus runs).
        StepKind::Get => 7.0,
        StepKind::Put => 8.0,
        StepKind::PageIn => 9.0,
        StepKind::PageOut => 10.0,
    };
    base + (depth as f64) * 0.5 + (1.0 + est_tokens.max(0) as f64).ln() * 0.1
        - if success { 0.0 } else { 0.5 }
}

impl RlmOutcome {
    /// The precomputed step→f64 sequence stored in
    /// `agent_trajectories.encoded_series` for MSM comparison (B3/B4).
    pub fn encoded_series(&self) -> Vec<f64> {
        let mut series = Vec::with_capacity(self.steps.len());
        for s in &self.steps {
            series.push(encode_step(s.kind, s.depth, s.est_tokens, s.success));
        }
        series
    }
}

/// Persist a completed RLM run as an `agent_trajectories` row + its
/// `trajectory_steps` (B3). One transaction; preallocated. Returns the
/// trajectory id.
///
/// E (closed MSM loop): a *graded* run (rubric verify) labels its own
/// `success` here from the self-grade (`≥ GRADE_PASS`), so the next run's
/// MSM strategy cohorts are populated immediately — no wait on an external
/// report. *Ungraded* runs leave `success` NULL for
/// [`label_trajectories_from_outcomes`] to back-fill from `agent_outcomes`
/// (the Part-A↔B seam). The per-run rubric is the sharper trajectory signal
/// (an explicit outcome report grades the whole parent task, not this run).
pub async fn persist_trajectory(
    pool: &PgPool,
    task_id: Uuid,
    parent_task_id: Option<Uuid>,
    environment: &Value,
    query: &str,
    outcome: &RlmOutcome,
) -> Result<i64, sqlx::Error> {
    let series = outcome.encoded_series();
    let depth_reached = outcome.steps.iter().map(|s| s.depth).max().unwrap_or(0) as i32;
    let total_latency: i64 = outcome.steps.iter().map(|s| s.latency_ms).sum();
    let sha = sha256_hex(query);
    let success: Option<bool> = if outcome.graded {
        outcome.self_grade.map(|g| g >= GRADE_PASS)
    } else {
        None
    };

    let mut tx = pool.begin().await?;
    let traj_id: i64 = sqlx::query_scalar(
        "INSERT INTO agent_trajectories
            (task_id, parent_task_id, kind, environment, query_sha256, strategy,
             depth_reached, total_subcalls, total_latency_ms, success, self_grade,
             encoded_series)
         VALUES ($1, $2, 'rlm', $3, $4, $5, $6, $7, $8, $9, $10, $11)
         RETURNING id",
    )
    .bind(task_id)
    .bind(parent_task_id)
    .bind(environment)
    .bind(&sha)
    .bind(&outcome.strategy)
    .bind(depth_reached)
    .bind(outcome.subcalls as i32)
    .bind(total_latency)
    .bind(success)
    .bind(outcome.self_grade.map(|g| g as f64))
    .bind(&series)
    .fetch_one(&mut *tx)
    .await?;
    for step in &outcome.steps {
        sqlx::query(
            "INSERT INTO trajectory_steps
                (trajectory_id, ord, step_kind, depth, latency_ms, est_tokens, success)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (trajectory_id, ord) DO NOTHING",
        )
        .bind(traj_id)
        .bind(step.ord)
        .bind(step.kind.as_str())
        .bind(step.depth as i32)
        .bind(step.latency_ms)
        .bind(step.est_tokens)
        .bind(step.success)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(traj_id)
}

fn sha256_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    format!("{:x}", h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(max_depth: u32, budget: u32) -> RlmFrame {
        RlmFrame::new_root(
            "http://root/a2a/jsonrpc".into(),
            serde_json::json!({ "kind": "corpus" }),
            "q".into(),
            "http://sub/a2a/jsonrpc".into(),
            None,
            8,
            4,
            None,
            false,
            max_depth,
            budget,
            Uuid::nil(),
        )
    }

    #[test]
    fn new_root_clamps_depth_and_budget_to_hard_caps() {
        let f = root(99, 9999);
        assert_eq!(f.depth, 1);
        assert_eq!(f.depth_remaining, MAX_RLM_DEPTH - 1);
        assert_eq!(f.budget_remaining, MAX_RLM_BUDGET);
        assert_eq!(f.path, vec!["http://root/a2a/jsonrpc".to_string()]);
    }

    #[test]
    fn new_root_depth_one_is_leaf_only() {
        // Default rlm_depth = 1 ⇒ no remaining recursion ⇒ B1 behavior.
        let f = root(1, 64);
        assert_eq!(f.depth_remaining, 0);
    }

    #[test]
    fn entered_clamps_to_remaining_height_and_appends_path() {
        let mut child = root(4, 64);
        child.depth = 3; // a node deep in the tree …
        child.depth_remaining = 99; // … with a crafted, inflated budget
        let entered = child.entered("http://peer/a2a/jsonrpc".into());
        // Clamped to MAX_RLM_DEPTH - depth, so the tree can never exceed the cap.
        assert_eq!(entered.depth_remaining, MAX_RLM_DEPTH - 3);
        assert!(entered.budget_remaining <= MAX_RLM_BUDGET);
        assert!(
            entered
                .path
                .contains(&"http://peer/a2a/jsonrpc".to_string())
        );
    }

    #[test]
    fn encode_step_penalizes_failure_and_is_deterministic() {
        let ok = encode_step(StepKind::Subcall, 1, 64, true);
        let fail = encode_step(StepKind::Subcall, 1, 64, false);
        assert!(
            fail < ok,
            "a failed step encodes lower than a successful one"
        );
        assert_eq!(
            ok,
            encode_step(StepKind::Subcall, 1, 64, true),
            "encoding is deterministic"
        );
    }

    // ===================== Phase 7 — Store engine integration =====================

    /// Deterministic tree id for the store tests (the registry is keyed by it).
    fn store_tree() -> Uuid {
        Uuid::from_u128(0x0005_704e_0000_0000_0000_0000_0000_0001)
    }

    /// GOLDEN (extended for Phase 7): `encode_step`'s per-kind BASE code is
    /// isolated by probing at `depth=0, est_tokens=0, success=true`, where the
    /// formula collapses to exactly `base` (`0*0.5 + ln(1+0)*0.1 - 0 == 0`). This
    /// pins every code — the six pre-existing ones MUST be unchanged (byte-
    /// identity of File/Corpus encodings) and the four new store-plane ones
    /// continue the numbering 7..10.
    #[test]
    fn encode_step_golden_codes_including_phase7() {
        let base = |k: StepKind| encode_step(k, 0, 0, true);
        // Pre-existing codes — must not move (regression guard).
        assert_eq!(base(StepKind::Peek), 1.0);
        assert_eq!(base(StepKind::Filter), 2.0);
        assert_eq!(base(StepKind::Chunk), 3.0);
        assert_eq!(base(StepKind::Subcall), 4.0);
        assert_eq!(base(StepKind::Verify), 5.0);
        assert_eq!(base(StepKind::Stitch), 6.0);
        // Phase 7 store-plane codes — new, continuing the numbering.
        assert_eq!(base(StepKind::Get), 7.0);
        assert_eq!(base(StepKind::Put), 8.0);
        assert_eq!(base(StepKind::PageIn), 9.0);
        assert_eq!(base(StepKind::PageOut), 10.0);
        // `as_str` round-trips for the new kinds (free-text `step_kind` column).
        assert_eq!(StepKind::Get.as_str(), "get");
        assert_eq!(StepKind::Put.as_str(), "put");
        assert_eq!(StepKind::PageIn.as_str(), "page_in");
        assert_eq!(StepKind::PageOut.as_str(), "page_out");
    }

    /// REGRESSION (byte-identity): a representative File/Corpus run's encoded
    /// series is exactly what `encode_step` produces for its steps — the Phase-7
    /// enum/encode additions do not perturb any pre-Phase-7 step encoding. The
    /// golden series is recomputed from the SAME formula a pre-change build used
    /// (the six base codes are pinned above), so a drift in either the formula or
    /// a base code fails here.
    #[test]
    fn file_corpus_encoded_series_is_byte_identical() {
        // Peek, Filter, two Subcalls (one ok, one failed), Stitch — the shape a
        // File run with a 2-snippet fan-out emits.
        let steps = vec![
            RlmStep {
                ord: 0,
                kind: StepKind::Peek,
                depth: 1,
                latency_ms: 3,
                est_tokens: 10,
                success: true,
            },
            RlmStep {
                ord: 1,
                kind: StepKind::Filter,
                depth: 1,
                latency_ms: 1,
                est_tokens: 0,
                success: true,
            },
            RlmStep {
                ord: 2,
                kind: StepKind::Subcall,
                depth: 1,
                latency_ms: 9,
                est_tokens: 64,
                success: true,
            },
            RlmStep {
                ord: 3,
                kind: StepKind::Subcall,
                depth: 1,
                latency_ms: 9,
                est_tokens: 64,
                success: false,
            },
            RlmStep {
                ord: 4,
                kind: StepKind::Stitch,
                depth: 1,
                latency_ms: 5,
                est_tokens: 32,
                success: true,
            },
        ];
        let outcome = RlmOutcome {
            final_answer: "x".into(),
            steps: steps.clone(),
            subcalls: 2,
            chunks: 2,
            strategy: "chunk".into(),
            verified: false,
            self_grade: Some(0.25),
            graded: false,
            choice_reason: "hint",
            depth_reached: 1,
        };
        // The exact pre-Phase-7 golden encodings (independently computed).
        let golden = [
            1.739_789_527_279_837_2_f64,
            2.5_f64,
            4.917_438_726_989_563_f64,
            4.417_438_726_989_563_f64,
            6.849_650_756_146_648_f64,
        ];
        let series = outcome.encoded_series();
        assert_eq!(series.len(), golden.len());
        for (got, want) in series.iter().zip(golden.iter()) {
            assert!(
                (got - want).abs() < 1e-12,
                "File/Corpus encoding drifted: got {got}, want {want}"
            );
        }
        // And the step_kind text sequence is the classic one (no store kinds).
        let kinds: Vec<&str> = steps.iter().map(|s| s.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec!["peek", "filter", "subcall", "subcall", "stitch"]
        );
    }

    /// SECURITY: `from_json` parses `{"kind":"store"}` to a NIL-root placeholder
    /// (it never trusts a JSON-supplied id), and `from_frame` binds the
    /// authoritative `root_task_id` from the trusted frame — so a crafted frame
    /// can only address its OWN tree's store.
    #[test]
    fn store_root_comes_from_frame_not_json() {
        // Even a JSON that tries to inject a different root_task_id is ignored.
        let crafted =
            serde_json::json!({ "kind": "store", "root_task_id": Uuid::from_u128(0xdead) });
        match RlmEnvironment::from_json(&crafted).expect("store env parses") {
            RlmEnvironment::Store { root_task_id } => {
                assert_eq!(
                    root_task_id,
                    Uuid::nil(),
                    "from_json must NOT read root from JSON"
                );
            }
            other => panic!("expected Store, got {other:?}"),
        }
        // from_frame binds the frame's root (here the test tree).
        let mut frame = root(2, 32);
        frame.root_task_id = store_tree();
        match RlmEnvironment::from_frame(&crafted, &frame).expect("store env parses") {
            RlmEnvironment::Store { root_task_id } => {
                assert_eq!(
                    root_task_id,
                    store_tree(),
                    "from_frame binds the trusted frame root"
                );
            }
            other => panic!("expected Store, got {other:?}"),
        }
    }

    /// The Store env reports the `store` MSM cohort kind and a `store:<uuid>`
    /// label, distinct from file/corpus.
    #[test]
    fn store_env_kind_and_label() {
        let env = RlmEnvironment::Store {
            root_task_id: store_tree(),
        };
        assert_eq!(env.kind(), "store");
        assert_eq!(env.label(), format!("store:{}", store_tree()));
        // A store env has no project filter.
        assert!(env.project().is_none());
    }

    /// SHARED WORKING MEMORY: a page written by one simulated sub-agent into the
    /// tree store is visible to a LATER sibling that lists/reads the SAME tree —
    /// the registry is keyed by `root_task_id`, so siblings share one store.
    #[test]
    fn store_pages_are_shared_across_siblings() {
        let registry = crate::tape::registry::TapeRegistry::new();
        let tree = store_tree();

        // Sub-agent A writes a working-memory page (a non-accumulator scratch
        // page, simulating an agent's REPL output / intermediate variable).
        let addr_a = context_tape::PageAddress::Scratch {
            tree,
            slot: b"facts/0".to_vec().into_boxed_slice(),
        };
        registry.with_store_mut(tree, |s| {
            s.put(
                addr_a.clone(),
                context_tape::Page::new(
                    addr_a.clone(),
                    "the capital of France is Paris".to_string(),
                    context_tape::PageMeta::clean(context_tape::PageKind::Scratch, 8, 0.5),
                ),
            );
        });

        // Sibling B decomposes the SAME tree's store and sees A's page.
        let snippets = store_snippets(&registry, tree, 16);
        assert_eq!(snippets.len(), 1, "sibling sees the shared page");
        assert_eq!(snippets[0].provenance, addr_a.to_path());
        assert!(snippets[0].text.contains("Paris"));
        assert_eq!(
            snippets[0].addr.as_ref(),
            Some(&addr_a),
            "snippet carries the page address"
        );

        // Sibling B can also read it directly through the Get-recording helper.
        let mut steps = Vec::new();
        let mut ord = 0;
        let got = store_get_step(&registry, tree, &addr_a, 1, &mut steps, &mut ord);
        assert_eq!(got.as_deref(), Some("the capital of France is Paris"));
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].kind, StepKind::Get);
        assert!(steps[0].success);

        // A DIFFERENT tree's store is isolated (no cross-tree leakage).
        let other_tree = Uuid::from_u128(0x0005_704e_0000_0000_0000_0000_0000_0002);
        assert!(store_snippets(&registry, other_tree, 16).is_empty());
    }

    /// UNBOUNDED OUTPUT: the accumulate-in-store stitch folds N partials where
    /// `N > RLM_STITCH_WINDOW` into ONE answer WITHOUT ever holding them all in a
    /// single reduce prompt. The pure (concatenating) reducer lets us assert,
    /// deterministically, that (a) every one of the N facts reaches the final
    /// answer, (b) more than one reduce call happened (so it genuinely folded in
    /// bounded windows, not a single prompt), and (c) no single reduce prompt
    /// contained all N partials (the unboundedness invariant). It also asserts
    /// the Get/Put store-plane steps were recorded.
    #[tokio::test]
    async fn accumulate_in_store_stitch_folds_beyond_one_window() {
        use std::cell::RefCell;

        let registry = crate::tape::registry::TapeRegistry::new();
        let tree = store_tree();

        // N strictly greater than one window, spanning >= 3 windows.
        let n = RLM_STITCH_WINDOW * 2 + 1;
        let partials: Vec<String> = (0..n).map(|i| format!("FACT{i}")).collect();

        // Pure reducer: record each prompt it saw, and "fold" by extracting all
        // FACT tokens it can see (running summary + this window) into a new
        // summary. This models a perfect reducer while staying deterministic.
        let seen_prompts: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let reduce = |prompt: String| {
            seen_prompts.borrow_mut().push(prompt.clone());
            // Collect every FACT<i> token present in the prompt, dedup-preserving.
            let mut facts: Vec<String> = Vec::new();
            for tok in prompt.split(|c: char| !(c.is_alphanumeric())) {
                if tok.starts_with("FACT") && !facts.contains(&tok.to_string()) {
                    facts.push(tok.to_string());
                }
            }
            let folded = facts.join(",");
            std::future::ready(Ok::<String, McpError>(folded))
        };

        let mut steps = Vec::new();
        let mut ord = 0;
        // Root-level stitch: the stitching node's id equals the tree id, so the
        // accumulator slots are namespaced `accum/<tree>/…` (C1).
        let answer = accumulate_in_store_stitch(
            &registry,
            tree,
            tree,
            &partials,
            "merge the facts",
            1,
            &mut steps,
            &mut ord,
            reduce,
        )
        .await
        .expect("fold succeeds");

        // (a) Every fact reached the final answer (unbounded output assembled).
        for i in 0..n {
            assert!(
                answer.contains(&format!("FACT{i}")),
                "missing FACT{i} in folded answer"
            );
        }

        // (b) It folded in multiple windows (more than one reduce call).
        let prompts = seen_prompts.into_inner();
        let expected_windows = n.div_ceil(RLM_STITCH_WINDOW);
        assert_eq!(
            prompts.len(),
            expected_windows,
            "one reduce per bounded window"
        );
        assert!(
            prompts.len() > 1,
            "N>window must fold across multiple windows"
        );

        // (c) No single reduce prompt held more than RLM_STITCH_WINDOW FRESH
        //     pages. Each fresh page is situated with a `[scratch/<tree>/…]`
        //     provenance marker (the running summary is NOT, so counting markers
        //     counts exactly the window's fresh pages). This is the unboundedness
        //     invariant: an arbitrarily large N is never inlined into one prompt.
        let marker = format!("[scratch/{tree}/");
        for p in &prompts {
            let fresh_pages = p.matches(&marker).count();
            assert!(
                fresh_pages <= RLM_STITCH_WINDOW,
                "a window prompt held {fresh_pages} fresh pages (> window {RLM_STITCH_WINDOW})"
            );
        }
        // And at least one window held strictly fewer than N (the whole set was
        // never in a single prompt).
        assert!(
            prompts.iter().all(|p| p.matches(&marker).count() < n),
            "no single prompt may hold all {n} partials"
        );

        // Store-plane steps recorded: a Put per partial + a Put per window summary,
        // and a Get per partial read back. The series carries the new codes.
        let puts = steps.iter().filter(|s| s.kind == StepKind::Put).count();
        let gets = steps.iter().filter(|s| s.kind == StepKind::Get).count();
        let stitches = steps.iter().filter(|s| s.kind == StepKind::Stitch).count();
        assert_eq!(
            puts,
            n + expected_windows,
            "put per partial + per rolling summary"
        );
        assert_eq!(gets, n, "each accumulated partial is read back once");
        assert_eq!(stitches, expected_windows, "one fold/stitch per window");

        // The encoded series contains the Get and Put codes (>=7.0) — the new
        // store-plane signature the MSM learner now sees.
        let outcome = RlmOutcome {
            final_answer: answer,
            steps: steps.clone(),
            subcalls: 0,
            chunks: n,
            strategy: "chunk".into(),
            verified: false,
            self_grade: None,
            graded: false,
            choice_reason: "msm_exploit",
            depth_reached: 1,
        };
        let series = outcome.encoded_series();
        assert!(
            series.iter().any(|&v| (7.0..8.0).contains(&v)),
            "encoded_series must contain a Get code (~7.x)"
        );
        assert!(
            series.iter().any(|&v| (8.0..9.0).contains(&v)),
            "encoded_series must contain a Put code (~8.x)"
        );

        // The accumulator pages (`accum/0..` + the rolling `accum/summary`) ARE
        // resident durable working memory after the fold — but they are EXCLUDED
        // from a Store re-decomposition (they are the fold, not source). This
        // test wrote ONLY accumulator pages, so a re-list yields zero source
        // snippets even though the pages are present.
        let resident = registry.with_store(tree, |s| s.len());
        assert_eq!(
            resident,
            n + 1,
            "N accum pages + 1 rolling summary are resident"
        );
        let relisted = store_snippets(&registry, tree, 1024);
        assert!(
            relisted.is_empty(),
            "accum/* fold pages are excluded from a Store re-decomposition"
        );
    }

    /// The empty-partials accumulate path still ends in a single Stitch (parity
    /// with the non-store empty handling) and records no Get/Put churn beyond it.
    #[tokio::test]
    async fn accumulate_in_store_stitch_handles_empty_partials() {
        let registry = crate::tape::registry::TapeRegistry::new();
        let tree = store_tree();
        let mut steps = Vec::new();
        let mut ord = 0;
        let answer = accumulate_in_store_stitch(
            &registry,
            tree,
            tree,
            &[],
            "q",
            1,
            &mut steps,
            &mut ord,
            |_prompt| std::future::ready(Ok::<String, McpError>("EMPTY".to_string())),
        )
        .await
        .expect("empty fold succeeds");
        assert_eq!(answer, "EMPTY");
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].kind, StepKind::Stitch);
    }

    /// A Store snippet's child environment is a `{"kind":"store"}` env (the child
    /// re-decomposes the SAME tree store) — never a file/corpus env, and the
    /// `root_task_id` is left for the child frame to bind via `from_frame`.
    #[test]
    fn store_snippet_child_environment_is_store_kind() {
        let snip = Snippet {
            text: "x".into(),
            provenance: "scratch/abc/00".into(),
            start_chunk: None,
            end_chunk: None,
            addr: Some(context_tape::PageAddress::Scratch {
                tree: store_tree(),
                slot: b"facts/0".to_vec().into_boxed_slice(),
            }),
        };
        let env = snippet_environment(
            &RlmEnvironment::Store {
                root_task_id: store_tree(),
            },
            &snip,
        );
        assert_eq!(env.get("kind").and_then(|k| k.as_str()), Some("store"));
        // No file/region keys leak in.
        assert!(env.get("path").is_none());
    }

    // ===================== C1 — per-stitch accumulator namespacing =====================

    /// C1 (slot shape): `accum_slot` namespaces by the stitching node's task id —
    /// the slot bytes are exactly `accum/<parent_task_id>/<sub_key>`, keeping the
    /// `ACCUM_PREFIX` verbatim at the front. Two different sub-keys under the same
    /// node differ; the same sub-key under two different nodes differ — so no
    /// ordinal-only collision is possible.
    #[test]
    fn accum_slot_is_namespaced_by_parent_task_id() {
        let node_a = Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_00aa);
        let node_b = Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_00bb);

        let a_summary = accum_slot(node_a, "summary");
        let expected = format!("accum/{node_a}/summary");
        assert_eq!(
            &*a_summary,
            expected.as_bytes(),
            "slot is accum/<parent_task_id>/<sub_key>"
        );
        assert!(
            a_summary.starts_with(ACCUM_PREFIX.as_bytes()),
            "ACCUM_PREFIX is preserved verbatim at the front"
        );

        // Same sub-key, different nodes ⇒ distinct slots (the C1 invariant).
        assert_ne!(
            accum_slot(node_a, "summary"),
            accum_slot(node_b, "summary"),
            "the SAME ordinal under two stitch nodes must not collide"
        );
        // Different sub-keys, same node ⇒ distinct slots (ordinals stay separate).
        assert_ne!(accum_slot(node_a, "0"), accum_slot(node_a, "summary"));
    }

    /// C1 (exclusion preserved): `is_accum_slot` STILL excludes a namespaced
    /// `accum/<uuid>/<sub_key>` page (the `starts_with("accum/")` check is
    /// unaffected by the inserted id segment), so children never re-fold any
    /// stitch's accumulator pages — their own or a sibling's.
    #[test]
    fn is_accum_slot_matches_namespaced_pages() {
        let node = Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_00cc);
        let tree = store_tree();
        for sub_key in ["0", "1", "summary"] {
            let addr = context_tape::PageAddress::Scratch {
                tree,
                slot: accum_slot(node, sub_key),
            };
            assert!(
                is_accum_slot(&addr),
                "namespaced accum/<uuid>/{sub_key} must still be excluded"
            );
        }
        // A genuine (non-accumulator) working-memory page is NOT excluded.
        let working = context_tape::PageAddress::Scratch {
            tree,
            slot: b"facts/0".to_vec().into_boxed_slice(),
        };
        assert!(!is_accum_slot(&working));
    }

    /// C1 (no clobber at depth>1): TWO concurrent-style stitches with DIFFERENT
    /// node ids folding into the SAME shared tree store keep PRIVATE rolling
    /// summaries — neither overwrites the other's `accum/<id>/summary`. This is the
    /// regression test for the depth>1 collision: under ordinal-only keying both
    /// would write `accum/summary` and the second would clobber the first.
    #[tokio::test]
    async fn sibling_stitches_do_not_clobber_each_others_summary() {
        let registry = crate::tape::registry::TapeRegistry::new();
        let tree = store_tree();
        let node_a = Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_0a01);
        let node_b = Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_0b02);

        // A pass-through reducer: the folded answer is just the window block, so
        // each stitch's summary deterministically reflects only ITS OWN partials.
        let reduce = |prompt: String| std::future::ready(Ok::<String, McpError>(prompt));

        let mut steps = Vec::new();
        let mut ord = 0;
        // Stitch A folds its partials under node_a.
        accumulate_in_store_stitch(
            &registry,
            tree,
            node_a,
            &["A_FACT_ONE".to_string(), "A_FACT_TWO".to_string()],
            "qa",
            2,
            &mut steps,
            &mut ord,
            reduce,
        )
        .await
        .expect("stitch A folds");
        // Stitch B folds DIFFERENT partials under node_b into the SAME tree.
        accumulate_in_store_stitch(
            &registry,
            tree,
            node_b,
            &["B_FACT_ONE".to_string(), "B_FACT_TWO".to_string()],
            "qb",
            2,
            &mut steps,
            &mut ord,
            reduce,
        )
        .await
        .expect("stitch B folds");

        // Each node's private rolling summary survives, scoped to its own facts.
        let summary_a = registry
            .with_store(tree, |s| {
                s.get(&context_tape::PageAddress::Scratch {
                    tree,
                    slot: accum_slot(node_a, "summary"),
                })
                .map(|p| p.content.clone())
            })
            .expect("node A summary is resident");
        let summary_b = registry
            .with_store(tree, |s| {
                s.get(&context_tape::PageAddress::Scratch {
                    tree,
                    slot: accum_slot(node_b, "summary"),
                })
                .map(|p| p.content.clone())
            })
            .expect("node B summary is resident");

        assert!(
            summary_a.contains("A_FACT_ONE") && summary_a.contains("A_FACT_TWO"),
            "node A summary holds A's facts"
        );
        assert!(
            !summary_a.contains("B_FACT"),
            "node A summary was NOT clobbered by sibling B"
        );
        assert!(
            summary_b.contains("B_FACT_ONE") && summary_b.contains("B_FACT_TWO"),
            "node B summary holds B's facts"
        );
        assert!(
            !summary_b.contains("A_FACT"),
            "node B summary was NOT clobbered by sibling A"
        );

        // Both stitches' accumulator pages are excluded from re-decomposition
        // (only fold pages were written, so the source working set is empty).
        assert!(
            store_snippets(&registry, tree, 1024).is_empty(),
            "every accum/<id>/* fold page is excluded from a Store re-decomposition"
        );
    }

    // ===================== H1 — root-only TapeStore drop =====================

    /// H1 (depth predicate + idempotency): the root-only drop guard frees the
    /// shared store ONLY at `depth == 1`; a child frame (`depth > 1`) leaves it
    /// resident (its parent and siblings still share it), and a repeated root drop
    /// is a harmless no-op (the underlying `drop_tree` is an idempotent DashMap
    /// remove — never a double-free).
    ///
    /// This exercises the exact guard logic of `drop_tree_if_root`
    /// (`if frame.depth == 1 { registry.drop_tree(&frame.root_task_id) }`) against
    /// a bare [`TapeRegistry`], mirroring how the store tests avoid standing up a
    /// full `SystemContext` (which needs a DB) for pure registry-side behavior.
    #[test]
    fn root_only_drop_predicate_and_idempotency() {
        let registry = crate::tape::registry::TapeRegistry::new();
        let tree = store_tree();

        // Materialise a store for the tree (lazy-create on first touch).
        registry.with_store_mut(tree, |s| {
            let addr = context_tape::PageAddress::Scratch {
                tree,
                slot: b"facts/0".to_vec().into_boxed_slice(),
            };
            s.put(
                addr.clone(),
                context_tape::Page::new(
                    addr,
                    "resident".to_string(),
                    context_tape::PageMeta::clean(context_tape::PageKind::Scratch, 1, 0.5),
                ),
            );
        });
        assert!(registry.contains(&tree), "store materialised");

        // The guard the root-drop applies: drop iff depth == 1.
        let drop_if_root = |registry: &crate::tape::registry::TapeRegistry, frame: &RlmFrame| {
            if frame.depth == 1 {
                registry.drop_tree(&frame.root_task_id);
            }
        };

        // A CHILD frame (depth > 1) must NOT drop the shared store.
        let mut child = root(MAX_RLM_DEPTH, MAX_RLM_BUDGET);
        child.depth = 2;
        child.root_task_id = tree;
        drop_if_root(&registry, &child);
        assert!(
            registry.contains(&tree),
            "a depth>1 child must NOT drop the shared tree store"
        );

        // The ROOT frame (depth == 1) frees it.
        let mut root_frame = root(MAX_RLM_DEPTH, MAX_RLM_BUDGET);
        root_frame.root_task_id = tree;
        assert_eq!(root_frame.depth, 1, "new_root yields depth 1");
        drop_if_root(&registry, &root_frame);
        assert!(
            !registry.contains(&tree),
            "the root frame frees the shared tree store"
        );

        // Idempotent: a second root drop (e.g. another success return point) is a
        // harmless no-op — never a double-free.
        drop_if_root(&registry, &root_frame);
        assert!(!registry.contains(&tree), "second root drop is a no-op");
    }
}
