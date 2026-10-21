import { homeCard, OG_CONTENT_TYPE, OG_SIZE } from "@/lib/og";

export const alt = "Grit: a new Git client and library, in Rust";
export const size = OG_SIZE;
export const contentType = OG_CONTENT_TYPE;

export default function Image() {
  return homeCard();
}
