import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { resetSiteCache } from "../lib/docs";
import { REPO_ROOT } from "../lib/repo";

/** Copy content/docs to a temp dir, apply `edit`, and point the site loader at it. */
export function withContentCopy(edit: (dir: string) => void): () => void {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "grit-docs-"));
  fs.cpSync(path.join(REPO_ROOT, "content", "docs"), dir, { recursive: true });
  edit(dir);
  process.env.GRIT_DOCS_CONTENT = dir;
  resetSiteCache();
  return () => {
    delete process.env.GRIT_DOCS_CONTENT;
    resetSiteCache();
    fs.rmSync(dir, { recursive: true, force: true });
  };
}
