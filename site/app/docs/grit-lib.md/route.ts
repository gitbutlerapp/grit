import { bundle } from "@/lib/llms";
import { markdown } from "@/lib/respond";

export const dynamic = "force-static";

/** The whole library half of the docs in one Markdown file. */
export function GET() {
  return markdown(bundle(true));
}
