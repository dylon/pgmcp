//! Rust ordered synchronization-skeleton extraction (`sync_ops`, v21).
//!
//! Walks each function body with `syn` in source order, emitting an ordered
//! [`FunctionSyncOps`]: lock acquire/release (with RAII release synthesis),
//! channel send/recv, task spawn, await points, and `select!` choices. The
//! held-set at any point is recoverable from the ordered `acquire`/`release`
//! ops + `nesting_depth` + `guard_id` pairing, which the lock-order analyzer
//! consumes.
//!
//! **Acquire detection is argument-count-gated** to avoid the obvious
//! method-name collisions: `RwLock::read`/`write` take ZERO args, while
//! `io::Read::read(buf)` / `io::Write::write(buf)` take an argument — so only
//! 0-arg `read`/`write`/`lock` are treated as acquires. Resource identity is
//! best-effort (`self.field` paths and `let`-aliases resolve with high
//! confidence; arbitrary receivers fall to `Unknown`), recorded per op so the
//! analysis layer can discount weak edges.
//!
//! **RAII release model.** A lock guard bound by `let g = m.lock().unwrap();`
//! lives until its enclosing block ends (released LIFO), or earlier on
//! `drop(g)`. An unbound acquire (a temporary, e.g. `m.lock().unwrap().poke();`)
//! is released at the end of its statement. This is intraprocedural and
//! approximates Rust's exact temporary-lifetime rules; guards that escape the
//! function (returned/moved) get no synthesized release and are conservatively
//! treated as held to function end by the analyzer.

use proc_macro2::Span;
use syn::spanned::Spanned;
use syn::{Block, Expr, ImplItem, Item, Member, Pat, Stmt};

use crate::parsing::sync_ops::{
    FunctionSyncOps, ResourceConfidence, ResourceKind, SyncOp, SyncOpKind, SyncParadigm,
};

/// Parse `content` and emit one [`FunctionSyncOps`] per function that contains
/// at least one synchronization op. Parse errors yield `Vec::new()`.
pub fn extract(content: &str) -> Vec<FunctionSyncOps> {
    let file = match syn::parse_file(content) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for item in &file.items {
        collect_item(item, &mut out);
    }
    out
}

fn collect_item(item: &Item, out: &mut Vec<FunctionSyncOps>) {
    match item {
        Item::Fn(f) => push_fn(&f.sig, &f.block, out),
        Item::Impl(im) => {
            for ii in &im.items {
                if let ImplItem::Fn(m) = ii {
                    push_fn(&m.sig, &m.block, out);
                }
            }
        }
        Item::Trait(t) => {
            for ti in &t.items {
                if let syn::TraitItem::Fn(m) = ti
                    && let Some(blk) = &m.default
                {
                    push_fn(&m.sig, blk, out);
                }
            }
        }
        Item::Mod(m) => {
            if let Some((_, items)) = &m.content {
                for it in items {
                    collect_item(it, out);
                }
            }
        }
        _ => {}
    }
}

fn push_fn(sig: &syn::Signature, block: &Block, out: &mut Vec<FunctionSyncOps>) {
    let mut v = SyncVisitor::new(sig.ident.to_string(), line_of(sig.ident.span()));
    v.walk_block(block, 0);
    if !v.ops.is_empty() {
        let end_line = line_of(block.brace_token.span.close());
        out.push(FunctionSyncOps {
            function: v.function,
            start_line: v.start_line,
            end_line,
            ops: v.ops,
        });
    }
}

/// A lock guard awaiting release. Lives on the block-scoped `guard_stack` when
/// named (let-bound) and in a per-statement list when a temporary.
struct PendingGuard {
    guard_id: u32,
    /// Binding name for `drop(name)` matching; `None` for temporaries.
    name: Option<String>,
    resource_key: Option<String>,
    resource_kind: ResourceKind,
    confidence: f32,
    depth: u32,
}

struct SyncVisitor {
    function: String,
    start_line: u32,
    ops: Vec<SyncOp>,
    seq: u32,
    next_guard_id: u32,
    /// Named (let-bound) guards, released LIFO at block exit.
    guard_stack: Vec<PendingGuard>,
    /// `let x = &self.field;` → `x` ⇒ `"self.field"`, so a later `x.lock()`
    /// keys to the field path rather than the local name.
    aliases: std::collections::HashMap<String, String>,
    /// Best-effort cursor for synthesized `release` lines.
    last_line: u32,
}

