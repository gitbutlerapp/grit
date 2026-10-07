// API docs: https://docs.rs/grit-lib/latest/grit_lib/reflog/index.html
use grit_lib::reflog::{read_reflog, reflog_path};
use std::path::Path;

fn main() -> grit_lib::error::Result<()> {
    let git_dir = Path::new(".git");
    println!(
        "HEAD reflog path: {}",
        reflog_path(git_dir, "HEAD").display()
    );

    for entry in read_reflog(git_dir, "HEAD")?.iter().take(5) {
        println!("{} -> {} {}", entry.old_oid, entry.new_oid, entry.message);
    }
    Ok(())
}
