import fs from "node:fs";
import path from "node:path";

/** Repository root. The site lives in `site/`, so sources are one level up. */
export const REPO_ROOT = process.env.GRIT_REPO_ROOT
  ? path.resolve(process.env.GRIT_REPO_ROOT)
  : path.resolve(process.cwd(), "..");

export const SITE_URL = "https://grit-scm.com";
export const GITHUB_URL = "https://github.com/gitbutlerapp/grit";
export const GITHUB_BLOB = `${GITHUB_URL}/blob/main`;
export const DOCS_RS = "https://docs.rs/grit-lib";

/** Docs Markdown sources (`content/docs`), overridable for tests. */
export function docsContentDir(): string {
  return process.env.GRIT_DOCS_CONTENT ?? path.join(REPO_ROOT, "content", "docs");
}

export function blogContentDir(): string {
  return process.env.GRIT_BLOG_CONTENT ?? path.join(REPO_ROOT, "content", "blog");
}

/** Local rustdoc output (`cargo doc -p grit-lib --no-deps`). */
export function rustdocRoot(): string {
  return process.env.GRIT_RUSTDOC_ROOT ?? path.join(REPO_ROOT, "target", "doc");
}

/**
 * Strict mode (CI and production builds): missing rustdoc, unresolved
 * `rustdoc:` links and broken internal links fail the build instead of
 * degrading gracefully for local preview.
 */
export function strict(): boolean {
  return process.env.GRIT_SITE_STRICT === "1";
}

/** Read a repository file given a repo-relative path, refusing paths that escape it. */
export function readRepoFile(rel: string): string {
  const abs = path.resolve(REPO_ROOT, rel);
  if (!abs.startsWith(REPO_ROOT + path.sep)) {
    throw new Error(`path escapes repository: ${rel}`);
  }
  if (!fs.existsSync(abs)) {
    throw new Error(`missing file: ${rel}`);
  }
  return fs.readFileSync(abs, "utf8");
}