impl SyncVisitor {
    fn new(function: String, start_line: u32) -> Self {
        Self {
            function,
            start_line,
            ops: Vec::new(),
            seq: 0,
            next_guard_id: 0,
            guard_stack: Vec::new(),
            aliases: std::collections::HashMap::new(),
            last_line: start_line,
        }
    }

    fn alloc_guard(&mut self) -> u32 {
        let g = self.next_guard_id;
        self.next_guard_id += 1;
        g
    }

    #[allow(clippy::too_many_arguments)]
    fn push_op(
        &mut self,
        kind: SyncOpKind,
        rkind: ResourceKind,
        key: Option<String>,
        conf: f32,
        depth: u32,
        guard_id: Option<u32>,
        line: u32,
    ) {
        self.last_line = self.last_line.max(line);
        let paradigm = if kind.is_message() {
            SyncParadigm::Message
        } else {
            SyncParadigm::Lock
        };
        self.ops.push(SyncOp {
            seq: self.seq,
            op_kind: kind,
            resource_kind: rkind,
            paradigm,
            resource_key: key,
            resource_confidence: conf,
            nesting_depth: depth,
            guard_id,
            line,
        });
        self.seq += 1;
    }

    fn emit_release(&mut self, g: &PendingGuard) {
        let line = self.last_line;
        self.push_op(
            SyncOpKind::Release,
            g.resource_kind,
            g.resource_key.clone(),
            g.confidence,
            g.depth,
            Some(g.guard_id),
            line,
        );
    }

    /// Release a batch of temporaries (LIFO), e.g. at end of a statement.
    fn release_temps(&mut self, mut temps: Vec<PendingGuard>) {
        while let Some(g) = temps.pop() {
            self.emit_release(&g);
        }
    }

    /// Release the most-recent named guard with this binding name (`drop(g)`).
    fn release_named(&mut self, name: &str) {
        if let Some(pos) = self
            .guard_stack
            .iter()
            .rposition(|g| g.name.as_deref() == Some(name))
        {
            let g = self.guard_stack.remove(pos);
            self.emit_release(&g);
        }
    }

    fn walk_block(&mut self, block: &Block, depth: u32) {
        let mark = self.guard_stack.len();
        for stmt in &block.stmts {
            self.walk_stmt(stmt, depth);
        }
        // Release guards opened in THIS block, LIFO (RAII order).
        while self.guard_stack.len() > mark {
            let g = self.guard_stack.pop().expect("len checked above");
            self.emit_release(&g);
        }
    }

    fn walk_stmt(&mut self, stmt: &Stmt, depth: u32) {
        match stmt {
            Stmt::Local(local) => {
                let bind = pat_ident(&local.pat);
                if let Some(init) = &local.init {
                    // Alias: `let x = &self.field;` lets later `x.lock()` key to the field.
                    if let (Some(name), Some(path)) = (&bind, field_path_str(peel(&init.expr)))
                        && path.contains('.')
                    {
                        self.aliases.insert(name.clone(), path);
                    }

                    let mut temps: Vec<PendingGuard> = Vec::new();
                    let value_guard = self.walk_expr(&init.expr, depth, &mut temps);

                    // Promote the bound value's acquire to a named (block-scoped) guard.
                    if let (Some(name), Some(gid)) = (&bind, value_guard)
                        && let Some(pos) = temps.iter().position(|t| t.guard_id == gid)
                    {
                        let mut g = temps.remove(pos);
                        g.name = Some(name.clone());
                        self.guard_stack.push(g);
                    }
                    // Remaining temporaries die at end of the let-statement.
                    self.release_temps(temps);

                    // `let ... else { diverge }` block.
                    if let Some((_, diverge)) = &init.diverge {
                        self.walk_expr_scoped(diverge, depth + 1);
                    }
                }
            }
            Stmt::Expr(expr, _) => {
                if let Some(name) = drop_target(expr) {
                    self.release_named(&name);
                    return;
                }
                let mut temps = Vec::new();
                self.walk_expr(expr, depth, &mut temps);
                self.release_temps(temps);
            }
            Stmt::Macro(m) => self.detect_macro(&m.mac, depth),
            Stmt::Item(_) => {}
        }
    }

    /// Walk an expression that owns a fresh temporary scope (a sub-block body,
    /// match arm, etc.), releasing its temporaries at scope end.
    fn walk_expr_scoped(&mut self, expr: &Expr, depth: u32) {
        let mut temps = Vec::new();
        self.walk_expr(expr, depth, &mut temps);
        self.release_temps(temps);
    }

