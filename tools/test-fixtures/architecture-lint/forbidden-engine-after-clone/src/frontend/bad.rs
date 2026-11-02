fn poll(engines: Engines) {
    engines.git_engine.clone().diff_summary(root);
}
