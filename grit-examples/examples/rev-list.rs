// API docs: https://docs.rs/grit-lib/latest/grit_lib/rev_list/enum.ObjectFilter.html
use grit_lib::rev_list::ObjectFilter;

fn main() -> Result<(), String> {
    let filter = ObjectFilter::parse("blob:limit=1m")?;

    println!("small blob included? {}", filter.includes_blob(512));
    println!(
        "large blob included? {}",
        filter.includes_blob(2 * 1024 * 1024)
    );
    Ok(())
}
