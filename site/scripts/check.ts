/**
 * `npm run check`: validate the docs and blog content without a full build.
 *
 * Loading the site in strict mode already fails on manifest problems, code
 * fences without a language, missing includes, bad benchmark baselines and
 * `rustdoc:` links that don't resolve (CI builds rustdoc first). On top of that
 * this checks every internal link and `#anchor` in the docs and blog.
 */
process.env.GRIT_SITE_STRICT ??= "1";

const { loadSite } = await import("../lib/docs");
const { loadPosts } = await import("../lib/blog");
const { validateLinks } = await import("../lib/validate");

const site = loadSite();
const posts = loadPosts();
const problems = validateLinks(site, posts);
if (problems.length) {
  for (const problem of problems) console.error(problem);
  console.error(`\n${problems.length} broken link(s) in content/docs or content/blog`);
  process.exit(1);
}
console.log(`docs ok: ${site.pages.length} pages (${site.commandPages.length} commands), ${posts.length} blog posts`);

export {};
