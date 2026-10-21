/**
 * Committed v1 JSON Schemas for `grit --json` outcomes (`content/schemas/*.v1.json`).
 */
import fs from "node:fs";
import path from "node:path";
import type { Site } from "./docs";
import { docsContentDir, readRepoFile, REPO_ROOT, SITE_URL } from "./repo";

const FILE = /^(.+)\.v1\.json$/;

/** Public URL for a command path schema document (`status`, `stash.list`, …). */
export function schemaPublicUrl(command: string): string {
  return `${SITE_URL}/schemas/${command}/v1.json`;
}

/** Site-relative href for a schema (for Markdown in docs pages). */
export function schemaHref(command: string): string {
  return `/schemas/${command}/v1.json`;
}

export function schemasContentDir(): string {
  return process.env.GRIT_SCHEMAS_CONTENT ?? path.join(REPO_ROOT, "content", "schemas");
}

export interface SchemaFile {
  command: string;
  relPath: string;
  body: string;
}

let cache: SchemaFile[] | undefined;

/** Every committed schema file, sorted by command path. */
export function loadSchemas(): SchemaFile[] {
  if (cache) return cache;
  const dir = schemasContentDir();
  if (!fs.existsSync(dir)) throw new Error(`missing schemas directory ${dir}`);
  const files = fs
    .readdirSync(dir)
    .filter((n) => n.endsWith(".v1.json"))
    .sort();
  cache = files.map((name) => {
    const m = FILE.exec(name);
    if (!m) throw new Error(`unexpected schema file name ${name}`);
    const command = m[1];
    const relPath = path.join("content", "schemas", name);
    return { command, relPath, body: readRepoFile(relPath) };
  });
  return cache;
}

export function resetSchemasCache(): void {
  cache = undefined;
}

/** Docs page slug that should link a schema (command pages and `agents` for `error`). */
export function schemaDocSlug(command: string): string {
  if (command === "error") return "agents";
  return command.split(".")[0];
}

const SCHEMA_LINK = /\/schemas\/([a-z0-9_.-]+)\/v1\.json/g;

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
export function linkedSchemaCommands(_site: Site): Set<string> {
  const linked = new Set<string>();
  for (const file of markdownSources()) {
    const text = fs.readFileSync(file, "utf8");
    for (const m of text.matchAll(SCHEMA_LINK)) linked.add(m[1]);
  }
  return linked;
}

/**
 * Every schema must be linked from a docs page; every `/schemas/.../v1.json` link
 * must match a committed schema file.
 */
export function validateSchemaLinks(site: Site): string[] {
  const onDisk = new Set(loadSchemas().map((s) => s.command));
  const linked = linkedSchemaCommands(site);
  const problems: string[] = [];
  for (const command of onDisk) {
    if (!linked.has(command)) {
      problems.push(
        `content/schemas/${command}.v1.json: no docs page links ${schemaHref(command)} (expected on /docs/${schemaDocSlug(command)}/)`,
      );
    }
  }
  for (const command of linked) {
    if (!onDisk.has(command)) {
      problems.push(`docs link ${schemaHref(command)} but content/schemas/${command}.v1.json is missing`);
    }
  }
  return problems.sort();
}

/** Markdown line for a single schema link (used in command pages). */
export function schemaLinkLine(command: string): string {
  return `Schema: [v1](${schemaHref(command)})`;
}
