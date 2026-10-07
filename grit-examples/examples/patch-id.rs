// API docs: https://docs.rs/grit-lib/latest/grit_lib/patch_ids/fn.compute_patch_ids_from_text.html
use grit_lib::patch_ids::{compute_patch_ids_from_text, PatchIdMode};

fn main() {
    let patch = b"diff --git a/file b/file\n--- a/file\n+++ b/file\n@@ -1 +1 @@\n-old\n+new\n";
    let ids = compute_patch_ids_from_text(patch, PatchIdMode::Stable);

    println!("computed {} patch-id record(s)", ids.len());
}
