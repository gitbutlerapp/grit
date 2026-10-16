import { describe, expect, it } from "vitest";
import { GET, generateStaticParams } from "../app/schemas/[[...slug]]/route";
import { loadSchemas, schemaPublicUrl } from "../lib/schemas";

describe("JSON schema routes", () => {
  it("generates a static param for every committed schema", () => {
    const commands = loadSchemas().map((s) => s.command);
    const params = generateStaticParams().map((p) => p.slug?.join("/"));
    expect(params).toHaveLength(commands.length);
    expect(new Set(params)).toEqual(new Set(commands.map((c) => `${c}/v1.json`)));
  });

  it("serves status.v1.json with application/schema+json", async () => {
    const res = await GET(new Request(schemaPublicUrl("status")), {
      params: Promise.resolve({ slug: ["status", "v1.json"] }),
    });
    expect(res.status).toBe(200);
    expect(res.headers.get("Content-Type")).toContain("application/schema+json");
    const body = await res.text();
    expect(body).toContain('"https://grit-scm.com/schemas/status/v1.json"');
    expect(body).toContain("ChangeJson");
  });

  it("404s unknown schema paths", async () => {
    const res = await GET(new Request("https://grit-scm.com/schemas/nope/v1.json"), {
      params: Promise.resolve({ slug: ["nope", "v1.json"] }),
    });
    expect(res.status).toBe(404);
  });
});
