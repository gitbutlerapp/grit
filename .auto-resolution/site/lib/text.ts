/** Front matter, slugs and small Markdown helpers shared by docs and blog. */

const FRONT_MATTER = /^---\s*\n([\s\S]*?)\n---\s*\n/;

/** Split simple `key: value` front matter from a Markdown document. */
export function parseFrontMatter(text: string): { meta: Record<string, string>; body: string } {
  const match = FRONT_MATTER.exec(text);
  if (!match) return { meta: {}, body: text };
  const meta: Record<string, string> = {};
  for (const raw of match[1].split("\n")) {
    const line = raw.trim();
    if (!line || line.startsWith("#")) continue;
    const sep = line.indexOf(":");
    if (sep < 0) throw new Error(`invalid front matter line: ${JSON.stringify(raw)}`);
    meta[line.slice(0, sep).trim()] = line
      .slice(sep + 1)
      .trim()
      .replace(/^["']|["']$/g, "");
  }
  return { meta, body: text.slice(match[0].length) };
}

/** Heading anchor: ASCII-folded, lowercase, non-alphanumeric runs become `-`. */
export function slugify(value: string): string {
  const ascii = value.normalize("NFKD").replace(/[^\x00-\x7f]/g, "");
  const slug = ascii
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "");
  return slug || "section";
}

/** Assigns unique anchors within one page (`intro`, `intro-2`, …). */
export class AnchorSet {
  private seen = new Map<string, number>();

  next(text: string): string {
    const base = slugify(text);
    const count = this.seen.get(base) ?? 0;
    this.seen.set(base, count + 1);
    return count === 0 ? base : `${base}-${count + 1}`;
  }
}

const MARKDOWN_LINK = /\[([^\]]*)\]\(([^)]+)\)/g;

/**
 * Rewrite `[text](path)` links. `urlFor` gets the path without its fragment and
 * returns a replacement URL, or `null` to leave the link alone.
 */
export function rewriteMarkdownLinks(body: string, urlFor: (path: string) => string | null): string {
  return body.replace(MARKDOWN_LINK, (whole, text: string, rawUrl: string) => {
    const url = rawUrl.trim();
    const hash = url.indexOf("#");
    const target = hash >= 0 ? url.slice(0, hash) : url;
    const fragment = hash >= 0 && hash < url.length - 1 ? url.slice(hash) : "";
    if (!target) return whole;
    const replaced = urlFor(target);
    return replaced === null ? whole : `[${text}](${replaced}${fragment})`;
  });
}

/** A Markdown twin: title, optional date and summary, then the body. */
export function composeMarkdownTwin(
  title: string,
  body: string,
  opts: { summary?: string; published?: string } = {},
): string {
  const parts = [`# ${title}`];
  if (opts.published) parts.push(`**Date:** ${opts.published}`);
  if (opts.summary?.trim()) parts.push(`> ${opts.summary.trim()}`);
  parts.push(body.trimEnd());
  return parts.join("\n\n") + "\n";
}

export function isExternal(url: string): boolean {
  return /^(https?:|mailto:|tel:)/i.test(url);
}
