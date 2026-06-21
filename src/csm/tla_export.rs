//! `GlobalType` → TLA⁺ encoder (the "global-cursor" model).
//!
//! Renders a protocol's [`GlobalType`] as a deterministic, faithful TLA⁺ module so
//! a downstream consumer (the Crucible `fv-planner`) can model-check it with TLC
//! WITHOUT an LLM authoring the encoding — the faithfulness-critical step becomes a
//! tested Rust function instead of a generate-then-check loop. This is pure analysis
//! (a `GlobalType` in, a TLA⁺ string out — no file I/O, no checker spawned), exactly
//! the boundary discipline of [`crate::csm::string_diagram`].
//!
//! # The model
//! A single global cursor `g` (a state-name string) walks the protocol; a `fired`
//! map records which message labels have fired (so a consumer can layer property
//! assertions like data-dependency ordering on top); and — when the protocol nests
//! ([`GlobalType::GlobalBox`]) or recurses ([`GlobalType::GlobalCall`]) — a `stack`
//! of **return-state names** gives the visibly-pushdown semantics. The keystone is
//! that `g` and the stack entries are both state-name strings, so a frame return is
//! literally `g' = stack[Len(stack)]` — true pushdown control flow, with recursion
//! handled by encoding each callee body once and bounding stack depth (`MaxStack`),
//! so TLC stays finite. The structural obligations the type guarantees — well-nesting
//! (`WellNested`) and bounded depth (`StackBounded`) — are emitted directly.
//!
//! Deadlock-freedom is TLC's built-in check (the terminating `g = "DONE"` self-stutter
//! keeps `DONE` from being a false positive). Liveness/ordering properties are layered
//! by the consumer over the emitted `fired` map; the encoder stays property-agnostic.

use std::collections::{BTreeMap, BTreeSet};

use crate::csm::mpst::global::{GlobalType, ProtocolEnv};

/// What an `End` does in the current encoding context.
#[derive(Clone)]
enum Ret {
    /// Call-free top level: `End` goes to the terminal `DONE` state.
    Done,
    /// A protocol/callee body that may also be entered via a call (recursion): `End`
    /// is stack-dependent — empty stack ⇒ `DONE` (outermost), non-empty ⇒ pop the
    /// frame, emit `ret_label`, and return to the state named on the stack top.
    ToplevelOrReturn { ret_label: String },
    /// An inline box body: `End` always pops (the box pushed on entry), emitting
    /// `exit_label`.
    BoxExit { exit_label: String },
}

struct Encoder<'a> {
    env: &'a ProtocolEnv,
    /// `(name, body)` of each emitted TLA⁺ action, in emission order.
    actions: Vec<(String, String)>,
    /// Every message label that can fire (for the `Labels` set + `fired` init).
    labels: BTreeSet<String>,
    /// Any box/call present ⇒ the model carries a `stack`.
    has_stack: bool,
    /// Any `GlobalCall` present ⇒ recursion ⇒ bounded by `MaxStack`.
    has_call: bool,
    /// Rec variable → its loop-head state name (for `Var` back-edges), lexically scoped.
    rec_heads: BTreeMap<String, String>,
    /// Callee name → its (shared) encoded entry state — encoding each body once is
    /// what makes self-recursion a finite back-edge rather than infinite inlining.
    callee_entries: BTreeMap<String, String>,
    counter: usize,
    error: Option<String>,
}

impl<'a> Encoder<'a> {
    fn fresh(&mut self) -> String {
        let s = format!("s{}", self.counter);
        self.counter += 1;
        s
    }

    fn push_action(&mut self, body: String) {
        let name = format!("Step{}", self.actions.len());
        self.actions.push((name, body));
    }

    /// `UNCHANGED stack` clause iff the model carries a stack (else empty).
    fn keep_stack(&self) -> Option<String> {
        self.has_stack.then(|| "UNCHANGED stack".to_string())
    }

    /// Join conjuncts with the TLA⁺ `/\` operator.
    fn conj(parts: &[String]) -> String {
        parts.join(" /\\ ")
    }

