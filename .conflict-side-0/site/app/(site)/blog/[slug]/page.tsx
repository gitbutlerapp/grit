import type { Metadata } from "next";
import { notFound } from "next/navigation";
import { Footer, Rail, TopBar } from "@/components/Rail";
import { BLOG_DESCRIPTION, displayDate, loadPosts, shortSha } from "@/lib/blog";
import { initHighlighter } from "@/lib/highlight";
import { parseDoc, renderHtml } from "@/lib/markdown";

export const dynamicParams = false;

export function generateStaticParams() {
  return loadPosts().map((post) => ({ slug: post.slug }));
}

function findPost(slug: string) {
  return loadPosts().find((p) => p.slug === slug);
}

export async function generateMetadata({ params }: { params: Promise<{ slug: string }> }): Promise<Metadata> {
  const post = findPost((await params).slug);
  if (!post) return {};
  const url = `/blog/${post.slug}/`;
  return {
    title: `${post.title} - the Grit project`,
    description: post.summary || BLOG_DESCRIPTION,
    alternates: { canonical: url, types: { "text/markdown": `${url}index.md` } },
    openGraph: { url, title: post.title, description: post.summary || BLOG_DESCRIPTION, type: "article" },
  };
}

export default async function BlogPost({ params }: { params: Promise<{ slug: string }> }) {
  const post = findPost((await params).slug);
  if (!post) notFound();
  await initHighlighter();
  const { root, toc } = parseDoc(post.body);
  return (
    <>
      <TopBar blogHref="../" />
      <main>
        <section className="commit">
          <Rail parts={["line from-head", "head-dot"]} />
          <div className="hero post-hero">
            <div className="refs">
              <a className="ref" href="../">
                ← blog
              </a>
              <span>{shortSha(post)}</span>
              <time dateTime={post.published}>{displayDate(post.published)}</time>
              <span>{post.author}</span>
            </div>
            <h1>{post.title}</h1>
            {post.summary ? <p className="lede">{post.summary}</p> : null}
          </div>
        </section>
        <section className="commit">
          <Rail parts={["line"]} />
          <div className="post-body">
            <article className="content" dangerouslySetInnerHTML={{ __html: renderHtml(root.children) }} />
            {toc.length ? (
              <aside className="toc" aria-label="On this page">
                <h2>On this page</h2>
                <ol>
                  {toc.map((item) => (
                    <li key={item.anchor} className={`toc-level-${item.level}`}>
                      <a href={`#${item.anchor}`}>{item.text}</a>
                    </li>
                  ))}
                </ol>
              </aside>
            ) : null}
          </div>
        </section>
      </main>
      <Footer>
        <a href="../">All posts</a>
        <a href="/">Home</a>
        <a href="../feed.xml">RSS feed</a>
      </Footer>
    </>
  );
}
