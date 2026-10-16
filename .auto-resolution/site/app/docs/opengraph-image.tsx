import { docsOverviewCard, OG_CONTENT_TYPE, OG_SIZE } from "@/lib/og";

export const alt = "Grit docs: the command line and the library";
export const size = OG_SIZE;
export const contentType = OG_CONTENT_TYPE;

export default function Image() {
  return docsOverviewCard();
}