    /// Walk `g`, whose entry state is `st`, emitting actions; `ret` says what its
    /// `End` leaves do.
    fn emit(&mut self, g: &GlobalType, st: String, ret: &Ret) {
        if self.error.is_some() {
            return;
        }
        match g {
            GlobalType::Interaction { label, cont, .. } => {
                let next = self.fresh();
                self.labels.insert(label.name.clone());
                let mut parts = vec![
                    format!("g = \"{st}\""),
                    format!("g' = \"{next}\""),
                    format!("fired' = [fired EXCEPT ![\"{}\"] = 1]", label.name),
                ];
                parts.extend(self.keep_stack());
                self.push_action(Self::conj(&parts));
                self.emit(cont, next, ret);
            }
            GlobalType::Choice { branches, .. } => {
                for b in branches {
                    let bst = self.fresh();
                    self.labels.insert(b.label.name.clone());
                    let mut parts = vec![
                        format!("g = \"{st}\""),
                        format!("g' = \"{bst}\""),
                        format!("fired' = [fired EXCEPT ![\"{}\"] = 1]", b.label.name),
                    ];
                    parts.extend(self.keep_stack());
                    self.push_action(Self::conj(&parts));
                    self.emit(&b.cont, bst, ret);
                }
            }
            GlobalType::Rec { var, body } => {
                // Lexically scoped binding: a `Var` in `body` jumps back to `st`.
                let prev = self.rec_heads.insert(var.clone(), st.clone());
                self.emit(body, st, ret);
                match prev {
                    Some(p) => {
                        self.rec_heads.insert(var.clone(), p);
                    }
                    None => {
                        self.rec_heads.remove(var);
                    }
                }
            }
            GlobalType::Var { var } => {
                let Some(head) = self.rec_heads.get(var).cloned() else {
                    self.error = Some(format!(
                        "free recursion variable `{var}` (not bound by a Rec)"
                    ));
                    return;
                };
                let mut parts = vec![
                    format!("g = \"{st}\""),
                    format!("g' = \"{head}\""),
                    "UNCHANGED fired".to_string(),
                ];
                parts.extend(self.keep_stack());
                self.push_action(Self::conj(&parts));
            }
            GlobalType::End => self.emit_end(st, ret),
            GlobalType::GlobalBox {
                enter,
                body,
                exit,
                cont,
            } => {
                // enter: push the return state (cont), jump into the body.
                let body_st = self.fresh();
                let cont_st = self.fresh();
                self.labels.insert(enter.name.clone());
                self.labels.insert(exit.name.clone());
                // A box's nesting is syntactically bounded, so its enter is UNGUARDED; StackBounded
                // verifies the realized depth stays within MaxStack (an over-low MaxStack fails that
                // invariant rather than deadlocking the enter).
                let parts = vec![
                    format!("g = \"{st}\""),
                    format!("g' = \"{body_st}\""),
                    format!("fired' = [fired EXCEPT ![\"{}\"] = 1]", enter.name),
                    format!("stack' = Append(stack, \"{cont_st}\")"),
                ];
                self.push_action(Self::conj(&parts));
                self.emit(
                    body,
                    body_st,
                    &Ret::BoxExit {
                        exit_label: exit.name.clone(),
                    },
                );
                self.emit(cont, cont_st, ret);
            }
            GlobalType::GlobalCall { callee, cont, .. } => {
                // Resolve (or reuse) the callee's shared entry; encode its body once.
                let cont_st = self.fresh();
                let entry = match self.callee_entries.get(&callee.name) {
                    Some(e) => e.clone(),
                    None => {
                        let e = self.fresh();
                        self.callee_entries.insert(callee.name.clone(), e.clone());
                        match self.env.resolve(callee) {
                            Some(body) => {
                                let body = body.clone();
                                self.emit(
                                    &body,
                                    e.clone(),
                                    &Ret::ToplevelOrReturn {
                                        ret_label: format!("ret:{}", callee.name),
                                    },
                                );
                            }
                            None => {
                                self.error = Some(format!(
                                    "unknown callee `{}` (not in the protocol environment)",
                                    callee.name
                                ));
                                return;
                            }
                        }
                        e
                    }
                };
                let call_label = format!("call:{}", callee.name);
                self.labels.insert(call_label.clone());
                self.labels.insert(format!("ret:{}", callee.name));
                let parts = vec![
                    format!("g = \"{st}\""),
                    "Len(stack) < MaxStack".to_string(),
                    format!("g' = \"{entry}\""),
                    format!("fired' = [fired EXCEPT ![\"{call_label}\"] = 1]"),
                    format!("stack' = Append(stack, \"{cont_st}\")"),
                ];
                self.push_action(Self::conj(&parts));
                // Depth-bound fallback: at MaxStack, TRUNCATE the recursion — skip the call and run
                // its continuation rather than deadlock at the call site. A sound bounded-model
                // approximation that keeps TLC finite; safety obligations are unaffected (no extra
                // frame is pushed, and the existing frames still unwind correctly).
                let skip = format!(
                    "g = \"{st}\" /\\ Len(stack) >= MaxStack /\\ g' = \"{cont_st}\" /\\ UNCHANGED << fired, stack >>"
                );
                self.push_action(skip);
                self.emit(cont, cont_st, ret);
            }
        }
    }

