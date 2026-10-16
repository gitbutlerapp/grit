import { displayDate, loadPosts, shortSha } from "@/lib/blog";
import { blogPostCard, OG_CONTENT_TYPE, OG_SIZE } from "@/lib/og";

export const alt = "Grit blog";
export const size = OG_SIZE;
export const contentType = OG_CONTENT_TYPE;
export const dynamicParams = false;

export function generateStaticParams() {
  return loadPosts().map((post) => ({ slug: post.slug }));
}

export default async function Image({ params }: { params: Promise<{ slug: string }> }) {
  const slug = (await params).slug;
  const post = loadPosts().find((p) => p.slug === slug);
  if (!post) throw new Error("unknown blog post");
  return blogPostCard({ title: post.title, summary: post.summary, published: displayDate(post.published), sha: shortSha(post) });
}
