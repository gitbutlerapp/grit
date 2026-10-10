"use client";

import { useEffect } from "react";

/** Marks the nav sub-list link (`data-spy`) for the heading currently in view. */
export function ScrollSpy() {
  useEffect(() => {
    const links = [...document.querySelectorAll<HTMLAnchorElement>("[data-spy]")];
    if (!links.length || !("IntersectionObserver" in window)) return;
    const mark = (id: string) => links.forEach((a) => a.classList.toggle("on", a.dataset.spy === id));
    const observer = new IntersectionObserver(
      (entries) => {
        const visible = entries
          .filter((e) => e.isIntersecting)
          .sort((a, b) => a.boundingClientRect.top - b.boundingClientRect.top)[0];
        if (visible) mark(visible.target.id);
      },
      { rootMargin: "0px 0px -70% 0px" },
    );
    links.forEach((a) => {
      const heading = document.getElementById(a.dataset.spy!);
      if (heading) observer.observe(heading);
    });
    mark(links[0].dataset.spy!);
    return () => observer.disconnect();
  }, []);
  return null;
}
