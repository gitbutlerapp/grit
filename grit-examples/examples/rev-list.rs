// API docs: https://docs.rs/grit-lib/latest/grit_lib/rev_list/enum.ObjectFilter.html
use grit_lib::revwalk::rev_list::ObjectFilter;

fn main() -> Result<(), String> {
    let filter = ObjectFilter::parse("blob:limit=1m").map_err(|e| e.to_string())?;
    match filter {
        ObjectFilter::BlobLimit(bytes) => {
            println!("blob size limit: {bytes} bytes");
        }
        _ => println!("parsed filter: {filter:?}"),
    }
    Ok(())
}
