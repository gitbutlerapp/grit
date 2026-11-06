//! jq-like filtering for `--json` output.
//!
//! [`write_json_filter`] evaluates a jq expression against serialized JSON bytes
//! using the [jaq](https://github.com/01mf02/jaq) engine and writes the result
//! directly to a writer. Iterator-style filters (path `[]`, comma, `select`, and
//! similar) always produce a JSON array so scripts see a stable type; singular
//! filters still emit one value as-is (including `null` for a missing field).

use anyhow::{bail, Context, Result};
use jaq_core::load::parse::{BinaryOp, Term};
use jaq_core::load::{parse, Arena, File, Loader};
use jaq_core::path::{Opt, Part};
use jaq_core::{data, unwrap_valr, Compiler, Ctx, Vars};
use jaq_json::{read, write, Num, Val};
use serde_json::Value;
use std::io::Write;

use crate::json_to_jaq;

/// Apply a jq-like `filter` expression to `input`.
///
/// # Parameters
///
/// * `input` — the JSON value to filter (typically a command outcome object).
/// * `filter` — a jq expression, e.g. `.branch`, `.commits[].oid`, or `{branch, clean}`.
///
/// # Returns
///
/// The filtered JSON value. Iterator-style filters always return a JSON array
/// (`[]`, `[x]`, or `[x, …]`). Other filters emit zero values as `null`, one
/// value as-is, and two or more values as a JSON array.
///
/// # Errors
///
/// Returns an error when the expression fails to parse, compile, or execute.
pub fn apply_json_filter(input: &Value, filter: &str) -> Result<Value> {
    let input_val = json_to_jaq::serialize_value(input).context("building filter input")?;
    let mut buf = Vec::new();
    write_json_filter_val(&mut buf, input_val, filter)?;
    serde_json::from_slice(&buf).context("parsing filtered JSON output")
}

/// Evaluate `filter` on serialized JSON bytes (tests and byte-oriented callers).
#[cfg_attr(not(test), allow(dead_code))]
pub fn write_json_filter(w: &mut dyn Write, input_json: Vec<u8>, filter: &str) -> Result<()> {
    let input_val = read::parse_single(&input_json).map_err(|e| anyhow::anyhow!("{e}"))?;
    drop(input_json);
    write_json_filter_val(w, input_val, filter)
}

/// Evaluate `filter` on an already-built jaq value and write compact JSON to `w`.
///
/// Iterator-style filters always write a JSON array. Singular filters write
/// `null` for zero values, one value as-is, and wrap two or more values in an array.
pub fn write_json_filter_val(w: &mut dyn Write, input_val: Val, filter: &str) -> Result<()> {
    let filter = filter.trim();
    if filter.is_empty() {
        bail!("--filter expression must not be empty");
    }

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
    let stream = filter_yields_stream(filter).unwrap_or(false);
    if stream || vals.len() > 1 {
        w.write_all(b"[").context("writing filter output")?;
        for (i, val) in vals.iter().enumerate() {
            if i > 0 {
                w.write_all(b",").context("writing filter output")?;
            }
            write::write(w, &pp, 0, val)
                .map_err(|e| anyhow::anyhow!("writing filter output: {e}"))?;
        }
        w.write_all(b"]").context("writing filter output")?;
    } else {
        match vals.len() {
            0 => w.write_all(b"null").context("writing filter output")?,
            1 => write::write(w, &pp, 0, &vals[0])
                .map_err(|e| anyhow::anyhow!("writing filter output: {e}"))?,
            _ => unreachable!(),
        }
    }
    Ok(())
}

/// Whether a successfully parsed filter can emit a variable number of values.
fn filter_yields_stream(filter: &str) -> Option<bool> {
    let term = parse(filter, |p| p.term())?;
    Some(term_yields_stream(&term))
}

fn term_yields_stream(term: &Term<&str>) -> bool {
    match term {
        Term::Recurse => true,
        Term::Path(base, path) => {
            term_yields_stream(base)
                || path.0.iter().any(|(part, opt)| {
                    matches!(opt, Opt::Optional)
                        || match part {
                            Part::Range(start, end) => {
                                start.as_ref().is_some_and(term_yields_stream)
                                    || end.as_ref().is_some_and(term_yields_stream)
                                    || matches!((start.as_ref(), end.as_ref()), (None, None))
                            }
                            Part::Index(idx) => term_yields_stream(idx),
                        }
                })
        }
        Term::Neg(inner) | Term::Label(_, inner) => term_yields_stream(inner),
        Term::BinOp(left, op, right) => match op {
            BinaryOp::Comma => true,
            _ => term_yields_stream(left) || term_yields_stream(right),
        },
        Term::Fold(kind, _, _, _) => *kind == "foreach",
        Term::TryCatch(try_, catch) => {
            catch.is_none()
                || term_yields_stream(try_)
                || catch.as_ref().is_some_and(|c| term_yields_stream(c))
        }
        Term::IfThenElse(branches, else_) => {
            branches
                .iter()
                .any(|(cond, then_)| term_yields_stream(cond) || term_yields_stream(then_))
                || else_.as_ref().is_some_and(|e| term_yields_stream(e))
        }
        Term::Def(_, body) => term_yields_stream(body),
        Term::Call(name, args) => call_yields_stream(name, args),
        Term::Obj(pairs) => pairs.iter().any(|(key, val)| {
            term_yields_stream(key) || val.as_ref().is_some_and(|v| term_yields_stream(v))
        }),
        Term::Str(_, parts) => parts.iter().any(
            |part| matches!(part, jaq_core::load::lex::StrPart::Term(t) if term_yields_stream(t)),
        ),
        Term::Break(_) | Term::Id | Term::Num(_) | Term::Var(_) | Term::Arr(_) => false,
    }
}

