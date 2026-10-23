//! Structural JSON Schema backward-compatibility checks for `--json` contracts.
//!
//! Compares a committed (older) schema document to a newly generated one and
//! reports breaking changes: removed properties, narrowed types, new required
//! fields, removed enum variants, tightened `additionalProperties`, and removed
//! nullability.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

/// One breaking change between an older and newer schema document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BreakingChange {
    /// JSON pointer-style path (e.g. `/properties/status`).
    pub path: String,
    /// Short machine-readable category.
    pub kind: &'static str,
    /// Human-readable detail for test output and CI logs.
    pub detail: String,
}

/// Returns every breaking difference when `new_doc` replaces `old_doc`.
///
/// Additive changes (new optional properties, new enum variants, new definitions,
/// adding an `enum` to a former free-form string) produce an empty list.
pub fn breaking_changes(old_doc: &Value, new_doc: &Value) -> Vec<BreakingChange> {
    let old_root = SchemaNode::parse(old_doc);
    let new_root = SchemaNode::parse(new_doc);
    let mut out = Vec::new();
    compare_nodes("", &old_root, &new_root, &mut out);
    out
}

/// Formats breaking changes for a test failure or update-mode refusal.
pub fn format_breaking_changes(command: &str, version: u32, changes: &[BreakingChange]) -> String {
    let mut lines = vec![format!(
        "breaking JSON schema change for `{command}` v{version} (publish v{} and keep v{version} frozen):",
        version + 1
    )];
    for change in changes {
        lines.push(format!(
            "  {} at {}: {}",
            change.kind, change.path, change.detail
        ));
    }
    lines.join("\n")
}

#[derive(Debug, Clone)]
struct SchemaNode {
    raw: Value,
    types: TypeSet,
    properties: BTreeMap<String, SchemaNode>,
    required: BTreeSet<String>,
    additional_properties: AdditionalProperties,
    enum_values: Option<BTreeSet<String>>,
    items: Option<Box<SchemaNode>>,
    combinator: Combinator,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdditionalProperties {
    Unspecified,
    Allowed,
    Forbidden,
}

#[derive(Debug, Clone)]
enum Combinator {
    None,
    OneOf(Vec<SchemaNode>),
    AnyOf(Vec<SchemaNode>),
    AllOf(Vec<SchemaNode>),
}

#[derive(Debug, Clone, Default)]
struct TypeSet {
    null: bool,
    boolean: bool,
    integer: bool,
    number: bool,
    string: bool,
    array: bool,
    object: bool,
}

impl TypeSet {
    fn from_type_value(value: &Value) -> Self {
        let mut set = Self::default();
        match value {
            Value::String(name) => set.insert_name(name),
            Value::Array(names) => {
                for name in names {
                    if let Some(s) = name.as_str() {
                        set.insert_name(s);
                    }
                }
            }
            _ => {}
        }
        set
    }

    fn insert_name(&mut self, name: &str) {
        match name {
            "null" => self.null = true,
            "boolean" => self.boolean = true,
            "integer" => {
                self.integer = true;
                self.number = true;
            }
            "number" => self.number = true,
            "string" => self.string = true,
            "array" => self.array = true,
            "object" => self.object = true,
            _ => {}
        }
    }

    fn is_empty(&self) -> bool {
        !self.null
            && !self.boolean
            && !self.integer
            && !self.string
            && !self.array
            && !self.object
            && !self.number
    }

    fn narrows(&self, old: &Self) -> bool {
        if self.is_empty() {
            return false;
        }
        (old.null && !self.null)
            || (old.boolean && !self.boolean)
            || (old.integer && !self.integer)
            || (old.number && !self.number)
            || (old.string && !self.string)
            || (old.array && !self.array)
            || (old.object && !self.object)
    }
}

impl SchemaNode {
    fn parse(doc: &Value) -> Self {
        let root = doc.as_object().expect("schema document must be an object");
        let definitions = root.get("definitions").cloned().unwrap_or(Value::Null);
        Self::parse_inner(doc, &definitions, 0)
    }

