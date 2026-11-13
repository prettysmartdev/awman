//! Structural H5 guard (design 9.2 and 16.2): a test must not be able to
//! report success without reaching its assertions.
//!
//! Rules, applied to every `#[test]` function and to the test-code helpers
//! it calls in the same file:
//!
//! - an early `return` (including `let … else { return }`) that comes before
//!   one of the function's assertions;
//! - a `return` after printing a "skip" message (the classic early-return skip);
//! - assertions only inside an `if` (or `if let Ok/Some`) with no `else`, so a
//!   missing capability or a swallowed error silently skips them;
//! - `#[ignore]` without a `"not-applicable: <reason>"` string.
//!
//! "Assertion" means an `assert*`/`prop_assert*`/`panic!`/`unreachable!` macro
//! or an `unwrap`/`expect`-family call. A conditional inside a loop body, or on
//! `cfg!(…)`, is a per-item or per-platform check, not a capability skip.
//! Syntax alone cannot prove that the assertions are adequate; the named
//! contract tests and the seeded defects complement this scan.
use syn::{
    visit::{self, Visit},
    Expr, ItemFn,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Event {
    Return,
    SkipMessage,
    Assertion,
}

#[derive(Default)]
struct Scan {
    events: Vec<Event>,
    findings: Vec<String>,
    loop_depth: usize,
    closure_depth: usize,
    conditional_depth: usize,
    unconditional_assertion: bool,
    block_asserted: Vec<bool>,
}

fn is_assert_macro(name: &str) -> bool {
    name.starts_with("assert")
        || name.starts_with("prop_assert")
        || ["panic", "unreachable", "todo", "unimplemented"].contains(&name)
}

fn is_print_macro(name: &str) -> bool {
    [
        "println", "eprintln", "print", "eprint", "writeln", "info", "warn", "debug",
    ]
    .contains(&name)
}

/// True when `block` contains an assertion macro outside nested closures' returns.
fn has_assertion_macro(block: &syn::Block) -> bool {
    struct Find(bool);
    impl<'ast> Visit<'ast> for Find {
        fn visit_macro(&mut self, m: &'ast syn::Macro) {
            if m.path
                .segments
                .last()
                .is_some_and(|s| is_assert_macro(&s.ident.to_string()))
            {
                self.0 = true;
            }
            visit::visit_macro(self, m);
        }
        fn visit_item_fn(&mut self, _: &'ast ItemFn) {}
    }
    let mut f = Find(false);
    f.visit_block(block);
    f.0
}

fn is_cfg_macro(e: &Expr) -> bool {
    match e {
        Expr::Macro(m) => m.mac.path.is_ident("cfg"),
        Expr::Unary(u) => is_cfg_macro(&u.expr),
        Expr::Paren(p) => is_cfg_macro(&p.expr),
        Expr::Binary(b) => is_cfg_macro(&b.left) && is_cfg_macro(&b.right),
        _ => false,
    }
}

impl<'ast> Visit<'ast> for Scan {
    fn visit_expr_match(&mut self, m: &'ast syn::ExprMatch) {
        // Assertions in an earlier arm cannot prove a later capability arm.
        // Preserve each arm's control flow instead of flattening sibling events.
        self.visit_expr(&m.expr);
        let asserting_arm = m.arms.iter().any(|arm| {
            let mut scan = Scan::default();
            scan.visit_expr(&arm.body);
            scan.events.contains(&Event::Assertion)
        });
        for arm in &m.arms {
            let mut scan = Scan {
                loop_depth: self.loop_depth,
                ..Scan::default()
            };
            scan.visit_arm(arm);
            if asserting_arm
                && scan.events.contains(&Event::Return)
                && !scan.events.contains(&Event::Assertion)
            {
                self.findings
                    .push("H5 match arm returns without reaching the contract assertions".into());
            }
            self.events.extend(scan.events.iter().copied());
            self.findings.extend(scan.finish());
        }
    }
    fn visit_block(&mut self, block: &'ast syn::Block) {
        self.block_asserted.push(false);
        visit::visit_block(self, block);
        self.block_asserted.pop();
    }
    fn visit_item_fn(&mut self, _: &'ast ItemFn) {
        // A nested fn item is not this test's body.
    }
    fn visit_expr_closure(&mut self, c: &'ast syn::ExprClosure) {
        self.closure_depth += 1;
        visit::visit_expr_closure(self, c);
        self.closure_depth -= 1;
    }
    fn visit_expr_async(&mut self, a: &'ast syn::ExprAsync) {
        self.closure_depth += 1;
        visit::visit_expr_async(self, a);
        self.closure_depth -= 1;
    }
    fn visit_expr_for_loop(&mut self, l: &'ast syn::ExprForLoop) {
        self.loop_depth += 1;
        visit::visit_expr_for_loop(self, l);
        self.loop_depth -= 1;
    }
    fn visit_expr_while(&mut self, l: &'ast syn::ExprWhile) {
        self.loop_depth += 1;
        visit::visit_expr_while(self, l);
        self.loop_depth -= 1;
    }
    fn visit_expr_loop(&mut self, l: &'ast syn::ExprLoop) {
        self.loop_depth += 1;
        visit::visit_expr_loop(self, l);
        self.loop_depth -= 1;
    }
    fn visit_expr_break(&mut self, b: &'ast syn::ExprBreak) {
        if self.closure_depth == 0 && self.events.contains(&Event::SkipMessage) {
            self.events.push(Event::Return);
        }
        visit::visit_expr_break(self, b);
    }
    fn visit_expr_return(&mut self, r: &'ast syn::ExprReturn) {
        if self.closure_depth == 0 && self.loop_depth == 0 && !self.block_asserted.last().copied().unwrap_or(false) && (r.expr.is_none() || r.expr.as_ref().is_some_and(|e| matches!(e.as_ref(), Expr::Call(c) if matches!(c.func.as_ref(), Expr::Path(p) if p.path.is_ident("Ok"))))) {
            self.events.push(Event::Return);
        }
        visit::visit_expr_return(self, r);
    }
    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        if let Some(name) = m.path.segments.last().map(|s| s.ident.to_string()) {
            if is_assert_macro(&name) {
                self.events.push(Event::Assertion);
                if let Some(asserted) = self.block_asserted.last_mut() {
                    *asserted = true;
                }
                if self.conditional_depth == 0 {
                    self.unconditional_assertion = true;
                }
            } else if is_print_macro(&name)
                && m.tokens.to_string().to_lowercase().contains("skip")
                && self.closure_depth == 0
            {
                self.events.push(Event::SkipMessage);
            }
        }
        visit::visit_macro(self, m);
    }
    fn visit_expr_method_call(&mut self, c: &'ast syn::ExprMethodCall) {
        let name = c.method.to_string();
        if ["unwrap", "expect", "unwrap_err", "expect_err"].contains(&name.as_str()) {
            self.events.push(Event::Assertion);
        }
        visit::visit_expr_method_call(self, c);
    }
    fn visit_stmt(&mut self, s: &'ast syn::Stmt) {
        // `operation().ok();` / `let _ = operation().ok();` throw the error away.
        let discarded = match s {
            syn::Stmt::Expr(Expr::MethodCall(m), Some(_)) => Some(m),
            syn::Stmt::Local(l) if matches!(l.pat, syn::Pat::Wild(_)) => match l.init.as_ref() {
                Some(init) => match init.expr.as_ref() {
                    Expr::MethodCall(m) => Some(m),
                    _ => None,
                },
                None => None,
            },
            _ => None,
        };
        if discarded.is_some_and(|m| m.method == "ok") && self.closure_depth == 0 {
            self.findings
                .push("H5 `.ok()` discards an error the test never checks".into());
        }
        visit::visit_stmt(self, s);
    }
    fn visit_expr_if(&mut self, i: &'ast syn::ExprIf) {
        if i.else_branch.is_none()
            && self.loop_depth == 0
            && !is_cfg_macro(&i.cond)
            && has_assertion_macro(&i.then_branch)
        {
            let what = match i.cond.as_ref() {
                Expr::Let(l)
                    if matches!(l.pat.as_ref(), syn::Pat::TupleStruct(p)
                        if p.path.segments.last().is_some_and(|s| s.ident == "Ok" || s.ident == "Some")) =>
                {
                    "H5 assertions only run when `if let Ok/Some` matches (swallowed error or optional capability)"
                }
                _ => "H5 conditional assertions without a failing alternative",
            };
            self.findings.push(what.into());
        }
        self.conditional_depth += 1;
        visit::visit_expr_if(self, i);
        self.conditional_depth -= 1;
    }
}

impl Scan {
    fn finish(mut self) -> Vec<String> {
        for (i, e) in self.events.iter().enumerate() {
            if *e != Event::Return {
                continue;
            }
            if self.events[..i].contains(&Event::SkipMessage) {
                self.findings
                    .push("H5 early-return skip: returns after printing a skip message".into());
            }
            if self.events[i + 1..].contains(&Event::Assertion) {
                self.findings
                    .push("H5 early return before the test's assertions".into());
            }
        }
        if self.unconditional_assertion {
            self.findings.retain(|f| {
                !f.starts_with("H5 conditional assertions")
                    && !f.starts_with("H5 assertions only run when")
            });
        }
        self.findings.sort();
        self.findings.dedup();
        self.findings
    }
}

fn is_test_fn(f: &ItemFn) -> bool {
    f.attrs
        .iter()
        .any(|a| a.path().segments.last().is_some_and(|s| s.ident == "test"))
}

pub fn test_findings(f: &ItemFn) -> Vec<String> {
    let mut scan = Scan::default();
    scan.visit_block(&f.block);
    let mut findings = scan.finish();
    if guaranteed_assertion(&f.block) {
        findings.retain(|f| {
            !f.starts_with("H5 conditional assertions")
                && !f.starts_with("H5 assertions only run when")
        });
    }
    for a in &f.attrs {
        if a.path().is_ident("ignore") {
            let reasoned = match &a.meta {
                syn::Meta::NameValue(n) => matches!(&n.value,
                    Expr::Lit(l) if matches!(&l.lit, syn::Lit::Str(s) if s.value().starts_with("not-applicable:"))),
                _ => false,
            };
            if !reasoned {
                findings.push(
                    "H5 #[ignore] needs a \"not-applicable: <reason>\" string; required cases cannot skip"
                        .into(),
                );
            }
        }
    }
    findings
}

/// Helper scan for a helper (a non-test fn in test code): the same structural
/// rules, without the `#[ignore]` rule.
fn helper_scan(f: &ItemFn) -> Vec<String> {
    let mut scan = Scan::default();
    scan.visit_block(&f.block);
    let mut findings = scan.finish();
    if guaranteed_assertion(&f.block) {
        findings.retain(|f| {
            !f.starts_with("H5 conditional assertions")
                && !f.starts_with("H5 assertions only run when")
        });
    }
    findings
}

fn guaranteed_assertion(block: &syn::Block) -> bool {
    fn expr(e: &Expr) -> bool {
        match e {
            Expr::Macro(m) => m
                .mac
                .path
                .segments
                .last()
                .is_some_and(|s| is_assert_macro(&s.ident.to_string())),
            Expr::Block(b) => guaranteed_assertion(&b.block),
            Expr::If(i) => {
                guaranteed_assertion(&i.then_branch)
                    && i.else_branch.as_ref().is_some_and(|(_, e)| expr(e))
            }
            Expr::Match(m) => !m.arms.is_empty() && m.arms.iter().all(|a| expr(&a.body)),
            _ => false,
        }
    }
    block.stmts.iter().any(|s| match s {
        syn::Stmt::Expr(e, _) => expr(e),
        syn::Stmt::Macro(m) => m
            .mac
            .path
            .segments
            .last()
            .is_some_and(|s| is_assert_macro(&s.ident.to_string())),
        _ => false,
    })
}

/// True for `#[cfg(test)]` and `#[cfg(all(test, …))]`.
pub fn is_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.path().is_ident("cfg")
            && a.parse_args::<syn::Meta>().is_ok_and(|m| match m {
                syn::Meta::Path(p) => p.is_ident("test"),
                syn::Meta::List(l) => {
                    l.path.is_ident("all")
                        && l.tokens.to_string().split(',').any(|t| t.trim() == "test")
                }
                _ => false,
            })
    })
}

