//! Repeat real offline boots with independently owned roots.
mod guest {
    #[test]
    fn repeated_boots_use_distinct_state_roots_and_cleanup() {
        let mut roots = std::collections::BTreeSet::new();
        for _ in 0..3 {
            let s = crate::support::soak_sandbox(
                "soak::guest::repeated_boots_use_distinct_state_roots_and_cleanup",
            );
            assert!(roots.insert(crate::support::state(&s)));
            let archive = crate::support::fixture(&s);
            crate::support::watch(&s);
            crate::support::session::boot(&s, crate::support::awman_exe(), &archive);
        }
        assert_eq!(roots.len(), 3);
    }
}
