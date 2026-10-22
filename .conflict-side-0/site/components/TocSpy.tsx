"use client";

import { useEffect } from "react";

/**
 * Marks the table-of-contents link for the section being read (`aria-current="location"`):
 * the last heading that has scrolled past the top of the window. Renders nothing.
 */
export function TocSpy({ toc = ".toc" }: { toc?: string }) {
  useEffect(() => {
    const links = [...document.querySelectorAll<HTMLAnchorElement>(`${toc} a[href^="#"]`)];
    const headings = links
      .map((a) => document.getElementById(decodeURIComponent(a.hash.slice(1))))
      .filter((h): h is HTMLElement => !!h);
    if (!headings.length) return;
    let frame = 0;
    let current: HTMLAnchorElement | undefined;
    const update = () => {
      frame = 0;
      const line = Math.min(160, window.innerHeight * 0.25);
      let index = -1;
      for (let i = 0; i < headings.length; i++) if (headings[i].getBoundingClientRect().top <= line) index = i;
      // At the very bottom, the last section is current even if its heading never reaches the line.
      if (window.innerHeight + window.scrollY >= document.documentElement.scrollHeight - 2) index = headings.length - 1;
      const link = index >= 0 ? links.find((a) => a.hash === `#${headings[index].id}`) : undefined;
      if (link === current) return;
      current?.removeAttribute("aria-current");
      link?.setAttribute("aria-current", "location");
      current = link;
    };
    const onScroll = () => {
      if (!frame) frame = requestAnimationFrame(update);
    };
    update();
    window.addEventListener("scroll", onScroll, { passive: true });
    window.addEventListener("resize", onScroll);
    return () => {
      window.removeEventListener("scroll", onScroll);
      window.removeEventListener("resize", onScroll);
      if (frame) cancelAnimationFrame(frame);
      current?.removeAttribute("aria-current");
    };
  }, [toc]);
  return null;
}