    fn parse_inner(value: &Value, definitions: &Value, depth: u16) -> Self {
        if depth > 32 {
            return Self::opaque(value);
        }
        if let Some(reference) = value.get("$ref").and_then(Value::as_str) {
            if let Some(target) = resolve_ref(reference, definitions) {
                return Self::parse_inner(&target, definitions, depth + 1);
            }
            return Self::opaque(value);
        }

        let mut node = Self::opaque(value);
        if let Some(type_value) = value.get("type") {
            node.types = TypeSet::from_type_value(type_value);
        }
        if let Some(extra) = value.get("additionalProperties") {
            node.additional_properties = match extra {
                Value::Bool(false) => AdditionalProperties::Forbidden,
                Value::Bool(true) => AdditionalProperties::Allowed,
                _ => AdditionalProperties::Unspecified,
            };
        }
        if let Some(values) = value.get("enum").and_then(Value::as_array) {
            let set: BTreeSet<String> = values
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect();
            if !set.is_empty() {
                node.enum_values = Some(set);
            }
        }
        if let Some(props) = value.get("properties").and_then(Value::as_object) {
            for (name, prop) in props {
                node.properties.insert(
                    name.clone(),
                    Self::parse_inner(prop, definitions, depth + 1),
                );
            }
        }
        if let Some(required) = value.get("required").and_then(Value::as_array) {
            for name in required {
                if let Some(s) = name.as_str() {
                    node.required.insert(s.to_owned());
                }
            }
        }
        if let Some(items) = value.get("items") {
            node.items = Some(Box::new(Self::parse_inner(items, definitions, depth + 1)));
        }

        if let Some(one_of) = value.get("oneOf").and_then(Value::as_array) {
            node.combinator = Combinator::OneOf(
                one_of
                    .iter()
                    .map(|v| Self::parse_inner(v, definitions, depth + 1))
                    .collect(),
            );
        } else if let Some(any_of) = value.get("anyOf").and_then(Value::as_array) {
            node.combinator = Combinator::AnyOf(
                any_of
                    .iter()
                    .map(|v| Self::parse_inner(v, definitions, depth + 1))
                    .collect(),
            );
        } else if let Some(all_of) = value.get("allOf").and_then(Value::as_array) {
            node.combinator = Combinator::AllOf(
                all_of
                    .iter()
                    .map(|v| Self::parse_inner(v, definitions, depth + 1))
                    .collect(),
            );
        }

        node
    }

