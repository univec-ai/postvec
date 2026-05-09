import DefaultTheme from "vitepress/theme";
import type { Theme } from "vitepress";
import HomePage from "./components/HomePage.vue";
import DownloadPanel from "./components/DownloadPanel.vue";
import CopyCommand from "./components/CopyCommand.vue";
import PgSnippet from "./components/PgSnippet.vue";
import "./custom.css";

export default {
  extends: DefaultTheme,
  enhanceApp({ app }) {
    app.component("HomePage", HomePage);
    app.component("DownloadPanel", DownloadPanel);
    app.component("CopyCommand", CopyCommand);
    app.component("PgSnippet", PgSnippet);
  },
} satisfies Theme;