    /// Walk `expr` in source order, recording ops into `self.ops` and pushing
    /// any acquire temporaries into `temps`. Returns the `guard_id` of the
    /// expression's *value* acquire (for `let`-promotion), threading it through
    /// pass-through wrappers (`.unwrap()`, `?`, `.await`, refs).
    fn walk_expr(&mut self, expr: &Expr, depth: u32, temps: &mut Vec<PendingGuard>) -> Option<u32> {
        match expr {
            Expr::MethodCall(mc) => {
                // Source order: receiver, then args, then this call's own op.
                let recv_guard = self.walk_expr(&mc.receiver, depth, temps);
                for a in &mc.args {
                    self.walk_expr(a, depth, temps);
                }
                let name = mc.method.to_string();
                if let Some((kind, rkind)) = classify_method(&name, mc.args.len()) {
                    let (key, conf) = self.resource_key_of(&mc.receiver);
                    let gid = self.alloc_guard();
                    let line = line_of(mc.method.span());
                    let guard_id = kind.is_acquire().then_some(gid);
                    self.push_op(kind, rkind, key.clone(), conf, depth, guard_id, line);
                    if kind.is_acquire() {
                        temps.push(PendingGuard {
                            guard_id: gid,
                            name: None,
                            resource_key: key,
                            resource_kind: rkind,
                            confidence: conf,
                            depth,
                        });
                        return Some(gid);
                    }
                    return None;
                }
                // Guard-preserving wrappers keep the receiver's value acquire.
                if is_passthrough(&name) {
                    return recv_guard;
                }
                None
            }
            Expr::Await(ea) => {
                let inner = self.walk_expr(&ea.base, depth, temps);
                self.push_op(
                    SyncOpKind::Await,
                    ResourceKind::Unknown,
                    None,
                    ResourceConfidence::Unknown.value(),
                    depth,
                    None,
                    line_of(ea.await_token.span()),
                );
                inner
            }
            Expr::Try(t) => self.walk_expr(&t.expr, depth, temps),
            Expr::Reference(r) => self.walk_expr(&r.expr, depth, temps),
            Expr::Paren(p) => self.walk_expr(&p.expr, depth, temps),
            Expr::Group(g) => self.walk_expr(&g.expr, depth, temps),
            Expr::Unary(u) => self.walk_expr(&u.expr, depth, temps),
            Expr::Call(call) => {
                if let Some((kind, rkind)) = classify_call(&call.func) {
                    self.push_op(
                        kind,
                        rkind,
                        None,
                        ResourceConfidence::Unknown.value(),
                        depth,
                        None,
                        line_of(call.func.span()),
                    );
                }
                for a in &call.args {
                    self.walk_expr(a, depth, temps);
                }
                None
            }
            Expr::Macro(m) => {
                self.detect_macro(&m.mac, depth);
                None
            }
            Expr::Let(l) => self.walk_expr(&l.expr, depth, temps),
            Expr::Field(f) => self.walk_expr(&f.base, depth, temps),
            Expr::Binary(b) => {
                self.walk_expr(&b.left, depth, temps);
                self.walk_expr(&b.right, depth, temps);
                None
            }
            Expr::Assign(a) => {
                self.walk_expr(&a.right, depth, temps);
                self.walk_expr(&a.left, depth, temps);
                None
            }
            Expr::Tuple(t) => {
                for e in &t.elems {
                    self.walk_expr(e, depth, temps);
                }
                None
            }
            Expr::Array(a) => {
                for e in &a.elems {
                    self.walk_expr(e, depth, temps);
                }
                None
            }
            Expr::Return(r) => {
                if let Some(e) = &r.expr {
                    self.walk_expr(e, depth, temps);
                }
                None
            }
            Expr::Cast(c) => self.walk_expr(&c.expr, depth, temps),
            // Control flow: nested bodies live one block deeper.
            Expr::If(ei) => {
                self.walk_expr(&ei.cond, depth, temps);
                self.walk_block(&ei.then_branch, depth + 1);
                if let Some((_, else_e)) = &ei.else_branch {
                    self.walk_expr_scoped(else_e, depth + 1);
                }
                None
            }
            Expr::Block(b) => {
                self.walk_block(&b.block, depth + 1);
                None
            }
            Expr::Unsafe(u) => {
                self.walk_block(&u.block, depth + 1);
                None
            }
            Expr::Async(a) => {
                self.walk_block(&a.block, depth + 1);
                None
            }
            Expr::Loop(l) => {
                self.walk_block(&l.body, depth + 1);
                None
            }
            Expr::While(w) => {
                self.walk_expr_scoped(&w.cond, depth);
                self.walk_block(&w.body, depth + 1);
                None
            }
            Expr::ForLoop(f) => {
                self.walk_expr_scoped(&f.expr, depth);
                self.walk_block(&f.body, depth + 1);
                None
            }
            Expr::Match(m) => {
                self.walk_expr_scoped(&m.expr, depth);
                for arm in &m.arms {
                    self.walk_expr_scoped(&arm.body, depth + 1);
                }
                None
            }
            _ => None,
        }
    }

