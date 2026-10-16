import { BLOG_DESCRIPTION, BLOG_TITLE, loadPosts } from "@/lib/blog";
import { initHighlighter } from "@/lib/highlight";
import { parseDoc, renderHtml } from "@/lib/markdown";
import { SITE_URL } from "@/lib/repo";

export const dynamic = "force-static";

const xml = (s: string) => s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
const rfc822 = (iso: string) => new Date(`${iso}T00:00:00Z`).toUTCString().replace("GMT", "+0000");

export async function GET() {
  await initHighlighter();
  const posts = loadPosts();
  const items = posts
    .map((post) => {
      const url = `${SITE_URL}/blog/${post.slug}/`;
      return `  <item>
    <title>${xml(post.title)}</title>
    <link>${url}</link>
    <guid isPermaLink="true">${url}</guid>
    <pubDate>${rfc822(post.published)}</pubDate>
    <description>${xml(post.summary)}</description>
    <content:encoded><![CDATA[${renderHtml(parseDoc(post.body).root.children)}]]></content:encoded>
  </item>`;
    })
    .join("\n");
  const latest = posts.length ? rfc822(posts[0].published) : rfc822("1970-01-01");
  const body = `<?xml version="1.0" encoding="utf-8"?>
<rss version="2.0" xmlns:content="http://purl.org/rss/1.0/modules/content/">
<channel>
  <title>${xml(BLOG_TITLE)}</title>
  <link>${SITE_URL}/blog/</link>
  <description>${xml(BLOG_DESCRIPTION)}</description>
  <lastBuildDate>${latest}</lastBuildDate>
${items}
</channel>
</rss>
`;
  return new Response(body, { headers: { "Content-Type": "application/rss+xml; charset=utf-8" } });
}
