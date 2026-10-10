/** Internal link and anchor validation for docs and blog content. */
import type { Post } from "./blog";
import { linkTargetSlug, type Site } from "./docs";
import { collectLinks, parseDoc } from "./markdown";
import { SITE_URL } from "./repo";
import { isExternal } from "./text";

/** Static files and routes that live outside the docs pages. */
const OTHER_PATHS = new Set(["/", "/blog/", "/llms.txt", "/llms-full.txt", "/docs/grit-cli.md", "/docs/grit-lib.md", "/install", "/install.ps1", "/install-nightly", "/install-nightly.ps1"]);

/** Anchors a page renders: its headings plus ids the overview layout adds. */
function anchorsFor(site: Site): Map<string, Set<string>> {
  const anchors = new Map<string, Set<string>>();
  for (const page of site.pages) {
    const ids = new Set(page.toc.map((t) => t.anchor));
    for (const t of parseDoc(page.markdown).root.children) {
      const id = (t.data?.hProperties as { id?: string } | undefined)?.id;
      if (id) ids.add(id);
    }
    if (page.slug === "index") {
      ids.add("commands");
      ids.add("for-agents");
      for (const group of site.commandGroups) ids.add(group.toLowerCase().replace(/[^a-z0-9]+/g, "-"));
    }
    anchors.set(page.slug, ids);
  }
  return anchors;
}

/** Docs slug for a site path: `/docs/status/` or the twin `/docs/status/index.md`. */
function docsSlug(path: string): string {
  return path.slice(6).replace(/(^|\/)index\.md$/, "").replace(/\/$/, "") || "index";
}

/** Every broken internal link, as `source: target (reason)`. */
export function validateLinks(site: Site, posts: Post[]): string[] {
  const anchors = anchorsFor(site);
  const problems: string[] = [];

  const checkDocsTarget = (source: string, slug: string, fragment: string, href: string) => {
    const ids = anchors.get(slug);
    if (!ids) problems.push(`${source}: ${href} (no docs page "${slug}")`);
    else if (fragment && !ids.has(fragment)) problems.push(`${source}: ${href} (no #${fragment} on ${slug})`);
  };

  for (const page of site.pages) {
    const source = `content/docs page ${page.slug}`;
    for (const { href } of collectLinks(parseDoc(page.markdown).root.children)) {
      if (isExternal(href) && !href.startsWith(SITE_URL)) continue;
      const [target, fragment = ""] = href.split("#");
      if (!target) {
        if (fragment && !anchors.get(page.slug)?.has(fragment)) problems.push(`${source}: #${fragment} (no such heading)`);
        continue;
      }
      const path = target.startsWith(SITE_URL) ? target.slice(SITE_URL.length) : target;
      if (path.startsWith("/")) {
        if (OTHER_PATHS.has(path)) continue;
        if (path.startsWith("/docs/")) checkDocsTarget(source, docsSlug(path), fragment, href);
        else if (path.startsWith("/blog/")) {
          const slug = path.slice(6).replace(/\/$/, "");
          if (slug && !posts.some((p) => p.slug === slug)) problems.push(`${source}: ${href} (no blog post "${slug}")`);
        } else problems.push(`${source}: ${href} (unknown site path)`);
        continue;
      }
      if (/\.(md|txt|png|svg|html)$/.test(path)) continue;
      checkDocsTarget(source, linkTargetSlug(page.slug, path), fragment, href);
    }
  }

  for (const post of posts) {
    const source = `content/blog/${post.slug}`;
    for (const { href } of collectLinks(parseDoc(post.body).root.children)) {
      const local = href.startsWith(SITE_URL) ? href.slice(SITE_URL.length) : href.startsWith("/") ? href : null;
      if (!local) continue;
      const [path, fragment = ""] = local.split("#");
      if (OTHER_PATHS.has(path)) continue;
      if (path.startsWith("/docs/")) checkDocsTarget(source, docsSlug(path), fragment, href);
      else if (path.startsWith("/blog/")) {
        const slug = path.slice(6).replace(/\/$/, "");
        if (slug && !posts.some((p) => p.slug === slug)) problems.push(`${source}: ${href} (no blog post "${slug}")`);
      }
    }
  }
  return problems;
}
