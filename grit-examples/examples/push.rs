// API docs: https://docs.rs/grit-lib/latest/grit_lib/protocol/index.html
use grit_lib::remote::protocol::{check_protocol_allowed_with, ProtocolPolicyInputs};

fn main() -> Result<(), grit_lib::remote::protocol::ProtocolError> {
    check_protocol_allowed_with(
        "https",
        &ProtocolPolicyInputs::default().with_git_protocol_from_user(Some("1".into())),
    )?;

    println!("https transport is allowed for a user-requested push");
    Ok(())
}