    fn emit_end(&mut self, st: String, ret: &Ret) {
        match ret {
            Ret::Done => {
                let mut parts = vec![
                    format!("g = \"{st}\""),
                    "g' = \"DONE\"".to_string(),
                    "UNCHANGED fired".to_string(),
                ];
                parts.extend(self.keep_stack());
                self.push_action(Self::conj(&parts));
            }
            Ret::BoxExit { exit_label } => {
                self.labels.insert(exit_label.clone());
                let parts = vec![
                    format!("g = \"{st}\""),
                    "Len(stack) > 0".to_string(),
                    "g' = stack[Len(stack)]".to_string(),
                    format!("fired' = [fired EXCEPT ![\"{exit_label}\"] = 1]"),
                    "stack' = SubSeq(stack, 1, Len(stack) - 1)".to_string(),
                ];
                self.push_action(Self::conj(&parts));
            }
            Ret::ToplevelOrReturn { ret_label } => {
                self.labels.insert(ret_label.clone());
                // empty stack ⇒ DONE (outermost); non-empty ⇒ pop & return.
                let done = "stack = << >> /\\ g' = \"DONE\" /\\ UNCHANGED << fired, stack >>";
                let pop = format!(
                    "Len(stack) > 0 /\\ g' = stack[Len(stack)] /\\ stack' = SubSeq(stack, 1, Len(stack) - 1) /\\ fired' = [fired EXCEPT ![\"{ret_label}\"] = 1]"
                );
                let body = format!("g = \"{st}\" /\\ ( ({done}) \\/ ({pop}) )");
                self.push_action(body);
            }
        }
    }

    fn render(&self, module: &str, start: &str) -> String {
        let mut out = String::with_capacity(2048);
        let bar = "-".repeat(28);
        out.push_str(&format!("{bar} MODULE {module} {bar}\n"));
        out.push_str(
            "\\* Generated by csm_protocol_to_tla (GlobalType -> TLA+, global-cursor model).\n",
        );
        if self.has_stack {
            out.push_str("EXTENDS Naturals, Sequences\n");
            out.push_str(
                "CONSTANT MaxStack  \\* bound on call/box nesting depth (keeps TLC finite)\n",
            );
            out.push_str("VARIABLES g, fired, stack\n");
            out.push_str("vars == << g, fired, stack >>\n");
        } else {
            out.push_str("EXTENDS Naturals\n");
            out.push_str("VARIABLES g, fired\n");
            out.push_str("vars == << g, fired >>\n");
        }
        // Labels set
        let labels: Vec<String> = self.labels.iter().map(|l| format!("\"{l}\"")).collect();
        out.push_str(&format!("Labels == {{ {} }}\n", labels.join(", ")));
        // Init
        if self.has_stack {
            out.push_str(&format!(
                "Init == g = \"{start}\" /\\ fired = [l \\in Labels |-> 0] /\\ stack = << >>\n"
            ));
        } else {
            out.push_str(&format!(
                "Init == g = \"{start}\" /\\ fired = [l \\in Labels |-> 0]\n"
            ));
        }
        out.push('\n');
        // Actions
        for (name, body) in &self.actions {
            out.push_str(&format!("{name} == {body}\n"));
        }
        // Next (disjunction + terminating self-stutter at DONE)
        let names: Vec<&str> = self.actions.iter().map(|(n, _)| n.as_str()).collect();
        let disj = if names.is_empty() {
            "FALSE".to_string()
        } else {
            names.join(" \\/ ")
        };
        out.push_str(&format!(
            "\nNext == {disj} \\/ (g = \"DONE\" /\\ UNCHANGED vars)\n"
        ));
        out.push_str("Spec == Init /\\ [][Next]_vars\n");
        if self.has_stack {
            out.push_str("\n\\* Structural obligations the type guarantees (check these):\n");
            out.push_str("WellNested   == (g = \"DONE\") => (stack = << >>)\n");
            out.push_str("StackBounded == Len(stack) <= MaxStack\n");
        }
        out.push_str(
            "\n\\* Deadlock-freedom is TLC's built-in check (the DONE self-stutter is the\n\
             \\* terminating state). Layer property assertions over `fired`, e.g.\n\
             \\*   DepOrder == (fired[\"b_req\"] = 1) => (fired[\"a_done\"] = 1)\n",
        );
        out.push_str(&format!(
            "{bar}{}\n",
            "=".repeat(module.len() + 9 + bar.len())
        ));
        out
    }
}

