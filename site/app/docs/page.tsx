import type { Metadata } from "next";
import { OverviewHeader } from "@/components/docs/Chrome";
import { OverviewLayout } from "@/components/docs/Layouts";
import { findPage, loadSite } from "@/lib/docs";

const DESCRIPTION =
  "How to use grit, a simple Git client built on grit-lib: a short tutorial and a man page for every command.";

export function generateMetadata(): Metadata {
  const page = findPage(loadSite(), "index");
  return {
    title: "Grit docs",
    description: page?.summary || DESCRIPTION,
    alternates: { canonical: "/docs/", types: { "text/markdown": "/docs/index.md" } },
    openGraph: { url: "/docs/", title: "Grit docs", description: page?.summary || DESCRIPTION },
  };
}

export default function DocsOverview() {
  const site = loadSite();
  return (
    <>
      <OverviewHeader />
      <div className="docs-body overview-body">
        <OverviewLayout site={site} />
      </div>
    </>
  );
}
