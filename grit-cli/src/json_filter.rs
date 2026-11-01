//! jq-like filtering for `--json` output.
//!
//! [`write_json_filter`] evaluates a jq expression against serialized JSON bytes
//! using the [jaq](https://github.com/01mf02/jaq) engine and writes the result
//! directly to a writer. When a filter yields multiple values, they are wrapped
//! in a JSON array so stdout still carries a single JSON value (matching the
//! `--json` contract).

use anyhow::{bail, Context, Result};
use jaq_core::load::{Arena, File, Loader};
use jaq_core::{data, unwrap_valr, Compiler, Ctx, Vars};
use jaq_json::{read, write, Num, Val};
use serde_json::Value;
use std::io::Write;

/// Apply a jq-like `filter` expression to `input`.
///
/// # Parameters
///
/// * `input` — the JSON value to filter (typically a command outcome object).
/// * `filter` — a jq expression, e.g. `.branch`, `.commits[].oid`, or `{branch, clean}`.
///
/// # Returns
///
/// The filtered JSON value. A filter that emits zero values yields `null`; one
/// value is returned as-is; multiple values are returned as a JSON array.
///
/// # Errors
///
/// Returns an error when the expression fails to parse, compile, or execute.
pub fn apply_json_filter(input: &Value, filter: &str) -> Result<Value> {
    let input_bytes = serde_json::to_vec(input).context("serializing JSON for filter input")?;
    let mut buf = Vec::new();
    write_json_filter(&mut buf, input_bytes, filter)?;
    serde_json::from_slice(&buf).context("parsing filtered JSON output")
}

/// Evaluate `filter` on `input_json` and write compact JSON to `w` (no trailing newline).
///
/// A filter that emits zero values writes `null`; one value is written as-is;
/// multiple values are written as a JSON array.
pub fn write_json_filter(w: &mut dyn Write, input_json: Vec<u8>, filter: &str) -> Result<()> {
    let filter = filter.trim();
    if filter.is_empty() {
        bail!("--filter expression must not be empty");
    }

    let input_val = read::parse_single(&input_json).map_err(|e| anyhow::anyhow!("{e}"))?;
    drop(input_json);

    let compiled = compile_filter(filter)?;
    let ctx = Ctx::<data::JustLut<Val>>::new(&compiled.lut, Vars::new([]));
    let out = compiled.id.run((ctx, input_val)).map(unwrap_valr);

    let mut vals = Vec::new();
    for item in out {
        vals.push(item.map_err(|e| anyhow::anyhow!("invalid filter `{filter}`: {e}"))?);
    }
    for val in &vals {
        ensure_strict_json(val, filter)?;
    }

    let pp = write::Pp::default();
    match vals.len() {
        0 => w.write_all(b"null").context("writing filter output")?,
        1 => write::write(w, &pp, 0, &vals[0])
            .map_err(|e| anyhow::anyhow!("writing filter output: {e}"))?,
        _ => {
            w.write_all(b"[").context("writing filter output")?;
            for (i, val) in vals.iter().enumerate() {
                if i > 0 {
                    w.write_all(b",").context("writing filter output")?;
                }
                write::write(w, &pp, 0, val)
                    .map_err(|e| anyhow::anyhow!("writing filter output: {e}"))?;
            }
            w.write_all(b"]").context("writing filter output")?;
        }
    }
    Ok(())
}

