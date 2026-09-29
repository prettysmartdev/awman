extern crate serde_yaml_ng;
use serde_yaml_ng::Value;
fn main() {
    for file in [".github/workflows/release.yml", ".github/workflows/test.yml"] {
        let text = std::fs::read_to_string(file).unwrap();
        let doc: Value = serde_yaml_ng::from_str(&text).unwrap();
        let jobs = doc["jobs"].as_mapping().unwrap();
        for (id, job) in jobs {
            if let Some(needs) = job.get("needs") {
                let dependencies: Vec<&str> = if let Some(s) = needs.as_str() {vec![s]} else {needs.as_sequence().unwrap().iter().map(|n| n.as_str().unwrap()).collect()};
                for dependency in dependencies {assert!(jobs.contains_key(Value::String(dependency.into())), "{file}: {id:?} needs unknown job {dependency}");}
            }
            for step in job["steps"].as_sequence().unwrap() {
                if let Some(script) = step.get("run").and_then(Value::as_str) {
                    for forbidden in ["codesign", "notarytool", "stapler", "MACOS_SIGNING", "NOTARY_", "pkgbuild", "spctl"] {assert!(!script.contains(forbidden), "{file}: forbidden signing operation {forbidden}");}
                    let mut child = std::process::Command::new("bash").arg("-n").stdin(std::process::Stdio::piped()).spawn().unwrap();
                    use std::io::Write;
                    child.stdin.take().unwrap().write_all(script.as_bytes()).unwrap();
                    assert!(child.wait().unwrap().success(), "{file}: invalid run block");
                }
            }
        }
        println!("PASS: {file}: YAML, job dependency references, shell syntax and no explicit signing operations");
    }
}