/// Sanitize an arbitrary protocol name into a valid TLA⁺ module identifier.
fn sanitize_module(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    if s.is_empty() || !s.chars().next().unwrap_or('_').is_ascii_alphabetic() {
        s.insert_str(0, "P_");
    }
    s
}

/// Encode `g` (resolving any [`GlobalType::GlobalCall`] callees through `env`) as a
/// TLA⁺ module named `module`. Returns the module source, or an error string if the
/// protocol references a free recursion variable or an unknown callee.
pub fn encode_tla(g: &GlobalType, env: &ProtocolEnv, module: &str) -> Result<String, String> {
    let module = sanitize_module(module);
    let mut enc = Encoder {
        env,
        actions: Vec::new(),
        labels: BTreeSet::new(),
        has_stack: false,
        has_call: false,
        rec_heads: BTreeMap::new(),
        callee_entries: BTreeMap::new(),
        counter: 0,
        error: None,
    };
    // Pre-pass: does the protocol (and its reachable callees) nest or recurse?
    let mut visited = BTreeSet::new();
    scan(g, env, &mut visited, &mut enc.has_stack, &mut enc.has_call);

    let start = enc.fresh(); // s0
    let ret = if enc.has_call {
        // A self-call must jump back to the top-level entry, so register it first.
        enc.callee_entries.insert(module.clone(), start.clone());
        Ret::ToplevelOrReturn {
            ret_label: format!("ret:{module}"),
        }
    } else {
        Ret::Done
    };
    enc.emit(g, start.clone(), &ret);

    if let Some(e) = enc.error {
        return Err(e);
    }
    Ok(enc.render(&module, &start))
}

