//! jq-like filtering for `--json` output.
//!
//! [`apply_json_filter`] evaluates a jq expression against a [`serde_json::Value`]
//! using the [jaq](https://github.com/01mf02/jaq) engine. When a filter yields
//! multiple values, they are wrapped in a JSON array so stdout still carries a
//! single JSON value (matching the `--json` contract).

use anyhow::{bail, Context, Result};
use jaq_core::load::{Arena, File, Loader};
use jaq_core::{data, unwrap_valr, Compiler, Ctx, Vars};
use jaq_json::{read, Val};
use serde_json::Value;

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
    let filter = filter.trim();
    if filter.is_empty() {
        bail!("--filter expression must not be empty");
    }

    let input_bytes = serde_json::to_vec(input).context("serializing JSON for filter input")?;
    let input_val = read::parse_single(&input_bytes).map_err(|e| anyhow::anyhow!("{e}"))?;

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
        .map_err(|errs| anyhow::anyhow!("invalid filter {filter:?}: {errs:?}"))?;
    let compiled = Compiler::default()
        .with_funs(funs)
        .compile(modules)
        .map_err(|errs| anyhow::anyhow!("invalid filter {filter:?}: {errs:?}"))?;
    let ctx = Ctx::<data::JustLut<Val>>::new(&compiled.lut, Vars::new([]));
    let out = compiled.id.run((ctx, input_val)).map(unwrap_valr);

    let mut results = Vec::new();
    for item in out {
        let val = item.map_err(|e| anyhow::anyhow!("filter {filter:?}: {e}"))?;
        let json =
            serde_json::from_str(&val.to_string()).context("converting filter output to JSON")?;
        results.push(json);
    }

    match results.len() {
        0 => Ok(Value::Null),
        1 => Ok(results.into_iter().next().unwrap_or(Value::Null)),
        _ => Ok(Value::Array(results)),
    }
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
}
