/**
 * Committed v1 JSON Schemas for `grit --json` outcomes (`content/schemas/*.v1.json`).
 */
import fs from "node:fs";
import path from "node:path";
import type { Site } from "./docs";
import { docsContentDir, readRepoFile, REPO_ROOT, SITE_URL } from "./repo";

const FILE_V1 = /^(.+)\.v1\.json$/;
const FILE_V2 = /^(.+)\.v2\.json$/;

/** Public URL for a command path schema document (`status`, `stash.list`, …). */
export function schemaPublicUrl(command: string): string {
  return `${SITE_URL}/schemas/${command}/v1.json`;
}

/** Public URL for a committed schema file (v1 or v2). */
export function schemaPublicUrlFor(schema: SchemaFile): string {
  return `${SITE_URL}/schemas/${schema.command}/v${schema.version}.json`;
}

/** Site-relative href for a schema (for Markdown in docs pages). */
export function schemaHref(command: string): string {
  return `/schemas/${command}/v1.json`;
}

/** Site-relative href for a v2 schema document. */
export function schemaHrefV2(command: string): string {
  return `/schemas/${command}/v2.json`;
}

export function schemasContentDir(): string {
  return process.env.GRIT_SCHEMAS_CONTENT ?? path.join(REPO_ROOT, "content", "schemas");
}

export interface SchemaFile {
  command: string;
  version: 1 | 2;
  relPath: string;
  body: string;
}

let cache: SchemaFile[] | undefined;

function loadSchemaFiles(suffix: string, version: 1 | 2, pattern: RegExp): SchemaFile[] {
  const dir = schemasContentDir();
  return fs
    .readdirSync(dir)
    .filter((n) => n.endsWith(suffix))
    .sort()
    .map((name) => {
      const m = pattern.exec(name);
      if (!m) throw new Error(`unexpected schema file name ${name}`);
      const command = m[1];
      const relPath = path.join("content", "schemas", name);
      return { command, version, relPath, body: readRepoFile(relPath) };
    });
}

/** Every committed v1 schema file, sorted by command path. */
export function loadSchemas(): SchemaFile[] {
  if (cache) return cache;
  if (!fs.existsSync(schemasContentDir())) {
    throw new Error(`missing schemas directory ${schemasContentDir()}`);
  }
  cache = [
    ...loadSchemaFiles(".v1.json", 1, FILE_V1),
    ...loadSchemaFiles(".v2.json", 2, FILE_V2),
  ];
  return cache;
}

/** Lookup one schema by command path and version. */
export function findSchema(command: string, version: 1 | 2): SchemaFile | undefined {
  return loadSchemas().find((s) => s.command === command && s.version === version);
}

export function resetSchemasCache(): void {
  cache = undefined;
}

/** Docs page slug that should link a schema (command pages and `agents` for `error`). */
export function schemaDocSlug(command: string): string {
  if (command === "error") return "agents";
  return command.split(".")[0];
}

const SCHEMA_LINK = /\/schemas\/([a-z0-9_.-]+)\/v([12])\.json/g;

function markdownSources(): string[] {
  const contentDir = docsContentDir();
  const paths = [path.join(contentDir, "agents.md"), path.join(contentDir, "schemas.md")];
  const commands = path.join(contentDir, "commands");
  if (fs.existsSync(commands)) {
    for (const name of fs.readdirSync(commands)) {
      if (name.endsWith(".md") && name !== "README.md") paths.push(path.join(commands, name));
    }
  }
  return paths.filter((p) => fs.existsSync(p));
}

/** Collect schema command paths linked from docs Markdown sources. */
export function linkedSchemaCommands(_site: Site): { v1: Set<string>; v2: Set<string> } {
  const v1 = new Set<string>();
  const v2 = new Set<string>();
  for (const file of markdownSources()) {
    const text = fs.readFileSync(file, "utf8");
    for (const m of text.matchAll(SCHEMA_LINK)) {
      if (m[2] === "2") v2.add(m[1]);
      else v1.add(m[1]);
    }
  }
  return { v1, v2 };
}

/**
 * Every schema must be linked from a docs page; every `/schemas/.../v1.json` link
 * must match a committed schema file.
 */
export function validateSchemaLinks(site: Site): string[] {
  const onDiskV1 = new Set(
    loadSchemas().filter((s) => s.version === 1).map((s) => s.command),
  );
  const onDiskV2 = new Set(
    loadSchemas().filter((s) => s.version === 2).map((s) => s.command),
  );
  const linked = linkedSchemaCommands(site);
  const problems: string[] = [];
  for (const command of onDiskV1) {
    if (!linked.v1.has(command)) {
      problems.push(
        `content/schemas/${command}.v1.json: no docs page links ${schemaHref(command)} (expected on /docs/${schemaDocSlug(command)}/)`,
      );
    }
  }
  for (const command of onDiskV2) {
    if (!linked.v2.has(command)) {
      problems.push(
        `content/schemas/${command}.v2.json: no docs page links ${schemaHrefV2(command)} (expected on /docs/${schemaDocSlug(command)}/)`,
      );
    }
  }
  for (const command of linked.v1) {
    if (!onDiskV1.has(command)) {
      problems.push(`docs link ${schemaHref(command)} but content/schemas/${command}.v1.json is missing`);
    }
  }
  for (const command of linked.v2) {
    if (!onDiskV2.has(command)) {
      problems.push(
        `docs link ${schemaHrefV2(command)} but content/schemas/${command}.v2.json is missing`,
      );
    }
  }
  return problems.sort();
}

/** Markdown line for a single schema link (used in command pages). */
export function schemaLinkLine(command: string): string {
  return `Schema: [v1](${schemaHref(command)})`;
}
