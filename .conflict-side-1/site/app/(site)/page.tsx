import type { Metadata } from "next";
import { Footer, Rail, TopBar } from "@/components/Rail";
import { highlightHtml, initHighlighter } from "@/lib/highlight";

export const metadata: Metadata = {
  alternates: {
    canonical: "/",
    types: { "text/plain": "/llms.txt", "text/markdown": "/docs/grit-cli.md" },
  },
  openGraph: { url: "/" },
};

const LIB_EXAMPLE = `use grit_lib::{diff, repo::Repository};

let repo = Repository::discover(None)?;
let index = repo.load_index()?;
if let Some(wt) = &repo.work_tree {
  for f in diff::diff_index_to_worktree(&repo.odb, &index, wt, false, false)? {
    println!("{:?} {}", f.status, f.path());
  }
}`;

export default async function Home() {
  await initHighlighter();
  return (
    <>
      <TopBar />
      <main>
        <section className="commit">
          <Rail parts={["line", "head-dot"]} style={{ line: { top: 110 } }} />
          <div className="hero">
            <div className="refs">
              <span className="ref">HEAD → main</span>
              <span>c0ffee1</span>
            </div>
            <h1>Grit</h1>
            <p className="tagline">A new Git client and library, in Rust.</p>
            <p className="lede">
              Compatible with every repository and every remote. Simpler to use, quick to run, and MIT-licensed
              throughout.
            </p>
            <div className="install">
              <code className="primary">$ curl -fsSL grit-scm.com/install | sh</code>
              <code className="secondary">cargo add grit-lib</code>
            </div>
          </div>
        </section>

        <section className="commit">
          <Rail parts={["line", "dot"]} />
          <div className="body">
            <div>
              <div className="sha">c0deba5e · feat(compat)</div>
              <h2>Every repo you already have.</h2>
            </div>
            <p>
              Same .git directory, same pack format, same remotes. GitHub, GitLab, your CI and your teammates won&apos;t
              notice. Switch to Grit mid-branch and switch back whenever you like.
            </p>
          </div>
        </section>

        <section className="commit">
          <Rail parts={["line", "branch-out", "branch-line", "branch-in", "branch-dot"]} />
          <div className="body branched">
            <div>
              <div className="sha">ca11ab1e · feat(lib) · branch: grit-lib</div>
              <h2>Link it. Don&apos;t shell out.</h2>
              <p>
                Objects, packs, index, refs, diff, merge as typed Rust modules. No process spawn, no parsing porcelain
                output.
              </p>
            </div>
            <div dangerouslySetInnerHTML={{ __html: highlightHtml("rust", LIB_EXAMPLE) }} />
          </div>
        </section>

        <section className="commit">
          <Rail parts={["line", "dot"]} />
          <div className="body">
            <div>
              <div className="sha">deadbeef · refactor(cli)</div>
              <h2>Simpler on purpose.</h2>
            </div>
            <p>
              The commands you know, with clearer output and fewer footguns. The CLI is a thin layer over the library,
              so whatever the CLI can do, your code can do.
            </p>
          </div>
        </section>

        <section className="commit">
          <Rail parts={["line", "dot"]} />
          <div className="body">
            <div>
              <div className="sha">feedface · docs(agents)</div>
              <h2>Docs your agent can read.</h2>
            </div>
            <p>
              Each half of the docs is one Markdown file: <a href="/docs/grit-cli.md">grit-cli.md</a> teaches an agent
              the CLI for everyday Git work, and <a href="/docs/grit-lib.md">grit-lib.md</a> covers building
              Git-compatible Rust programs. <a href="/llms.txt">llms.txt</a> indexes every page.
            </p>
          </div>
        </section>
      </main>
      <Footer>
        <a href="/docs/">Read the docs</a>
        <a href="/blog/introducing-grit/">Why we built it</a>
        <a href="https://maint.grit-scm.com">What&apos;s cooking</a>
        <a href="https://github.com/gitbutlerapp/grit/blob/main/TESTING.md">Testing</a>
      </Footer>
    </>
  );
}
