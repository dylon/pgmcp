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
}

impl RlmEnvironment {
    /// Parse the tool's `environment` JSON: `{kind:"file",path}` or
    /// `{kind:"corpus",project?}`.
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
            _ => Err("environment.kind must be \"file\", \"file_region\", or \"corpus\"".into()),
        }
    }

    fn project(&self) -> Option<&str> {
        match self {
            RlmEnvironment::Corpus { project } => project.as_deref(),
            RlmEnvironment::File { .. } | RlmEnvironment::FileRegion { .. } => None,
        }
    }

    /// Environment-kind tag for the learning loop's per-kind cohorts (E).
    pub fn kind(&self) -> &'static str {
        match self {
            RlmEnvironment::File { .. } | RlmEnvironment::FileRegion { .. } => "file",
            RlmEnvironment::Corpus { .. } => "corpus",
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
pub const MAX_RLM_DEPTH: u32 = 4;
/// Hard cap on total sub-calls across an entire recursion tree.
pub const MAX_RLM_BUDGET: u32 = 256;

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
    let mut final_answer = subcall(reduce_agent_url, &reduce_prompt, parent_task_id).await?;
    push_step(
        &mut steps,
        &mut ord,
        StepKind::Stitch,
        depth,
        &t,
        est_tokens(&final_answer),
        true,
    );

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
            RlmEnvironment::File { .. } | RlmEnvironment::FileRegion { .. } => {
                DecomposeStrategy::Chunk
            }
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
        })
        .collect())
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
}
