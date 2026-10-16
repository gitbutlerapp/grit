import { findPage, loadSite } from "@/lib/docs";
import { pageTwin } from "@/lib/llms";
import { markdown } from "@/lib/respond";

// Served at /docs/<page>/index.md through a rewrite in next.config.ts.
export const dynamic = "force-static";
export const dynamicParams = false;

export function generateStaticParams() {
  return loadSite().pages.map((p) => ({ slug: p.slug === "index" ? [] : p.slug.split("/") }));
}

export async function GET(_req: Request, { params }: { params: Promise<{ slug?: string[] }> }) {
  const slug = (await params).slug?.join("/") || "index";
  const site = loadSite();
  const page = findPage(site, slug);
  if (!page) return new Response("Not found", { status: 404 });
  return markdown(pageTwin(page, site));
}
