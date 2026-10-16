use std::process::Command;

#[test]
fn hash_info_json_reports_backends() {
    let exe = env!("CARGO_BIN_EXE_hash-info");
    let output = Command::new(exe)
        .arg("--json")
        .output()
        .expect("run hash-info");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).expect("utf8");
    let value: serde_json::Value = serde_json::from_str(text.trim()).expect("json");
    assert!(value.get("sha1").and_then(|v| v.as_str()).is_some());
    assert!(value.get("sha256").and_then(|v| v.as_str()).is_some());
}
