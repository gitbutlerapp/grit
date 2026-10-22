import "@fontsource/space-grotesk/400.css";
import "@fontsource/space-grotesk/500.css";
import "@fontsource/space-grotesk/700.css";
import "@fontsource/jetbrains-mono/400.css";
import "@fontsource/jetbrains-mono/500.css";
import "@fontsource/jetbrains-mono/700.css";
import "./globals.css";
import type { Metadata } from "next";
import Script from "next/script";
import { SITE_URL } from "@/lib/repo";

export const metadata: Metadata = {
  metadataBase: new URL(SITE_URL),
  title: "Grit - A new Git client and library, in Rust",
  description:
    "Compatible with every repository and every remote. Simpler to use, quick to run, and MIT-licensed throughout.",
  twitter: { card: "summary_large_image" },
  openGraph: { siteName: "Grit", type: "website" },
};

export default function RootLayout({ children }: { children: React.ReactNode }) {
  return (
    <html lang="en">
      <body>
        {children}
        <Script async src="https://u.gitbutler.com/script.js" data-website-id="2c6f680c-eaf5-4cd7-a419-1032ffab6bbc" />
      </body>
    </html>
  );
}
