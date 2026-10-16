import type { NextConfig } from "next";

const config: NextConfig = {
  // Keep the URLs the static site had: /docs/commit/, /blog/introducing-grit/.
  trailingSlash: true,
  // proxy.ts adds the slash for pages but leaves /install and friends alone, so
  // `curl -fsSL https://grit-scm.com/install | sh` gets the script, not a redirect.
  skipTrailingSlashRedirect: true,
  async rewrites() {
    // Markdown twins live at <page>/index.md next to each page.
    return [
      { source: "/docs/index.md", destination: "/md/docs" },
      { source: "/docs/:path*/index.md", destination: "/md/docs/:path*" },
      { source: "/blog/:slug/index.md", destination: "/md/blog/:slug" },
    ];
  },
  async headers() {
    const text = [{ key: "Content-Type", value: "text/plain; charset=utf-8" }];
    return ["/install", "/install-nightly", "/install.ps1", "/install-nightly.ps1"].map((source) => ({
      source,
      headers: text,
    }));
  },
};

export default config;
