import type { Metadata } from "next";
import { notFound } from "next/navigation";
import { DocsHeader, Sidebar } from "@/components/docs/Chrome";
import { CommandLayout, LibraryLayout, PlainLayout, StepsLayout } from "@/components/docs/Layouts";
import { ScrollSpy } from "@/components/docs/ScrollSpy";
import { findPage, isLibrarySlug, loadSite, STEP_GUIDE_SLUGS } from "@/lib/docs";
import { initHighlighter } from "@/lib/highlight";
import { docsCardFile, OG_SIZE } from "@/lib/og";

export const dynamicParams = false;

export function generateStaticParams() {
  return loadSite()
    .pages.filter((p) => p.slug !== "index")
    .map((p) => ({ slug: p.slug.split("/") }));
}

async function pageFor(params: Promise<{ slug: string[] }>) {
  const slug = (await params).slug.join("/");
  return findPage(loadSite(), slug);
}

export async function generateMetadata({ params }: { params: Promise<{ slug: string[] }> }): Promise<Metadata> {
  const page = await pageFor(params);
  if (!page) return {};
  const url = `/docs/${page.slug}/`;
  const title = `${page.title} - Grit docs`;
  return {
    title,
    description: page.summary,
    alternates: { canonical: url, types: { "text/markdown": `${url}index.md` } },
    openGraph: {
      url,
      title,
      description: page.summary,
      images: [{ url: `/og/docs/${docsCardFile(page.slug)}`, ...OG_SIZE, alt: `${page.title}: ${page.summary}` }],
    },
  };
}

export default async function DocsPage({ params }: { params: Promise<{ slug: string[] }> }) {
  const page = await pageFor(params);
  if (!page) notFound();
  await initHighlighter();
  const site = loadSite();
  const steps = STEP_GUIDE_SLUGS.has(page.slug);
  let layout: React.ReactNode;
  if (page.isCommand) layout = <CommandLayout page={page} site={site} />;
  else if (steps) layout = <StepsLayout page={page} site={site} />;
  else layout = (isLibrarySlug(page.slug) && LibraryLayout({ page, site })) || <PlainLayout page={page} site={site} />;
  return (
    <>
      <DocsHeader site={site} slug={page.slug} />
      <div className="docs-body">
        <Sidebar site={site} current={page.slug} toc={steps ? page.toc : []} />
        {layout}
      </div>
      {steps ? <ScrollSpy /> : null}
    </>
  );
}
