import { findSchema, loadSchemas } from "@/lib/schemas";
import { jsonSchema } from "@/lib/respond";

export const dynamic = "force-static";
export const dynamicParams = false;

export function generateStaticParams() {
  return loadSchemas().map((s) => ({ slug: [s.command, `v${s.version}.json`] }));
}

export async function GET(_req: Request, { params }: { params: Promise<{ slug?: string[] }> }) {
  const slug = (await params).slug;
  if (!slug || slug.length !== 2) {
    return new Response("Not found", { status: 404 });
  }
  const version = slug[1] === "v2.json" ? 2 : slug[1] === "v1.json" ? 1 : null;
  if (version === null) {
    return new Response("Not found", { status: 404 });
  }
  const schema = findSchema(slug[0], version);
  if (!schema) return new Response("Not found", { status: 404 });
  return jsonSchema(schema.body);
}
