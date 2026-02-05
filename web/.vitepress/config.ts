import { defineConfig } from "vitepress";
import { SITE } from "./theme/site";

export default defineConfig({
  title: "postvec",
  titleTemplate: ":title · postvec",
  description: SITE.description,
  lang: "en-US",
  cleanUrls: true,
  lastUpdated: false,
  ignoreDeadLinks: false,
  srcExclude: ["README.md"],
  sitemap: {
    hostname: SITE.url,
  },
  head: [
    ["link", { rel: "icon", type: "image/svg+xml", href: "/favicon.svg" }],
    ["meta", { name: "theme-color", content: "#f0f2ee" }],
    ["meta", { name: "color-scheme", content: "light dark" }],
    ["link", { rel: "preconnect", href: "https://fonts.googleapis.com" }],
    [
      "link",
      { rel: "preconnect", href: "https://fonts.gstatic.com", crossorigin: "" },
    ],
    [
      "link",
      {
        rel: "stylesheet",
        href: "https://fonts.googleapis.com/css2?family=IBM+Plex+Mono:ital,wght@0,400;0,500;1,400&family=IBM+Plex+Sans:ital,wght@0,400;0,500;0,600;1,400&family=IBM+Plex+Serif:ital,wght@0,400;0,500;0,600;1,400&display=swap",
      },
    ],
    ["meta", { property: "og:type", content: "website" }],
    ["meta", { property: "og:site_name", content: "postvec" }],
    ["meta", { property: "og:title", content: SITE.title }],
    ["meta", { property: "og:description", content: SITE.description }],
    ["meta", { property: "og:url", content: SITE.url }],
    ["meta", { name: "twitter:card", content: "summary" }],
    ["meta", { name: "twitter:title", content: SITE.title }],
    ["meta", { name: "twitter:description", content: SITE.description }],
  ],
  themeConfig: {
    logo: { light: "/logo.svg", dark: "/logo-dark.svg", alt: "postvec" },
    siteTitle: "postvec",
    search: {
      provider: "local",
      options: {
        detailedView: true,
        miniSearch: {
          searchOptions: {
            fuzzy: 0.2,
            prefix: true,
          },
        },
      },
    },
    nav: [
      { text: "Docs", link: "/docs/" },
      { text: "Install", link: "/docs/install/" },
      { text: "Guides", link: "/docs/guides/" },
      { text: "Models", link: "/docs/models/" },
      { text: "Downloads", link: "/download" },
    ],
    sidebar: {
      "/docs/": [
        {
          text: "Get started",
          collapsed: false,
          items: [
            { text: "Overview", link: "/docs/" },
            { text: "Quick start", link: "/docs/quickstart" },
            { text: "Which SQL call", link: "/docs/guides/starting" },
            { text: "How it works", link: "/docs/concepts/" },
          ],
        },
        {
          text: "Install",
          collapsed: true,
          items: [
            { text: "Choose a method", link: "/docs/install/" },
            { text: "Docker", link: "/docs/install/docker" },
            { text: "Packages", link: "/docs/install/packages" },
            { text: "Build from source", link: "/docs/install/source" },
            { text: "Configure the cluster", link: "/docs/install/setup" },
            { text: "Upgrade", link: "/docs/install/upgrade" },
            { text: "Uninstall", link: "/docs/install/uninstall" },
          ],
        },
        {
          text: "Use",
          collapsed: true,
          items: [
            { text: "Usage overview", link: "/docs/guides/" },
            { text: "Enable a column", link: "/docs/guides/enable" },
            { text: "Search", link: "/docs/guides/search" },
            { text: "Filters", link: "/docs/guides/filters" },
            { text: "Indexes", link: "/docs/guides/indexes" },
            { text: "Templates", link: "/docs/guides/templates" },
            { text: "Chunk long documents", link: "/docs/guides/chunking" },
            { text: "Adopt existing vectors", link: "/docs/guides/adopt" },
            { text: "Search a retired space", link: "/docs/guides/bridge" },
            { text: "Migrate in place", link: "/docs/guides/migrate" },
          ],
        },
        {
          text: "Operate",
          collapsed: true,
          items: [
            { text: "Status and health", link: "/docs/guides/status" },
            { text: "Retry dead jobs", link: "/docs/guides/retry" },
            { text: "Backup and restore", link: "/docs/guides/backup" },
            { text: "One-shot helpers", link: "/docs/guides/helpers" },
          ],
        },
        {
          text: "Models",
          collapsed: true,
          items: [
            { text: "How models work", link: "/docs/models/" },
            { text: "Pull, activate, remove", link: "/docs/models/pull" },
            { text: "Login and private catalogue", link: "/docs/models/login" },
            { text: "Air-gapped hosts", link: "/docs/models/air-gapped" },
          ],
        },
        {
          text: "Understand",
          collapsed: true,
          items: [
            { text: "Eventual consistency", link: "/docs/concepts/consistency" },
            { text: "Embedded vs remote", link: "/docs/concepts/modes" },
            { text: "Vector lock-in", link: "/docs/concepts/lock-in" },
          ],
        },
        {
          text: "Reference",
          collapsed: true,
          items: [
            { text: "SQL functions", link: "/docs/reference/sql" },
            { text: "CLI", link: "/docs/reference/cli" },
            { text: "GUCs", link: "/docs/reference/gucs" },
            { text: "Limits", link: "/docs/limits" },
            { text: "Security", link: "/docs/security" },
            { text: "Troubleshooting", link: "/docs/troubleshooting" },
            { text: "FAQ", link: "/docs/faq" },
          ],
        },
      ],
    },
    socialLinks: [{ icon: "github", link: SITE.github }],
    outline: { level: [2, 3], label: "On this page" },
    footer: {
      message:
        'PostgreSQL License · a <a href="https://univec.ai">UniVec</a> project',
      copyright: "© 2026 UniVec",
    },
    docFooter: {
      prev: "Previous",
      next: "Next",
    },
  },
});
