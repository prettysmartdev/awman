use crate::support;
use std::time::Instant;
#[test]
fn size_startup_and_rss_are_recorded_without_thresholds() {
    let mut s = support::sandbox("measure::size_startup_and_rss_are_recorded_without_thresholds");
    let a = support::artifact(&mut s);
    let mut starts = Vec::new();
    for _ in 0..5 {
        let start = Instant::now();
        support::run_artifact(&s, &a)
            .arg("--version")
            .run()
            .assert_success();
        starts.push(start.elapsed().as_micros() as u64);
    }
    let mut rss: Option<u64> = None;
    #[cfg(target_os = "linux")]
    {
        let mut child = support::run_artifact(&s, &a)
            .arg("__awman-builtin-kernel-check")
            .spawn();
        while child.try_exit().is_none() {
            if let Ok(status) = std::fs::read_to_string(format!("/proc/{}/status", child.pid())) {
                for line in status.lines().filter(|l| l.starts_with("VmHWM:")) {
                    if let Some(kib) = line
                        .split_whitespace()
                        .nth(1)
                        .and_then(|v| v.parse::<u64>().ok())
                    {
                        rss = Some(rss.unwrap_or(0).max(kib * 1024));
                    }
                }
            }
            std::thread::yield_now(); // sampling, not readiness synchronization
        }
        assert!(child.wait(std::time::Duration::from_secs(5)).success());
    }
    let warning = if rss.is_none() {
        Some("RSS unavailable on this runner; non-blocking measurement needs native sampler")
    } else {
        None
    };
    let record = serde_json::json!({"schema":1,"artifact_sha256":a.sha256,"target_os":std::env::consts::OS,"target_arch":std::env::consts::ARCH,
        "size_bytes":std::fs::metadata(&a.exe).unwrap().len(),"startup_us":starts,"startup_method":"supervised offline --version wall time, includes wrapper",
        "rss_bytes":rss,"rss_method":"Linux /proc VmHWM during exact kernel-check","blocking":false,"warning":warning});
    support::record(&format!("measurements-{}.json", a.sha256), &record);
}