    /// Resolve a `select!` / `tokio::select!` macro to a `Select` op. (We do not
    /// parse the macro's token stream for the inner sends/recvs in v1.)
    fn detect_macro(&mut self, mac: &syn::Macro, depth: u32) {
        if let Some(seg) = mac.path.segments.last()
            && seg.ident == "select"
        {
            self.push_op(
                SyncOpKind::Select,
                ResourceKind::Channel,
                None,
                ResourceConfidence::Unknown.value(),
                depth,
                None,
                line_of(seg.ident.span()),
            );
        }
    }

    /// Best-effort static identity + confidence tier for a lock/channel receiver.
    fn resource_key_of(&self, expr: &Expr) -> (Option<String>, f32) {
        let e = peel(expr);
        match field_path_str(e) {
            Some(path) if path.contains('.') => (Some(path), ResourceConfidence::FieldPath.value()),
            Some(ident) => match self.aliases.get(&ident) {
                Some(resolved) => (
                    Some(resolved.clone()),
                    ResourceConfidence::FieldPath.value(),
                ),
                None => (Some(ident), ResourceConfidence::LocalBinding.value()),
            },
            None => (None, ResourceConfidence::Unknown.value()),
        }
    }
}

/// Acquire/channel classification, argument-count-gated to dodge `io::Read`/
/// `io::Write` collisions (those take an argument; the lock methods take none).
fn classify_method(name: &str, argc: usize) -> Option<(SyncOpKind, ResourceKind)> {
    match (name, argc) {
        ("lock", 0) | ("try_lock", 0) => Some((SyncOpKind::Acquire, ResourceKind::Mutex)),
        ("read", 0) | ("try_read", 0) => Some((SyncOpKind::AcquireRead, ResourceKind::Rwlock)),
        ("write", 0) | ("try_write", 0) => Some((SyncOpKind::AcquireWrite, ResourceKind::Rwlock)),
        ("send", _) | ("try_send", _) | ("send_timeout", _) | ("blocking_send", _) => {
            Some((SyncOpKind::Send, ResourceKind::Channel))
        }
        ("recv", 0) | ("try_recv", 0) | ("blocking_recv", 0) => {
            Some((SyncOpKind::Recv, ResourceKind::Channel))
        }
        _ => None,
    }
}

/// Free-function spawn calls (`thread::spawn`, `tokio::spawn`, `task::spawn`).
fn classify_call(func: &Expr) -> Option<(SyncOpKind, ResourceKind)> {
    if let Expr::Path(p) = func
        && let Some(last) = p.path.segments.last()
        && (last.ident == "spawn" || last.ident == "spawn_blocking")
    {
        return Some((SyncOpKind::Spawn, ResourceKind::Task));
    }
    None
}

/// Method names that pass the receiver's lock guard through unchanged.
fn is_passthrough(name: &str) -> bool {
    matches!(name, "unwrap" | "expect" | "unwrap_unchecked")
}

/// `drop(g)` → the dropped binding name, if a bare identifier.
fn drop_target(expr: &Expr) -> Option<String> {
    if let Expr::Call(c) = expr
        && let Expr::Path(p) = c.func.as_ref()
        && p.path.is_ident("drop")
        && c.args.len() == 1
        && let Some(Expr::Path(arg)) = c.args.first()
    {
        return arg.path.get_ident().map(|i| i.to_string());
    }
    None
}

/// Strip references/parens/groups/derefs to the inner place expression.
fn peel(expr: &Expr) -> &Expr {
    match expr {
        Expr::Reference(r) => peel(&r.expr),
        Expr::Paren(p) => peel(&p.expr),
        Expr::Group(g) => peel(&g.expr),
        Expr::Unary(u) if matches!(u.op, syn::UnOp::Deref(_)) => peel(&u.expr),
        other => other,
    }
}

/// Dotted access-path string for a pure path/field chain (`self.a.b`, `x`);
/// `None` for anything else.
fn field_path_str(e: &Expr) -> Option<String> {
    match e {
        Expr::Path(p) => p.path.get_ident().map(|i| i.to_string()),
        Expr::Field(f) => {
            let base = field_path_str(&f.base)?;
            let seg = match &f.member {
                Member::Named(n) => n.to_string(),
                Member::Unnamed(i) => i.index.to_string(),
            };
            Some(format!("{base}.{seg}"))
        }
        _ => None,
    }
}

