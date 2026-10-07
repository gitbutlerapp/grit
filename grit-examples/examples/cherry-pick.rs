// API docs: https://docs.rs/grit-lib/latest/grit_lib/patch_ids/index.html
use grit_lib::patch_ids::{compute_patch_ids_from_text, PatchIdMode};

fn main() {
    let patch = b"diff --git a/a b/a\nindex 0000000..1111111 100644\n--- a/a\n+++ b/a\n@@ -0,0 +1 @@\n+hello\n";
    let ids = compute_patch_ids_from_text(patch, PatchIdMode::Stable);

    for (commit_id, patch_id) in ids {
        println!("{commit_id} has stable patch-id {patch_id}");
    }
}