fn call_yields_stream(name: &str, args: &[Term<&str>]) -> bool {
    const SINGULAR: &[&str] = &[
        "map",
        "map_values",
        "keys",
        "length",
        "add",
        "to_entries",
        "flatten",
        "unique",
        "sort",
        "group_by",
    ];
    if SINGULAR.contains(&name) {
        return false;
    }
    const INDEX_LIKE: &[&str] = &["first", "last", "nth"];
    if INDEX_LIKE.contains(&name) {
        return args.iter().any(term_yields_stream);
    }
    const GENERATOR: &[&str] = &[
        "select", "empty", "range", "recurse", "inputs", "limit", "while", "repeat", "paths",
        "foreach",
    ];
    if GENERATOR.contains(&name) {
        return true;
    }
    args.iter().any(term_yields_stream)
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
    fn iterator_filter_wraps_one_element() {
        let input = json!({
            "commits": [{"oid": "aaa", "subject": "one"}]
        });
        let out = apply_json_filter(&input, ".commits[].oid").unwrap();
        assert_eq!(out, json!(["aaa"]));
    }

    #[test]
    fn iterator_filter_empty_match_is_array() {
        let input = json!({
            "commits": [{"oid": "aaa", "subject": "one"}]
        });
        let out = apply_json_filter(&input, ".commits[] | select(.subject == \"nope\")").unwrap();
        assert_eq!(out, json!([]));
    }

    #[test]
    fn map_select_empty_is_singular_array_not_null() {
        let input = json!({
            "commits": [{"oid": "aaa", "subject": "one"}]
        });
        let out =
            apply_json_filter(&input, ".commits | map(select(.subject == \"nope\"))").unwrap();
        assert_eq!(out, json!([]));
    }

    #[test]
    fn singular_field_stays_scalar() {
        let input = json!({"branch": "main", "upstream": null});
        assert_eq!(apply_json_filter(&input, ".branch").unwrap(), json!("main"));
        assert_eq!(apply_json_filter(&input, ".upstream").unwrap(), json!(null));
    }

    #[test]
    fn index_accessor_stays_scalar() {
        let input = json!({
            "commits": [{"oid": "aaa", "refs": [{"name": "main"}]}]
        });
        assert_eq!(
            apply_json_filter(&input, ".commits[0].oid").unwrap(),
            json!("aaa")
        );
    }

    #[test]
    fn nested_iterator_filter_wraps_one_element() {
        let input = json!({
            "commits": [{"oid": "aaa", "refs": [{"name": "main"}]}]
        });
        let out = apply_json_filter(&input, ".commits[0].refs[].name").unwrap();
        assert_eq!(out, json!(["main"]));
    }

    #[test]
    fn staged_paths_iterator_empty_is_array() {
        let input = json!({"staged": []});
        let out = apply_json_filter(&input, ".staged[].path").unwrap();
        assert_eq!(out, json!([]));
    }

    #[test]
    fn reduce_over_iterator_input_stays_singular() {
        let input = json!({"staged": [{"path": "a"}]});
        let out = apply_json_filter(&input, "reduce .staged[].path as $x (\"\"; . + $x)").unwrap();
        assert_eq!(out, json!("a"));
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

    /// Guards the #2409 hot path: `--filter` must not use JSON-bytes re-parse on
    /// large command outcomes (see `write_json` in `output.rs`).
    #[test]
    fn large_diff_shaped_direct_serialize_for_filter() {
        const N: usize = 8000;
        let files: Vec<_> = (0..N)
            .map(|i| {
                json!({
                    "path": format!("src/file{i}.rs"),
                    "kind": "modified",
                    "hunks": [{"header": "@@", "lines": [{"kind": "context", "text": "x"}]}],
                })
            })
            .collect();
        let input = json!({"files": files});

        assert_eq!(
            apply_json_filter(&input, ".files|length").expect("filter large diff"),
            json!(N),
            "typed serialize path must filter large diff-shaped outcomes"
        );

        let direct = crate::json_to_jaq::serialize_value(&input).expect("direct jaq");
        let bytes = serde_json::to_vec(&input).expect("serialize");
        let parsed = jaq_json::read::parse_single(&bytes).expect("parse");
        let mut direct_buf = Vec::new();
        let mut parsed_buf = Vec::new();
        super::write_json_filter_val(&mut direct_buf, direct, ".files|length").expect("direct");
        super::write_json_filter_val(&mut parsed_buf, parsed, ".files|length").expect("parsed");
        assert_eq!(
            direct_buf, parsed_buf,
            "direct serialize and JSON re-parse must agree on large payloads"
        );

        let src = include_str!("output.rs");
        assert!(
            src.contains("json_to_jaq::serialize_value(value)"),
            "write_json filter path must build jaq input via typed serialize (#2409)"
        );
        let filter_branch = src
            .split("if let Some(expr) = filter")
            .nth(1)
            .and_then(|s| s.split("} else {").next())
            .expect("write_json filter branch");
        assert!(
            !filter_branch.contains("parse_single"),
            "write_json must not re-parse JSON bytes for --filter (#2409)"
        );
    }
}
