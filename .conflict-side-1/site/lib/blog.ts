/** Blog posts from `content/blog/*.md` (front matter: title, date, summary, author, slug). */
import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { blogContentDir, SITE_URL } from "./repo";
import { composeMarkdownTwin, parseFrontMatter, rewriteMarkdownLinks } from "./text";

export const BLOG_TITLE = "project notes from Grit";
export const BLOG_DESCRIPTION = "Short deep dives into building a Git-compatible, library-oriented Rust implementation.";
const AUTHOR = "the Grit project";

export interface Post {
  slug: string;
  title: string;
  summary: string;
  author: string;
  /** `YYYY-MM-DD`. */
  published: string;
  body: string;
  source: string;
}

let cached: Post[] | undefined;

export function loadPosts(): Post[] {
  if (cached) return cached;
  const dir = blogContentDir();
  const posts = fs
    .readdirSync(dir)
    .filter((n) => n.endsWith(".md"))
    .sort()
    .map((name) => {
      const source = path.join(dir, name);
      const { meta, body } = parseFrontMatter(fs.readFileSync(source, "utf8"));
      const stem = name.slice(0, -3);
      if (!meta.date) throw new Error(`${name}: missing required front matter field 'date'`);
      const heading = /^#\s+(.+)$/m.exec(body)?.[1];
      return {
        slug: meta.slug ?? stem,
        title: meta.title ?? heading ?? stem,
        summary: meta.summary ?? "",
        author: meta.author ?? AUTHOR,
        published: meta.date,
        body,
        source,
      };
    });
  cached = posts.sort((a, b) => (a.published === b.published ? b.slug.localeCompare(a.slug) : b.published.localeCompare(a.published)));
  return cached;
}

/** Decorative commit-style id for a post, stable across builds. */
export function shortSha(post: Post): string {
  return crypto.createHash("sha1").update(post.slug).digest("hex").slice(0, 7);
}

export function displayDate(iso: string, style: "long" | "short" = "long"): string {
  const date = new Date(`${iso}T00:00:00Z`);
  return date.toLocaleDateString("en-US", { month: style === "long" ? "long" : "short", day: "numeric", year: "numeric", timeZone: "UTC" });
}

export function postTwinUrl(slug: string): string {
  return `${SITE_URL}/blog/${slug}/index.md`;
}

/** The post's Markdown twin, with grit-scm.com links pointed at Markdown twins. */
export function postTwin(post: Post): string {
  const body = rewriteMarkdownLinks(post.body, (target) => {
    const blog = new RegExp(`^${SITE_URL}/blog/([^/#]+)/?$`).exec(target);
    if (blog) return postTwinUrl(blog[1]);
    const docs = new RegExp(`^${SITE_URL}/docs/(.*?)/?$`).exec(target);
    const rel = docs ? docs[1] : target.startsWith("/docs/") ? target.slice(6).replace(/\/$/, "") : null;
    if (rel === null) return null;
    return `${SITE_URL}/docs/${rel || "index"}/index.md`.replace("/docs/index/index.md", "/docs/index.md");
  });
  return composeMarkdownTwin(post.title, body, { summary: post.summary, published: post.published });
}
