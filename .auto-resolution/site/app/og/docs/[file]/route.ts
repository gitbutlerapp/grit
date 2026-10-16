import { findPage, loadSite } from "@/lib/docs";
import { docsCardFile, docsPageCard } from "@/lib/og";

// Share cards for docs pages, at /og/docs/<slug with / as __>.png. (Next.js
// can't nest opengraph-image under the docs catch-all route.)
export const dynamic = "force-static";
export const dynamicParams = false;

export function generateStaticParams() {
  return loadSite()
    .pages.filter((p) => p.slug !== "index")
    .map((p) => ({ file: docsCardFile(p.slug) }));
}

export async function GET(_req: Request, { params }: { params: Promise<{ file: string }> }) {
  const file = (await params).file;
  const page = loadSite().pages.find((p) => docsCardFile(p.slug) === file);
  if (!page) return new Response("Not found", { status: 404 });
  return docsPageCard(page);
}
