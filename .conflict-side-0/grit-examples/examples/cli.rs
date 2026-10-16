// API docs: https://docs.rs/grit-lib/latest/grit_lib/repo/struct.Repository.html
use std::process::Command;

fn main() -> std::io::Result<()> {
    let status = Command::new("grit").arg("status").status()?;
    println!("grit status exited with {status}");
    Ok(())
}
