import type { Metadata } from "next";
import { Footer, Rail, TopBar } from "@/components/Rail";
import { BLOG_DESCRIPTION, BLOG_TITLE, displayDate, loadPosts, shortSha } from "@/lib/blog";

export const metadata: Metadata = {
  title: "Blog - the Grit project",
  description: BLOG_DESCRIPTION,
  alternates: { canonical: "/blog/", types: { "application/rss+xml": "/blog/feed.xml" } },
  openGraph: { url: "/blog/", title: "Blog - the Grit project", description: BLOG_DESCRIPTION },
};

export default function BlogIndex() {
  const posts = loadPosts();
  return (
    <>
      <TopBar blogHref="./" />
      <main>
        <section className="commit">
          <Rail parts={["line from-head", "head-dot"]} />
          <div className="hero">
            <div className="refs">
              <span className="ref">HEAD → blog</span>
              <a href="feed.xml">feed.xml</a>
            </div>
            <h1>Blog</h1>
            <p className="tagline">{BLOG_TITLE[0].toUpperCase() + BLOG_TITLE.slice(1)}.</p>
            <p className="lede">{BLOG_DESCRIPTION}</p>
          </div>
        </section>
        {posts.map((post, i) => (
          <section className="commit" key={post.slug}>
            <Rail parts={["line", i === 0 ? "head-dot small" : "dot"]} />
            <div className="entry">
              <div className={i === 0 ? "sha hot" : "sha"}>
                {shortSha(post)} · <time dateTime={post.published}>{displayDate(post.published, "short")}</time> ·{" "}
                {post.author}
              </div>
              <h2>
                <a href={`${post.slug}/`}>{post.title}</a>
              </h2>
              {post.summary ? <p>{post.summary}</p> : null}
            </div>
          </section>
        ))}
      </main>
      <Footer>
        <a href="/">Home</a>
        <a href="feed.xml">RSS feed</a>
        <a href="https://github.com/gitbutlerapp/grit">GitHub</a>
      </Footer>
    </>
  );
}
