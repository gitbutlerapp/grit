import "./docs.css";

export default function DocsLayout({ children }: { children: React.ReactNode }) {
  return <div className="docs app">{children}</div>;
}
