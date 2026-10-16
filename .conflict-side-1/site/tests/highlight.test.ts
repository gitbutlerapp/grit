import { beforeAll, describe, expect, it } from "vitest";
import { highlightHtml, initHighlighter, tokenize } from "../lib/highlight";

beforeAll(() => initHighlighter());

describe("highlighting", () => {
  it("marks console commands, flags, strings and output", () => {
    const html = highlightHtml("console", "$ grit log --json | head -3\nabc123 output");
    expect(html).toContain('<span class="p">$</span>');
    expect(html).toContain('<span class="f">grit</span>');
    expect(html).toContain('<span class="a">--json</span>');
    expect(html).toContain('<span class="f">head</span>');
    expect(html).toContain('<span class="out">abc123 output</span>');
  });

  it("keeps multi-line quoted arguments part of the command", () => {
    const lines = tokenize("console", '$ grit commit -m "a\n\nb"');
    expect(lines.map((l) => l.cls)).toEqual(["cmd", "cmd", "cmd"]);
  });

  it("prompts every line of a shell script but not comments", () => {
    const lines = tokenize("bash", "cargo run \\\n  --x\n# note");
    expect(lines[0].tokens[0]).toEqual({ text: "$", cls: "p" });
    expect(lines[1].tokens.some((t) => t.cls === "p")).toBe(false);
    expect(lines[2].tokens[0].cls).toBe("c");
  });

  it("highlights Rust and hides doctest scaffolding", () => {
    const html = highlightHtml("rust", 'fn main() { let s = "x"; println!("{s}"); }\n# hidden');
    expect(html).toContain('<span class="k">fn</span>');
    expect(html).toContain('<span class="s">');
    expect(html).not.toContain("hidden");
  });

  it("marks JSON keys and numbers", () => {
    const html = highlightHtml("json", '{"a": 1}');
    expect(html).toMatch(/<span class="k">"?a/);
    expect(html).toContain('<span class="n">1</span>');
  });
});
