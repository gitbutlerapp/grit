/** The commit-graph rail beside each homepage and blog section; each part is a mark's CSS class. */
export function Rail({ parts, style }: { parts: string[]; style?: Record<string, React.CSSProperties> }) {
  return (
    <div className="rail" aria-hidden="true">
      {parts.map((part, i) => (
        <span key={i} className={part} style={style?.[part]} />
      ))}
    </div>
  );
}

export function TopBar({ blogHref = "/blog/" }: { blogHref?: string }) {
  return (
    <header className="topbar">
      <a className="brand" href="/" aria-label="grit homepage">
        grit
      </a>
      <nav className="nav" aria-label="Primary">
        <a href="/docs/">Docs</a>
        <a href="/docs/library/">Library</a>
        <a href={blogHref}>Blog</a>
        <a href="https://maint.grit-scm.com">Cooking</a>
        <a className="pill" href="https://github.com/gitbutlerapp/grit">
          GitHub
        </a>
      </nav>
    </header>
  );
}

export function Footer({ children }: { children: React.ReactNode }) {
  return (
    <footer className="commit">
      <Rail parts={["line stub", "root-dot"]} />
      <div className="footer">
        <div className="sha">
          0000001 · initial commit · by <a href="https://gitbutler.com">GitButler</a>
        </div>
        <div className="links">{children}</div>
      </div>
    </footer>
  );
}
