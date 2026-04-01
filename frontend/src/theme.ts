import { createTheme, Badge, NavLink } from "@mantine/core";

const CJK_FONTS =
  '-apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, "Noto Sans SC", "PingFang SC", "Microsoft YaHei", sans-serif';

export const theme = createTheme({
  fontFamily: CJK_FONTS,
  headings: { fontFamily: CJK_FONTS },
  components: {
    Badge: Badge.extend({
      styles: {
        root: { height: "auto", paddingBlock: 4 },
        label: { overflow: "visible", lineHeight: 1.6 },
      },
    }),
    NavLink: NavLink.extend({
      styles: {
        root: { minHeight: 40 },
        label: { lineHeight: 1.6 },
      },
    }),
  },
});
