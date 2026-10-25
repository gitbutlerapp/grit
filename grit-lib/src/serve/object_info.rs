//! Protocol v2 `command=object-info` (remote object metadata without a pack).

use super::{parse_oid, write_flush, write_line, Result, ServeError};
use crate::repo::Repository;

/// Write the v2 `object-info` response for `args` to `output`.
pub(crate) fn write_object_info_response(
    repo: &Repository,
    output: &mut dyn std::io::Write,
    args: &[String],
) -> Result<()> {
    let mut want_size = false;
    let mut want_type = false;
    let mut oids = Vec::new();
    for arg in args {
        match arg.as_str() {
            "size" => want_size = true,
            "type" => want_type = true,
            "" => {}
            line if let Some(hex) = line.strip_prefix("oid ") => {
                oids.push(parse_oid(repo, hex.trim())?);
            }
            other => {
                return Err(ServeError::Protocol(format!(
                    "object-info: unexpected line: '{other}'"
                )));
            }
        }
    }
    if want_size {
        write_line(output, "size")?;
    }
    if want_type {
        write_line(output, "type")?;
    }
    for oid in oids {
        write_line(
            output,
            &format_object_info_line(repo, &oid, want_size, want_type),
        )?;
    }
    write_flush(output)
}

fn format_object_info_line(
    repo: &Repository,
    oid: &crate::objects::ObjectId,
    want_size: bool,
    want_type: bool,
) -> String {
    let hex = oid.to_hex();
    let info = match repo.odb().read_info(oid) {
        Ok(info) => info,
        Err(_) => return format!("{hex} "),
    };
    let mut line = hex;
    if want_size {
        line.push(' ');
        line.push_str(&info.size.to_string());
    }
    if want_type {
        line.push(' ');
        line.push_str(info.kind.as_str());
    }
    line
}
