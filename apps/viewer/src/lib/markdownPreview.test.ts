import { expect, it } from "vitest";
import { markdownPreview } from "./markdownPreview";

it.each([
  ["**All 18 tests pass.**", "All 18 tests pass."],
  ["Merged [**PR #18**](https://example.com/pull/18).", "Merged PR #18."],
  ["See [the fix][pr].\n\n[pr]: https://example.com", "See the fix."],
  ["# Next steps\n\n- First task", "Next steps"],
  ["`**literal**` or a_b_c; 2 * 3.", "**literal** or a_b_c; 2 * 3."],
  ["```sh\nprintf '**ready**'\n```", "printf '**ready**'"],
  ["Escaped \\*stars\\* and [plain brackets]", "Escaped *stars* and [plain brackets]"],
  ["![Build report](file:///example/image.png)", "Build report"],
  ["https://example.com/a_b?q=1", "https://example.com/a_b?q=1"],
])("formats a one-line preview of %s", (source, expected) => {
  expect(markdownPreview(source)).toBe(expected);
});
