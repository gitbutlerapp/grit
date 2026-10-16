/**
 * Markdown → HTML for docs and blog pages.
 *
 * Pages are parsed once into an mdast tree with heading anchors assigned in
 * document order, so layouts can split a page into columns (prose here, code
 * there) without changing any anchor. Code blocks are highlighted by language;
 * a fence like ```` ```rust include=grit-examples/src/bin/guide_refs.rs ````
 * marks an included example file.
 */
import type { Heading, Link, Paragraph, Root, RootContent, Text } from "mdast";
import { fromMarkdown } from "mdast-util-from-markdown";
import { gfmFromMarkdown } from "mdast-util-gfm";
import { toHast } from "mdast-util-to-hast";
import { toString } from "mdast-util-to-string";
import { toHtml } from "hast-util-to-html";
import { gfm } from "micromark-extension-gfm";
import { highlightHtml } from "./highlight";
import { AnchorSet } from "./text";

export interface TocItem {
  level: number;
  text: string;
  anchor: string;
}

export interface DocTree {
  root: Root;
  toc: TocItem[];
}

/** Parse Markdown and give every heading a unique anchor (`id`). */
export function parseDoc(markdown: string): DocTree {
  const root = fromMarkdown(markdown, { extensions: [gfm()], mdastExtensions: [gfmFromMarkdown()] });
  const anchors = new AnchorSet();
  const toc: TocItem[] = [];
  const visit = (nodes: RootContent[]) => {
    for (const node of nodes) {
      if (node.type === "heading") {
        const text = toString(node);
        const anchor = anchors.next(text);
        node.data = { ...node.data, hProperties: { ...(node.data?.hProperties ?? {}), id: anchor } };
        if (node.depth >= 2) toc.push({ level: node.depth, text, anchor });
      } else if ("children" in node && node.type !== "paragraph") {
        visit(node.children as RootContent[]);
      }
    }
  };
  visit(root.children);
  return { root, toc };
}

/** Render mdast nodes to HTML. Call `initHighlighter()` first. */
export function renderHtml(nodes: RootContent[]): string {
  const root: Root = { type: "root", children: nodes };
  const hast = toHast(root, {
    allowDangerousHtml: true,
    handlers: {
      code(_state, node) {
        const lang = [node.lang, node.meta].filter(Boolean).join(" ");
        return { type: "raw", value: highlightHtml(lang, node.value) } as any;
      },
    },
  });
  // Wrap tables so wide ones scroll on their own.
  return toHtml(hast as any, { allowDangerousHtml: true })
    .replace(/<table>/g, '<div class="table"><table>')
    .replace(/<\/table>/g, "</table></div>");
}

/** Inline HTML of a paragraph's contents (no `<p>`). */
export function renderInline(paragraph: Paragraph): string {
  return renderHtml([paragraph]).replace(/^<p>|<\/p>$/g, "");
}

export interface Section {
  anchor: string;
  title: string;
  heading: Heading;
  nodes: RootContent[];
}

/** Split a page into the intro (before the first H2) and one section per H2. */
export function splitSections(root: Root): { intro: RootContent[]; sections: Section[] } {
  const intro: RootContent[] = [];
  const sections: Section[] = [];
  for (const node of root.children) {
    if (node.type === "heading" && node.depth === 2) {
      sections.push({
        anchor: String((node.data?.hProperties as any)?.id ?? ""),
        title: toString(node),
        heading: node,
        nodes: [],
      });
    } else if (sections.length) {
      sections[sections.length - 1].nodes.push(node);
    } else {
      intro.push(node);
    }
  }
  return { intro, sections };
}

export interface CodeBlock {
  /** Inline HTML caption (the lead-in paragraph, colon removed), or "". */
  caption: string;
  lang: string;
  meta: string;
  value: string;
}

/**
 * Remove code blocks from `nodes`. A block's caption is the paragraph right
 * before it when that paragraph ends with a colon ("Commit everything:") and
 * is at most `maxCaption` characters; other prose stays in place.
 */
export function pullCode(nodes: RootContent[], maxCaption = Infinity): { prose: RootContent[]; blocks: CodeBlock[] } {
  const prose: RootContent[] = [];
  const blocks: CodeBlock[] = [];
  for (const node of nodes) {
    if (node.type !== "code") {
      prose.push(node);
      continue;
    }
    let caption = "";
    const last = prose[prose.length - 1];
    if (last?.type === "paragraph") {
      const text = toString(last).trim();
      if (text.endsWith(":") && text.length <= maxCaption) {
        prose.pop();
        caption = renderInline(withoutTrailingColon(last));
      }
    }
    blocks.push({ caption, lang: node.lang ?? "text", meta: node.meta ?? "", value: node.value });
  }
  return { prose, blocks };
}

function withoutTrailingColon(paragraph: Paragraph): Paragraph {
  const children = [...paragraph.children];
  const tail = children[children.length - 1];
  if (tail?.type === "text") {
    children[children.length - 1] = { ...(tail as Text), value: (tail as Text).value.replace(/:\s*$/, "") };
  }
  return { ...paragraph, children };
}

/** Every link in `nodes`, in order. */
export function collectLinks(nodes: RootContent[]): { href: string; text: string }[] {
  const links: { href: string; text: string }[] = [];
  const visit = (list: RootContent[]) => {
    for (const node of list) {
      if (node.type === "link") links.push({ href: (node as Link).url, text: toString(node) });
      if ("children" in node) visit(node.children as RootContent[]);
    }
  };
  visit(nodes);
  return links;
}

/** The include path from a fence's meta (`include=<path>`), if any. */
export function includePath(meta: string): string | null {
  return /(?:^|\s)include=(\S+)/.exec(meta)?.[1] ?? null;
}
