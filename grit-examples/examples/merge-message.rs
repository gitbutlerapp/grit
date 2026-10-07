// API docs: https://docs.rs/grit-lib/latest/grit_lib/fmt_merge_msg/fn.fmt_merge_msg.html
use grit_lib::fmt_merge_msg::{fmt_merge_msg, FmtMergeMsgOptions};

fn main() {
    let input = "0123456789012345678901234567890123456789\t\tbranch 'topic' of example\n";
    let message = fmt_merge_msg(input, &FmtMergeMsgOptions::default());

    println!("{message}");
}
