import DefaultTheme from "vitepress/theme";
import type { Theme } from "vitepress";
import HomePage from "./components/HomePage.vue";
import DownloadPanel from "./components/DownloadPanel.vue";
import CopyCommand from "./components/CopyCommand.vue";
import PgSnippet from "./components/PgSnippet.vue";
import ServerPage from "./components/ServerPage.vue";
import OpenCoreStack from "./components/OpenCoreStack.vue";
import DownloadHero from "./components/DownloadHero.vue";
import "./custom.css";

export default {
  extends: DefaultTheme,
  enhanceApp({ app, router }) {
    app.component("HomePage", HomePage);
    app.component("DownloadPanel", DownloadPanel);
    app.component("CopyCommand", CopyCommand);
    app.component("PgSnippet", PgSnippet);
    app.component("ServerPage", ServerPage);
    app.component("OpenCoreStack", OpenCoreStack);
    app.component("DownloadHero", DownloadHero);

    // Client-side nav to /page#section (e.g. UniVec -> postvec pro) does
    // not scroll on layout: page. Do it after the new page has painted.
    if (typeof window === "undefined") return;
    const prev = router.onAfterRouteChange;
    router.onAfterRouteChange = async (to) => {
      await prev?.(to);
      const hash = to.split("#")[1];
      if (!hash) return;
      const id = decodeURIComponent(hash);
      const go = () => document.getElementById(id)?.scrollIntoView();
      requestAnimationFrame(() => requestAnimationFrame(go));
      window.setTimeout(go, 120);
    };
  },
} satisfies Theme;