fn line_of(span: Span) -> u32 {
    span.start().line as u32
}

fn pat_ident(pat: &Pat) -> Option<String> {
    match pat {
        Pat::Ident(pi) => Some(pi.ident.to_string()),
        Pat::Type(pt) => pat_ident(&pt.pat),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ops_of(src: &str) -> Vec<SyncOp> {
        let fns = extract(src);
        fns.into_iter().flat_map(|f| f.ops).collect()
    }

    #[test]
    fn two_named_locks_held_together() {
        // `a` is still held when `b` is acquired → the held-set walk will emit a→b.
        let src = r#"
            impl S {
                fn f(&self) {
                    let a = self.m1.lock().unwrap();
                    let b = self.m2.lock().unwrap();
                    drop(b);
                    drop(a);
                }
            }
        "#;
        let ops = ops_of(src);
        let acquires: Vec<_> = ops.iter().filter(|o| o.op_kind.is_acquire()).collect();
        assert_eq!(acquires.len(), 2, "two lock acquires");
        assert_eq!(acquires[0].resource_key.as_deref(), Some("self.m1"));
        assert_eq!(acquires[1].resource_key.as_deref(), Some("self.m2"));
        assert!(
            acquires
                .iter()
                .all(|o| o.resource_kind == ResourceKind::Mutex)
        );
        // Two releases (explicit drops), each paired by guard_id.
        let releases: Vec<_> = ops
            .iter()
            .filter(|o| o.op_kind == SyncOpKind::Release)
            .collect();
        assert_eq!(releases.len(), 2);
    }

    #[test]
    fn rwlock_read_write_argcount_gated() {
        // 0-arg read/write are rwlock acquires; the io-style write(buf) is NOT.
        let src = r#"
            fn f(rw: &X, w: &mut Y, buf: &[u8]) {
                let g = rw.write().unwrap();
                let r = rw.read().unwrap();
                w.write(buf).unwrap();   // io::Write — 1 arg — must be ignored
            }
        "#;
        let ops = ops_of(src);
        let kinds: Vec<_> = ops
            .iter()
            .filter(|o| o.op_kind.is_acquire())
            .map(|o| o.op_kind)
            .collect();
        assert_eq!(
            kinds,
            vec![SyncOpKind::AcquireWrite, SyncOpKind::AcquireRead],
            "only the 0-arg rwlock acquires are recorded; io write(buf) is excluded"
        );
    }

    #[test]
    fn channel_send_recv_and_spawn() {
        let src = r#"
            async fn f(tx: Tx, rx: Rx) {
                tx.send(1).await;
                let _ = rx.recv().await;
                tokio::spawn(async {});
            }
        "#;
        let ops = ops_of(src);
        assert!(ops.iter().any(|o| o.op_kind == SyncOpKind::Send));
        assert!(ops.iter().any(|o| o.op_kind == SyncOpKind::Recv));
        assert!(ops.iter().any(|o| o.op_kind == SyncOpKind::Spawn));
        assert!(
            ops.iter()
                .filter(|o| o.op_kind == SyncOpKind::Send || o.op_kind == SyncOpKind::Recv)
                .all(|o| o.paradigm == SyncParadigm::Message)
        );
    }

    #[test]
    fn temporary_lock_released_at_statement_end() {
        let src = r#"
            impl S { fn f(&self) { self.m.lock().unwrap().clear(); } }
        "#;
        let ops = ops_of(src);
        let acq = ops.iter().filter(|o| o.op_kind.is_acquire()).count();
        let rel = ops
            .iter()
            .filter(|o| o.op_kind == SyncOpKind::Release)
            .count();
        assert_eq!(acq, 1);
        assert_eq!(rel, 1, "the temporary guard is released at statement end");
    }

    #[test]
    fn alias_resolves_to_field_path() {
        let src = r#"
            impl S { fn f(&self) { let m = &self.state; let g = m.lock().unwrap(); drop(g); } }
        "#;
        let ops = ops_of(src);
        let acq = ops
            .iter()
            .find(|o| o.op_kind.is_acquire())
            .expect("acquire");
        assert_eq!(acq.resource_key.as_deref(), Some("self.state"));
        assert!((acq.resource_confidence - ResourceConfidence::FieldPath.value()).abs() < 1e-6);
    }

    #[test]
    fn no_sync_ops_yields_no_function() {
        let src = r#"fn pure(a: i32) -> i32 { a + 1 }"#;
        assert!(extract(src).is_empty());
    }
}
