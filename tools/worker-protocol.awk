# Prints the WORKER_PROTOCOL defined by the src/data/builtin_runtime.rs text on
# stdin: a literal `WORKER_PROTOCOL: &str = "..."`, or the
# `define_builtin_runtime_version!("X")` + `concat!($version, "Y")` pair.
# Prints nothing when neither form is present.
/define_builtin_runtime_version!\("/ { v = $0; sub(/.*define_builtin_runtime_version!\("/, "", v); sub(/".*/, "", v) }
/concat!\(\$version, "/ { s = $0; sub(/.*concat!\(\$version, "/, "", s); sub(/".*/, "", s) }
/WORKER_PROTOCOL: &str = "/ { l = $0; sub(/.*WORKER_PROTOCOL: &str = "/, "", l); sub(/".*/, "", l) }
END { if (l != "") print l; else if (v != "" && s != "") print v s }
