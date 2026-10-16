import { llmsFullTxt } from "@/lib/llms";
import { text } from "@/lib/respond";

export const dynamic = "force-static";

export function GET() {
  return text(llmsFullTxt());
}
