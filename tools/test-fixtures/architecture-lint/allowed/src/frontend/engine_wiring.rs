// A clone into a Layer 2 constructor is wiring; `engines.git_engine.diff_summary(` here is prose.
fn wiring(ctx: Ctx, engines: Engines) {
    let poller = GitSummaryPoller::new(ctx.engines.git_engine.clone());
    let sampler = ContainerStatsSampler::new(engines.runtime.clone(), handle);
    let notices = engines.take_runtime_notices();
    let global = engines.global_config.load();
    let wrapped = Sampler::new(
        self.engines
            .runtime
            .clone(),
    );
}

#[cfg(test)]
mod tests {
    fn fixture(engines: Engines) {
        engines.git_engine.diff_summary(root);
    }
}