/// Pre-pass: set `has_stack` if any box/call is reachable, `has_call` if any call is.
fn scan(
    g: &GlobalType,
    env: &ProtocolEnv,
    visited: &mut BTreeSet<String>,
    has_stack: &mut bool,
    has_call: &mut bool,
) {
    match g {
        GlobalType::Interaction { cont, .. } => scan(cont, env, visited, has_stack, has_call),
        GlobalType::Choice { branches, .. } => {
            for b in branches {
                scan(&b.cont, env, visited, has_stack, has_call);
            }
        }
        GlobalType::Rec { body, .. } => scan(body, env, visited, has_stack, has_call),
        GlobalType::Var { .. } | GlobalType::End => {}
        GlobalType::GlobalBox { body, cont, .. } => {
            *has_stack = true;
            scan(body, env, visited, has_stack, has_call);
            scan(cont, env, visited, has_stack, has_call);
        }
        GlobalType::GlobalCall { callee, cont, .. } => {
            *has_stack = true;
            *has_call = true;
            if visited.insert(callee.name.clone())
                && let Some(body) = env.resolve(callee)
            {
                scan(body, env, visited, has_stack, has_call);
            }
            scan(cont, env, visited, has_stack, has_call);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csm::mpst::global::{choice, end, gbox, gbranch, interaction, rec, var};
    use crate::csm::registry::protocol_env;
    use crate::csm::role::Label;

    fn lbl(s: &str) -> Label {
        Label::text(s)
    }

    #[test]
    fn linear_chain_encodes_to_a_call_free_module() {
        // O -> A : x . A -> O : y . end
        let g = interaction("O", "A", lbl("x"), interaction("A", "O", lbl("y"), end()));
        let m = encode_tla(&g, &ProtocolEnv::new(), "Chain").expect("encodes");
        assert!(m.contains("MODULE Chain"));
        assert!(m.contains("EXTENDS Naturals\n"), "no stack ⇒ no Sequences");
        assert!(!m.contains("stack"), "call-free chain has no stack: {m}");
        assert!(m.contains("Labels == { \"x\", \"y\" }"));
        assert!(m.contains("fired EXCEPT ![\"x\"] = 1"));
        assert!(m.contains("g' = \"DONE\""));
        assert!(m.contains("Spec == Init /\\ [][Next]_vars"));
        // determinism
        assert_eq!(m, encode_tla(&g, &ProtocolEnv::new(), "Chain").unwrap());
    }

    #[test]
    fn choice_emits_one_action_per_branch() {
        // O -> C { a : end ; b : end }
        let g = choice(
            "O",
            "C",
            vec![gbranch(lbl("a"), end()), gbranch(lbl("b"), end())],
        );
        let m = encode_tla(&g, &ProtocolEnv::new(), "Pick").expect("encodes");
        assert!(m.contains("fired EXCEPT ![\"a\"] = 1"));
        assert!(m.contains("fired EXCEPT ![\"b\"] = 1"));
    }

    #[test]
    fn rec_var_emits_a_back_edge_to_the_loop_head() {
        // mu t. O -> A : ping . t   (an infinite ping loop; back-edge to the head)
        let g = rec("t", interaction("O", "A", lbl("ping"), var("t")));
        let m = encode_tla(&g, &ProtocolEnv::new(), "Loop").expect("encodes");
        // the Var jumps back to s0 (the rec head = the chain's first state)
        assert!(m.contains("g' = \"s0\""), "back-edge to the loop head: {m}");
        assert!(m.contains("Labels == { \"ping\" }"));
    }

    #[test]
    fn box_emits_a_stack_with_wellnested_and_stackbounded() {
        // O -> W : task . box<enter>{ W -> T : invoke . T -> W : result }<exit> . end
        let inner = interaction(
            "W",
            "T",
            lbl("invoke"),
            interaction("T", "W", lbl("result"), end()),
        );
        let g = interaction(
            "O",
            "W",
            lbl("task"),
            gbox(lbl("enter"), inner, lbl("exit"), end()),
        );
        let m = encode_tla(&g, &ProtocolEnv::new(), "Hsm").expect("encodes");
        assert!(m.contains("EXTENDS Naturals, Sequences"));
        assert!(m.contains("CONSTANT MaxStack"));
        assert!(m.contains("stack' = Append(stack, "), "box pushes: {m}");
        assert!(
            m.contains("g' = stack[Len(stack)]"),
            "box-body End pops & returns"
        );
        assert!(m.contains("WellNested   == (g = \"DONE\") => (stack = << >>)"));
        assert!(m.contains("StackBounded == Len(stack) <= MaxStack"));
        assert!(m.contains("fired EXCEPT ![\"enter\"] = 1"));
        assert!(m.contains("fired EXCEPT ![\"exit\"] = 1"));
        // box enters are UNGUARDED (nesting is syntactically bounded) — a guarded enter could
        // deadlock at the bound; StackBounded is what verifies the depth.
        assert!(
            !m.contains("Len(stack) < MaxStack"),
            "box enters must be unguarded: {m}"
        );
    }

    #[test]
    fn recursive_cf_encodes_finitely_with_a_call_back_edge() {
        // The registry's genuinely self-calling pushdown protocol must encode WITHOUT
        // infinite inlining (the shared callee entry makes the self-call a back-edge).
        let env = protocol_env();
        let g = env
            .resolve(&crate::csm::mpst::global::ProtocolRef::new("recursive_cf"))
            .expect("recursive_cf is registered")
            .clone();
        let m = encode_tla(&g, &env, "recursive_cf").expect("encodes");
        assert!(m.contains("MODULE recursive_cf"));
        assert!(m.contains("CONSTANT MaxStack"));
        assert!(
            m.contains("call:recursive_cf"),
            "self-call label present: {m}"
        );
        assert!(m.contains("ret:recursive_cf"), "return label present");
        // the stack-dependent End (empty -> DONE ; non-empty -> pop & return)
        assert!(m.contains("g' = \"DONE\""));
        assert!(m.contains("g' = stack[Len(stack)]"));
        // the depth-bound skip fallback must be present so the bounded model never deadlocks at
        // the call site when the stack is full.
        assert!(
            m.contains("Len(stack) >= MaxStack"),
            "recursion needs a depth-bound skip fallback (no deadlock): {m}"
        );
        // finite: a bounded number of actions (no runaway inlining)
        let n_steps = m.matches("\nStep").count();
        assert!(n_steps < 50, "encoding stayed small ({n_steps} steps)");
    }

    #[test]
    fn unknown_callee_is_a_clear_error() {
        use crate::csm::mpst::global::{ProtocolRef, gcall};
        use std::collections::BTreeMap;
        let g = gcall(ProtocolRef::new("nope"), BTreeMap::new(), end());
        let err = encode_tla(&g, &ProtocolEnv::new(), "Bad").unwrap_err();
        assert!(err.contains("unknown callee"), "got: {err}");
    }
}