/// Follow file-local helpers called from tests (and `use … as …` aliases).
/// Only helpers that are test code are scanned: every function of an
/// integration-test file (`whole_file_is_test_code`), or functions inside a
/// `#[cfg(test)]` module of a production file. Production functions a test
/// calls are product code, not part of the test's control flow contract.
/// Cross-file helper identities still require compiled contract records.
pub fn helper_findings(items: &[syn::Item], whole_file_is_test_code: bool) -> Vec<String> {
    use std::collections::{BTreeMap, BTreeSet};
    fn collect(
        items: &[syn::Item],
        in_test_code: bool,
        functions: &mut BTreeMap<String, (ItemFn, bool)>,
        aliases: &mut BTreeMap<String, String>,
    ) {
        fn uses(tree: &syn::UseTree, aliases: &mut BTreeMap<String, String>) {
            match tree {
                syn::UseTree::Rename(r) => {
                    aliases.insert(r.rename.to_string(), r.ident.to_string());
                }
                syn::UseTree::Path(p) => uses(&p.tree, aliases),
                syn::UseTree::Group(g) => {
                    for t in &g.items {
                        uses(t, aliases)
                    }
                }
                _ => {}
            }
        }
        for item in items {
            match item {
                syn::Item::Fn(f) => {
                    let test_code = in_test_code || is_cfg_test(&f.attrs);
                    functions.insert(f.sig.ident.to_string(), (f.clone(), test_code));
                }
                syn::Item::Use(u) => uses(&u.tree, aliases),
                syn::Item::Mod(m) => {
                    if let Some((_, items)) = &m.content {
                        collect(
                            items,
                            in_test_code || is_cfg_test(&m.attrs),
                            functions,
                            aliases,
                        )
                    }
                }
                _ => {}
            }
        }
    }
    #[derive(Default)]
    struct Calls(Vec<String>);
    impl<'ast> Visit<'ast> for Calls {
        fn visit_expr_call(&mut self, c: &'ast syn::ExprCall) {
            if let Expr::Path(p) = c.func.as_ref() {
                if let Some(s) = p.path.segments.last() {
                    self.0.push(s.ident.to_string());
                }
            }
            visit::visit_expr_call(self, c);
        }
    }
    fn follow(
        name: &str,
        functions: &BTreeMap<String, (ItemFn, bool)>,
        aliases: &BTreeMap<String, String>,
        seen: &mut BTreeSet<String>,
        out: &mut Vec<String>,
    ) {
        let name = aliases.get(name).map_or(name, String::as_str);
        if !seen.insert(name.into()) {
            return;
        }
        if let Some((f, _)) = functions.get(name) {
            let mut calls = Calls::default();
            calls.visit_block(&f.block);
            for call in calls.0 {
                let call = aliases.get(&call).map_or(call.clone(), Clone::clone);
                if let Some((helper, true)) = functions.get(&call) {
                    if !is_test_fn(helper) {
                        for finding in helper_scan(helper) {
                            out.push(format!("helper {call}: {finding}"));
                        }
                    }
                    follow(&call, functions, aliases, seen, out);
                }
            }
        }
    }
    let mut functions = BTreeMap::new();
    let mut aliases = BTreeMap::new();
    collect(items, whole_file_is_test_code, &mut functions, &mut aliases);
    let mut out = Vec::new();
    for (name, (f, _)) in &functions {
        if is_test_fn(f) {
            follow(name, &functions, &aliases, &mut BTreeSet::new(), &mut out);
        }
    }
    // A helper in a separate module may be reached through a qualified path.
    // Scan explicitly named assertion helpers at their definition as well as local call sites;
    // production helpers remain outside this test-control-flow rule.
    for (name, (f, test_code)) in &functions {
        if *test_code
            && !is_test_fn(f)
            && ["assert_", "check_", "verify_"]
                .iter()
                .any(|prefix| name.starts_with(prefix))
        {
            for finding in helper_scan(f) {
                out.push(format!("helper {name}: {finding}"));
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn findings(src: &str) -> Vec<String> {
        let f: ItemFn = syn::parse_str(src).unwrap();
        test_findings(&f)
    }

    #[test]
    fn early_return_skip_is_found() {
        let f = findings(
            r#"#[test] fn t() { if !docker() { eprintln!("SKIP: no docker"); return; } assert!(run()); }"#,
        );
        assert!(f.iter().any(|m| m.contains("early-return skip")), "{f:?}");
        assert!(
            f.iter().any(|m| m.contains("before the test's assertions")),
            "{f:?}"
        );
    }

    #[test]
    fn a_later_match_arm_cannot_borrow_an_earlier_arms_assertion() {
        let f = findings(
            "#[test] fn t() { match probe() { Some(v) => { assert!(v); }, None => return } }",
        );
        assert!(f.iter().any(|m| m.contains("match arm returns")), "{f:?}");
    }

    #[test]
    fn a_qualified_cross_file_helper_is_checked_at_its_definition() {
        let syntax = syn::parse_file("pub fn assert_contract() { if !available() { eprintln!(\"SKIP\"); return; } assert!(ok()); }").unwrap();
        assert!(helper_findings(&syntax.items, true)
            .iter()
            .any(|m| m.contains("early-return skip")));
    }

    #[test]
    fn silent_capability_conditional_is_found() {
        let f = findings("#[test] fn t() { if available() { assert_eq!(a(), 1); } }");
        assert_eq!(
            f,
            ["H5 conditional assertions without a failing alternative"]
        );
        let f = findings("#[test] fn t() { if let Ok(v) = probe() { assert!(v); } }");
        assert!(f[0].contains("if let Ok/Some"), "{f:?}");
    }

    #[test]
    fn let_else_return_before_assertions_is_found() {
        let f = findings("#[test] fn t() { let Some(x) = find() else { return }; assert!(x); }");
        assert_eq!(f, ["H5 early return before the test's assertions"]);
    }

    #[test]
    fn ordinary_tests_pass() {
        assert!(findings("#[test] fn t() { let v = f().ok(); assert_eq!(v, None); }").is_empty());
        assert!(
            findings("#[test] fn t() { for r in rows() { if r.on { assert!(r.ok) } } }").is_empty()
        );
        assert!(findings("#[test] fn t() { if cfg!(unix) { assert!(x()) } }").is_empty());
        assert!(findings("#[test] fn t() -> R { assert!(a()); return Ok(()); }").is_empty());
        assert!(findings(
            "#[test] fn t() { let w = wait(|| { if x() { return true } false }); assert!(w); }"
        )
        .is_empty());
    }

    #[test]
    fn ignore_needs_a_not_applicable_reason() {
        let f = findings("#[test] #[ignore] fn t() { assert!(true) }");
        assert_eq!(f.len(), 1);
        assert!(findings(
            r#"#[test] #[ignore = "not-applicable: macOS only"] fn t() { assert!(true) }"#
        )
        .is_empty());
    }

    #[test]
    fn helpers_are_followed_only_in_test_code() {
        let file: syn::File = syn::parse_str(
            "fn product() { if !x() { return } assert!(y()) }
             #[cfg(test)] mod tests {
                 fn helper() { if !x() { return } assert!(y()) }
                 #[test] fn t() { helper(); product(); }
             }",
        )
        .unwrap();
        let out = helper_findings(&file.items, false);
        assert_eq!(
            out,
            ["helper helper: H5 early return before the test's assertions"]
        );
        let aliased: syn::File = syn::parse_str(
            "use self::check as verify;
             fn check() { if !x() { return } assert!(y()) }
             #[test] fn t() { verify(); }",
        )
        .unwrap();
        assert_eq!(helper_findings(&aliased.items, true).len(), 1);
    }
    #[test]
    fn asserting_branches_and_polling_success_returns_are_not_skips() {
        assert!(
            findings("#[test] fn t() { if mode() { assert!(a()); return; } assert!(b()); }")
                .is_empty()
        );
        assert!(findings(
            "#[test] fn t() { for _ in 0..20 { if ready() { return; } } panic!(\"never ready\"); }"
        )
        .is_empty());
    }
    #[test]
    fn printed_loop_break_before_assertions_is_a_skip() {
        let found = findings("#[test] fn t() { loop { if !cap() { eprintln!(\"skip unavailable\"); break; } assert!(run()); break; } }");
        assert!(found.iter().any(|f| f.contains("skip")), "{found:?}");
    }
}
