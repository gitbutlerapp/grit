//! Item-at-a-time renderers and shared JSON array streaming.

use anyhow::Result;
use std::io::Write;

use super::RenderContext;
use crate::output::json_write;
use crate::stdio;

/// Render a command outcome one item at a time (human or Markdown bodies, or JSON arrays).
#[allow(dead_code)] // adopted by streaming commands in a later CLI layer step
pub trait StreamRender {
    /// One element in the streamed listing.
    type Item;
    /// Values emitted once after the last item (counts, truncation flags, …).
    type Summary: serde::Serialize;

    /// Optional header before the first item (default: no-op).
    fn begin(&mut self, _w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        Ok(())
    }

    /// Render one item.
    fn item(&mut self, w: &mut dyn Write, ctx: &RenderContext, value: &Self::Item) -> Result<()>;

    /// Emit trailing summary fields after the item stream.
    fn finish(
        &mut self,
        w: &mut dyn Write,
        _ctx: &RenderContext,
        summary: &Self::Summary,
    ) -> Result<()> {
        let _ = summary;
        let _ = w;
        Ok(())
    }
}

/// Write a JSON array prefix for outcomes shaped like `{"commits":[...], "truncated": bool, "next": null}`.
pub fn begin_json_array_field(w: &mut dyn Write, field: &str) -> Result<()> {
    stdio::io_result(write!(w, "{{\"{field}\":["))
        .map_err(|e| anyhow::anyhow!("writing output: {e}"))
}

/// Finish a JSON array field; `extra` is inserted after `truncated` (for example `,"groups":null`).
pub fn finish_json_array_document(
    w: &mut dyn Write,
    _field: &str,
    truncated: bool,
    extra_fields: &str,
) -> Result<()> {
    stdio::io_result(write!(w, "],\"truncated\":{}", truncated))
        .map_err(|e| anyhow::anyhow!("writing output: {e}"))?;
    stdio::io_result(writeln!(
        w,
        "{extra_fields},\"next\":null}}",
        extra_fields = extra_fields
    ))
    .map_err(|e| anyhow::anyhow!("writing output: {e}"))
}

/// Serialize one value into a JSON array, with a leading comma when `first` is false.
/// Serialize one value into a JSON array, with a leading comma when `first` is false.
pub fn write_json_array_element<T: serde::Serialize>(
    w: &mut dyn Write,
    first: &mut bool,
    value: &T,
) -> Result<()> {
    let mut buf = Vec::with_capacity(1024);
    if !*first {
        buf.push(b',');
    } else {
        *first = false;
    }
    json_write(serde_json::to_writer(&mut buf, value))?;
    stdio::io_result(w.write_all(&buf)).map_err(|e| anyhow::anyhow!("writing output: {e}"))
}
