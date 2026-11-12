//! Prevent test helpers from bypassing the resource-owning foundation.
//! This checks direct Rust calls/imports, not macro expansion or runtime aliases.
use std::path::Path;
use syn::{
    visit::{self, Visit},
    Expr, Item,
};

pub fn findings(items: &[Item], whole_file: bool, path: &Path) -> Vec<String> {
    // This crate implements the reviewed ownership primitives themselves.
    if path
        .components()
        .any(|p| p.as_os_str() == "awman-test-support")
    {
        return Vec::new();
    }
    let mut scan = Scan {
        in_test: whole_file,
        artifact: path.components().any(|p| p.as_os_str() == "artifact"),
        findings: Vec::new(),
        allowed: Vec::new(),
    };
    for item in items {
        scan.visit_item(item);
    }
    scan.findings.sort();
    scan.findings.dedup();
    scan.findings
}
struct Scan {
    in_test: bool,
    artifact: bool,
    findings: Vec<String>,
    allowed: Vec<String>,
}
impl Scan {
    fn report(&mut self, rule: &str, what: &str) {
        if self.allowed.iter().any(|r| r == rule) {
            return;
        }
        self.findings
            .push(format!("{rule} {what}; use the owned test foundation"));
    }
}
impl<'ast> Visit<'ast> for Scan {
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        let old = self.in_test;
        self.in_test |= super::hygiene::is_cfg_test(&item.attrs);
        visit::visit_item_mod(self, item);
        self.in_test = old;
    }
    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        let old = self.in_test;
        let allowed = std::mem::replace(&mut self.allowed, exceptions(&item.attrs));
        self.in_test |= super::hygiene::is_cfg_test(&item.attrs)
            || item
                .attrs
                .iter()
                .any(|a| a.path().segments.last().is_some_and(|s| s.ident == "test"));
        visit::visit_item_fn(self, item);
        self.in_test = old;
        self.allowed = allowed;
    }
    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        let allowed = std::mem::replace(&mut self.allowed, exceptions(&item.attrs));
        visit::visit_impl_item_fn(self, item);
        self.allowed = allowed;
    }
    fn visit_expr_call(&mut self, e: &'ast syn::ExprCall) {
        if self.in_test {
            if let Expr::Path(p) = e.func.as_ref() {
                let names: Vec<_> = p
                    .path
                    .segments
                    .iter()
                    .map(|s| s.ident.to_string())
                    .collect();
                let last = names.last().map(String::as_str).unwrap_or("");
                let parent = names.iter().rev().nth(1).map(String::as_str).unwrap_or("");
                match (parent, last) {
                    (_, "set_var" | "remove_var" | "set_current_dir") => {
                        self.report("H1", "process-global mutation in test code")
                    }
                    ("Command", "new") => self.report("H2", "unowned subprocess in test code"),
                    (_, "sleep") => self.report("H3", "timing sleep in test code"),
                    ("TcpListener" | "UdpSocket" | "UnixListener", "bind") => {
                        self.report("H4", "unowned listener in test code")
                    }
                    ("env", "var" | "var_os" | "vars" | "vars_os") => {
                        let build_metadata = e.args.first().is_some_and(|a| matches!(a, Expr::Lit(l) if matches!(&l.lit, syn::Lit::Str(s) if s.value() == "CARGO_MANIFEST_DIR")));
                        if !build_metadata {
                            self.report("H7", "ambient environment read in test code");
                        }
                    }
                    _ => {}
                }
            }
        }
        visit::visit_expr_call(self, e);
    }
    fn visit_expr_method_call(&mut self, e: &'ast syn::ExprMethodCall) {
        if self.in_test && e.method == "command" {
            if let Some(Expr::Lit(l)) = e.args.first() {
                if let syn::Lit::Str(s) = &l.lit {
                    if [
                        "claude", "codex", "gemini", "opencode", "pi", "crush", "maki",
                    ]
                    .contains(&s.value().as_str())
                    {
                        self.report("H10", "host agent invocation");
                    }
                }
            }
        }
        visit::visit_expr_method_call(self, e);
    }
    fn visit_use_tree(&mut self, tree: &'ast syn::UseTree) {
        if self.in_test
            && self.artifact
            && matches!(tree, syn::UseTree::Path(p) if p.ident == "awman")
        {
            self.report("H9", "artifact test imports the product library");
        }
        visit::visit_use_tree(self, tree);
    }
}
fn exceptions(attrs: &[syn::Attribute]) -> Vec<String> {
    attrs
        .iter()
        .filter_map(|a| match &a.meta {
            syn::Meta::NameValue(n) if n.path.is_ident("doc") => match &n.value {
                Expr::Lit(l) => match &l.lit {
                    syn::Lit::Str(s) => s
                        .value()
                        .trim()
                        .strip_prefix("hygiene-allow(")
                        .and_then(|s| s.split_once("): "))
                        .filter(|(_, reason)| !reason.trim().is_empty())
                        .map(|(rule, _)| rule.to_string()),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        })
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn helpers_cannot_bypass_owned_resources_but_production_calls_are_not_test_code() {
        let bad = syn::parse_file("fn helper() { std::env::set_var(\"HOME\", \"x\"); std::process::Command::new(\"sh\"); std::thread::sleep(d); TcpListener::bind(addr); std::env::var(\"TOKEN\"); s.command(\"codex\"); } use awman::engine;").unwrap();
        assert!(findings(&bad.items, false, Path::new("src/main.rs")).is_empty());
        let out = findings(&bad.items, true, Path::new("tests/artifact/main.rs"));
        for rule in ["H1", "H2", "H3", "H4", "H7", "H9", "H10"] {
            assert!(out.iter().any(|s| s.starts_with(rule)), "{rule}: {out:?}");
        }
        let owned = syn::parse_file("#[cfg(test)] mod tests { fn helper() { s.command(\"git\"); s.ports().tcp_listener(); } }").unwrap();
        assert!(findings(&owned.items, false, Path::new("src/lib.rs")).is_empty());
    }
}
