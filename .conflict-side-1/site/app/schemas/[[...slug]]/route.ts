import { loadSchemas } from "@/lib/schemas";
import { jsonSchema } from "@/lib/respond";

export const dynamic = "force-static";
export const dynamicParams = false;

export function generateStaticParams() {
  return loadSchemas().map((s) => ({ slug: [s.command, "v1.json"] }));
}

export async function GET(_req: Request, { params }: { params: Promise<{ slug?: string[] }> }) {
  const slug = (await params).slug;
  if (!slug || slug.length !== 2 || slug[1] !== "v1.json") {
    return new Response("Not found", { status: 404 });
  }
  const schema = loadSchemas().find((s) => s.command === slug[0]);
  if (!schema) return new Response("Not found", { status: 404 });
  return jsonSchema(schema.body);
}