fn compile_filter(filter: &str) -> Result<jaq_core::Filter<data::JustLut<Val>>> {
    let program = File {
        code: filter,
        path: (),
    };
    let defs = jaq_core::defs()
        .chain(jaq_std::defs())
        .chain(jaq_json::defs());
    let funs = jaq_core::funs()
        .chain(jaq_std::funs())
        .chain(jaq_json::funs());
    let loader = Loader::new(defs);
    let arena = Arena::default();
    let modules = loader
        .load(&arena, program)
        .map_err(|_| anyhow::anyhow!("invalid filter `{filter}`: filter syntax error"))?;
    let compiled = Compiler::default()
        .with_funs(funs)
        .compile(modules)
        .map_err(|errs| {
            let detail = errs
                .into_iter()
                .flat_map(|(_, undefined)| {
                    undefined
                        .into_iter()
                        .map(|(name, _)| name.to_string())
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>()
                .join(", ");
            let detail = if detail.is_empty() {
                "syntax error".to_owned()
            } else {
                detail
            };
            anyhow::anyhow!("invalid filter `{filter}`: filter syntax error ({detail})")
        })?;
    Ok(compiled)
}

/// Reject jaq values that [`jaq_json::write`] can emit but strict JSON cannot represent.
fn ensure_strict_json(val: &Val, filter: &str) -> Result<()> {
    match val {
        Val::Null | Val::Bool(_) => Ok(()),
        Val::Num(n) => {
            if matches!(n, Num::Float(f) if !f.is_finite()) {
                return strict_json_error(filter);
            }
            Ok(())
        }
        Val::BStr(_) => strict_json_error(filter),
        Val::TStr(_) => Ok(()),
        Val::Arr(a) => {
            for item in a.iter() {
                ensure_strict_json(item, filter)?;
            }
            Ok(())
        }
        Val::Obj(o) => {
            for (key, value) in o.iter() {
                if !matches!(key, Val::TStr(_)) {
                    return strict_json_error(filter);
                }
                ensure_strict_json(key, filter)?;
                ensure_strict_json(value, filter)?;
            }
            Ok(())
        }
    }
}

fn strict_json_error(filter: &str) -> Result<()> {
    bail!("invalid filter `{filter}`: filter output is not valid JSON")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::apply_json_filter;

    #[test]
    fn selects_scalar_field() {
        let input = json!({"branch": "main", "clean": true});
        let out = apply_json_filter(&input, ".branch").unwrap();
        assert_eq!(out, json!("main"));
    }

    #[test]
    fn selects_object_projection() {
        let input = json!({"branch": "main", "clean": true, "head": null});
        let out = apply_json_filter(&input, "{branch, clean}").unwrap();
        assert_eq!(out, json!({"branch": "main", "clean": true}));
    }

    #[test]
    fn bad_filter_reports_syntax_error_not_debug() {
        let input = json!({"commits": []});
        let err = apply_json_filter(&input, ".commits[").unwrap_err();
        let msg = format!("{err:#}");
        assert!(!msg.contains("File {"), "must not leak jaq Debug: {msg}");
        assert!(msg.contains("invalid filter"), "{msg}");
        assert!(msg.contains("syntax error"), "{msg}");
    }

    #[test]
    fn maps_array_elements() {
        let input = json!({
            "commits": [
                {"oid": "aaa", "subject": "one"},
                {"oid": "bbb", "subject": "two"},
            ]
        });
        let out = apply_json_filter(&input, ".commits[].oid").unwrap();
        assert_eq!(out, json!(["aaa", "bbb"]));
    }

    #[test]
    fn filtered_object_preserves_serialization_key_order() {
        #[derive(serde::Serialize)]
        struct Order {
            zebra: u32,
            alpha: u32,
        }
        let input_bytes = serde_json::to_vec(&Order { zebra: 1, alpha: 2 }).unwrap();
        let mut buf = Vec::new();
        super::write_json_filter(&mut buf, input_bytes, ".").unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(
            s.find("\"zebra\"").unwrap() < s.find("\"alpha\"").unwrap(),
            "expected struct field order, got {s}"
        );
    }

    #[test]
    fn rejects_non_json_number_literals() {
        let input = json!({"branch": "main"});
        for expr in ["nan", "infinite"] {
            let err = apply_json_filter(&input, expr).unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains("not valid JSON"), "{expr}: {msg}");
        }
    }

    #[test]
    fn rejects_non_string_object_keys() {
        let input = json!({});
        let err = apply_json_filter(&input, "{(1): 2}").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("not valid JSON"), "{msg}");
    }
}
