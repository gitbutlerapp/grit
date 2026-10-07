// API docs: https://docs.rs/grit-lib/latest/grit_lib/protocol/index.html
use grit_lib::protocol::{check_protocol_allowed_with, ProtocolPolicyInputs};

fn main() -> Result<(), grit_lib::protocol::ProtocolError> {
    check_protocol_allowed_with(
        "https",
        &ProtocolPolicyInputs {
            git_allow_protocol: None,
            git_protocol_from_user: Some("1".into()),
            specific_allow: None,
            blanket_allow: None,
        },
    )?;

    println!("https transport is allowed for a user-requested push");
    Ok(())
}
