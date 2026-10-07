// API docs: https://docs.rs/grit-lib/latest/grit_lib/diff/enum.DiffStatus.html
use grit_lib::diff::DiffStatus;

fn main() {
    let statuses = [DiffStatus::Added, DiffStatus::Modified, DiffStatus::Deleted];

    for status in statuses {
        println!("{}", status.letter());
    }
}
