import { loadPosts, postTwin } from "@/lib/blog";
import { markdown } from "@/lib/respond";

// Served at /blog/<post>/index.md through a rewrite in next.config.ts.
export const dynamic = "force-static";
export const dynamicParams = false;

export function generateStaticParams() {
  return loadPosts().map((post) => ({ slug: post.slug }));
}

export async function GET(_req: Request, { params }: { params: Promise<{ slug: string }> }) {
  const slug = (await params).slug;
  const post = loadPosts().find((p) => p.slug === slug);
  if (!post) return new Response("Not found", { status: 404 });
  return markdown(postTwin(post));
}