    fn opaque(raw: &Value) -> Self {
        Self {
            raw: raw.clone(),
            types: TypeSet::default(),
            properties: BTreeMap::new(),
            required: BTreeSet::new(),
            additional_properties: AdditionalProperties::Unspecified,
            enum_values: None,
            items: None,
            combinator: Combinator::None,
        }
    }
}

fn resolve_ref(reference: &str, definitions: &Value) -> Option<Value> {
    let suffix = reference.strip_prefix("#/definitions/")?;
    definitions.get(suffix).cloned()
}

fn compare_nodes(path: &str, old: &SchemaNode, new: &SchemaNode, out: &mut Vec<BreakingChange>) {
    compare_combinators(path, old, new, out);

    if new.types.narrows(&old.types) {
        out.push(BreakingChange {
            path: path.to_owned(),
            kind: "type_narrowed",
            detail: format!(
                "type narrowed from {} to {}",
                type_label(&old.types),
                type_label(&new.types)
            ),
        });
    }

    compare_enums(path, old, new, out);
    compare_additional_properties(path, old, new, out);
    compare_properties(path, old, new, out);
    compare_items(path, old, new, out);
}

fn compare_combinators(
    path: &str,
    old: &SchemaNode,
    new: &SchemaNode,
    out: &mut Vec<BreakingChange>,
) {
    use Combinator::{AllOf, AnyOf, None as NoneCombo, OneOf};
    match (&old.combinator, &new.combinator) {
        (NoneCombo, NoneCombo) => {}
        (OneOf(old_arms), OneOf(new_arms)) | (AnyOf(old_arms), AnyOf(new_arms)) => {
            for (index, old_arm) in old_arms.iter().enumerate() {
                let arm_path = format!("{path}/[arm {index}]");
                if let Some(new_arm) = new_arms.get(index) {
                    compare_nodes(&arm_path, old_arm, new_arm, out);
                }
            }
            if old_arms.len() > new_arms.len() {
                out.push(BreakingChange {
                    path: path.to_owned(),
                    kind: "combinator_narrowed",
                    detail: format!(
                        "combinator arm count shrank from {} to {}",
                        old_arms.len(),
                        new_arms.len()
                    ),
                });
            }
        }
        (AllOf(old_arms), AllOf(new_arms)) => {
            for (index, old_arm) in old_arms.iter().enumerate() {
                let arm_path = format!("{path}/[allOf {index}]");
                if let Some(new_arm) = new_arms.get(index) {
                    compare_nodes(&arm_path, old_arm, new_arm, out);
                }
            }
        }
        (NoneCombo, OneOf(new_arms)) | (NoneCombo, AnyOf(new_arms)) => {
            // Replacing a plain schema with a combinator: compare each new arm to old.
            for (index, new_arm) in new_arms.iter().enumerate() {
                let arm_path = format!("{path}/[arm {index}]");
                compare_nodes(&arm_path, old, new_arm, out);
            }
        }
        (OneOf(old_arms), NoneCombo) | (AnyOf(old_arms), NoneCombo) => {
            for (index, old_arm) in old_arms.iter().enumerate() {
                let arm_path = format!("{path}/[arm {index}]");
                compare_nodes(&arm_path, old_arm, new, out);
            }
        }
        _ => {
            compare_nodes(path, old, new, out);
        }
    }
}

fn compare_enums(path: &str, old: &SchemaNode, new: &SchemaNode, out: &mut Vec<BreakingChange>) {
    let (Some(old_enum), Some(new_enum)) = (&old.enum_values, &new.enum_values) else {
        if old.enum_values.is_none()
            && new.enum_values.is_some()
            && old.types.string
            && !old.types.object
        {
            // Former free string; enum restriction is allowed (e.g. structured `kind`).
            return;
        }
        return;
    };
    for value in old_enum {
        if !new_enum.contains(value) {
            out.push(BreakingChange {
                path: path.to_owned(),
                kind: "enum_variant_removed",
                detail: format!("enum variant `{value}` removed"),
            });
        }
    }
}

fn compare_additional_properties(
    path: &str,
    old: &SchemaNode,
    new: &SchemaNode,
    out: &mut Vec<BreakingChange>,
) {
    let old_forbids = matches!(old.additional_properties, AdditionalProperties::Forbidden);
    let new_forbids = matches!(new.additional_properties, AdditionalProperties::Forbidden);
    if !old_forbids && new_forbids {
        out.push(BreakingChange {
            path: path.to_owned(),
            kind: "additional_properties_tightened",
            detail: "additionalProperties became false".into(),
        });
    }
}

fn compare_properties(
    path: &str,
    old: &SchemaNode,
    new: &SchemaNode,
    out: &mut Vec<BreakingChange>,
) {
    for (name, old_prop) in &old.properties {
        let prop_path = if path.is_empty() {
            format!("/properties/{name}")
        } else {
            format!("{path}/properties/{name}")
        };
        let Some(new_prop) = new.properties.get(name) else {
            out.push(BreakingChange {
                path: prop_path,
                kind: "property_removed",
                detail: format!("property `{name}` removed"),
            });
            continue;
        };
        compare_nodes(&prop_path, old_prop, new_prop, out);
    }

    for name in &new.required {
        if old.required.contains(name) {
            continue;
        }
        let prop_path = if path.is_empty() {
            format!("/required/{name}")
        } else {
            format!("{path}/required/{name}")
        };
        out.push(BreakingChange {
            path: prop_path,
            kind: "property_newly_required",
            detail: format!("property `{name}` is newly required"),
        });
    }
}

fn compare_items(path: &str, old: &SchemaNode, new: &SchemaNode, out: &mut Vec<BreakingChange>) {
    match (&old.items, &new.items) {
        (Some(old_items), Some(new_items)) => {
            let items_path = if path.is_empty() {
                "/items".into()
            } else {
                format!("{path}/items")
            };
            compare_nodes(&items_path, old_items, new_items, out);
        }
        (Some(_), None) => out.push(BreakingChange {
            path: if path.is_empty() {
                "/items".into()
            } else {
                format!("{path}/items")
            },
            kind: "items_removed",
            detail: "array item schema removed".into(),
        }),
        _ => {}
    }
}

fn type_label(types: &TypeSet) -> String {
    let mut names = Vec::new();
    if types.null {
        names.push("null");
    }
    if types.boolean {
        names.push("boolean");
    }
    if types.integer {
        names.push("integer");
    } else if types.number {
        names.push("number");
    }
    if types.string {
        names.push("string");
    }
    if types.array {
        names.push("array");
    }
    if types.object {
        names.push("object");
    }
    if names.is_empty() {
        "(unspecified)".into()
    } else {
        names.join("|")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn assert_breaking(old: Value, new: Value) {
        assert!(
            !breaking_changes(&old, &new).is_empty(),
            "expected breaking changes"
        );
    }

    fn assert_compatible(old: Value, new: Value) {
        assert!(
            breaking_changes(&old, &new).is_empty(),
            "unexpected breaking changes: {:?}",
            breaking_changes(&old, &new)
        );
    }

    #[test]
    fn schema_compat_removed_property() {
        let old = json!({
            "type": "object",
            "properties": {
                "status": { "type": "string" },
                "branch": { "type": "string" }
            },
            "required": ["status"]
        });
        let new = json!({
            "type": "object",
            "properties": {
                "branch": { "type": "string" }
            },
            "required": ["branch"]
        });
        let changes = breaking_changes(&old, &new);
        assert!(changes
            .iter()
            .any(|c| c.kind == "property_removed" && c.detail.contains("status")));
    }

    #[test]
    fn schema_compat_type_change() {
        let old = json!({ "type": "string" });
        let new = json!({ "type": "integer" });
        let changes = breaking_changes(&old, &new);
        assert!(changes.iter().any(|c| c.kind == "type_narrowed"));
        assert_breaking(old, new);
    }

    #[test]
    fn schema_compat_new_required() {
        let old = json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" }
            }
        });
        let new = json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" }
            },
            "required": ["name"]
        });
        let changes = breaking_changes(&old, &new);
        assert!(changes.iter().any(|c| c.kind == "property_newly_required"));
    }

    #[test]
    fn schema_compat_enum_removed() {
        let old = json!({ "type": "string", "enum": ["a", "b", "c"] });
        let new = json!({ "type": "string", "enum": ["a", "b"] });
        let changes = breaking_changes(&old, &new);
        assert!(changes
            .iter()
            .any(|c| c.kind == "enum_variant_removed" && c.detail.contains("c")));
    }

    #[test]
    fn schema_compat_additive_ok() {
        let old = json!({
            "type": "object",
            "properties": {
                "a": { "type": "string" }
            },
            "required": ["a"]
        });
        let new = json!({
            "type": "object",
            "properties": {
                "a": { "type": "string" },
                "b": { "type": "integer" }
            },
            "required": ["a"]
        });
        assert_compatible(old, new);
    }

    #[test]
    fn schema_compat_enum_on_string_ok() {
        let old = json!({
            "type": "object",
            "properties": {
                "kind": { "type": "string" }
            }
        });
        let new = json!({
            "type": "object",
            "properties": {
                "kind": { "type": "string", "enum": ["conflict", "plain"] }
            }
        });
        assert_compatible(old, new);
    }

    #[test]
    fn schema_compat_nullable_removed() {
        let old = json!({ "type": ["string", "null"] });
        let new = json!({ "type": "string" });
        assert_breaking(old, new);
    }

    #[test]
    fn schema_compat_resolves_ref() {
        let old = json!({
            "definitions": {
                "Item": {
                    "type": "object",
                    "properties": { "id": { "type": "string" } },
                    "required": ["id"]
                }
            },
            "type": "object",
            "properties": {
                "item": { "$ref": "#/definitions/Item" }
            }
        });
        let new = json!({
            "definitions": {
                "Item": {
                    "type": "object",
                    "properties": { "name": { "type": "string" } },
                    "required": ["name"]
                }
            },
            "type": "object",
            "properties": {
                "item": { "$ref": "#/definitions/Item" }
            }
        });
        assert_breaking(old, new);
    }
}
