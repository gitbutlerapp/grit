import { NextResponse, type NextRequest } from "next/server";

/** Paths served as-is: extensionless files in public/ that scripts fetch directly. */
const FILES = new Set(["/install", "/install-nightly"]);

/**
 * Page URLs end in a slash (/docs/commit/). Redirect slashless page paths, but
 * never files: anything with an extension, and the install scripts.
 */
export function proxy(request: NextRequest) {
  const { pathname } = request.nextUrl;
  const file = FILES.has(pathname) || /\.[^/]+$/.test(pathname) || /\/(opengraph|twitter)-image[^/]*$/.test(pathname);
  if (pathname.endsWith("/") || file) return NextResponse.next();
  // Build the URL by hand: NextURL normalizes the trailing slash away.
  const url = new URL(`${pathname}/${request.nextUrl.search}`, request.url);
  return NextResponse.redirect(url, 308);
}

export const config = {
  matcher: ["/((?!_next/|api/).*)"],
};
