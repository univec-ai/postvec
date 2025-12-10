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
    ["meta", { name: "theme-color", content: "#F1ECE0" }],
    ["meta", { name: "color-scheme", content: "light dark" }],
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
    logo: { src: "/logo.svg", alt: "postvec" },
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
      { text: "Quick start", link: "/docs/quickstart" },
      { text: "Install", link: "/docs/install/" },
      { text: "SQL", link: "/docs/reference/sql" },
      { text: "Downloads", link: "/download" },
    ],
    sidebar: {
      "/docs/": [
        {
          text: "Start",
          items: [
            { text: "Overview", link: "/docs/" },
            { text: "Quick start", link: "/docs/quickstart" },
          ],
        },
        {
          text: "Install",
          items: [
            { text: "Installation options", link: "/docs/install/" },
            { text: "Docker", link: "/docs/install/docker" },
            { text: "Packages", link: "/docs/install/packages" },
            { text: "Build from source", link: "/docs/install/source" },
            { text: "Configure the cluster", link: "/docs/install/setup" },
          ],
        },
        {
          text: "Core workflows",
          items: [
            { text: "Usage overview", link: "/docs/guides/" },
            { text: "Enable a column", link: "/docs/guides/enable" },
            { text: "Search", link: "/docs/guides/search" },
            { text: "Filters", link: "/docs/guides/filters" },
            { text: "Templates", link: "/docs/guides/templates" },
            { text: "Chunk long documents", link: "/docs/guides/chunking" },
          ],
        },
        {
          text: "Existing vectors",
          items: [
            { text: "Adopt existing vectors", link: "/docs/guides/adopt" },
            { text: "Search without migrating", link: "/docs/guides/bridge" },
            { text: "Migrate in place", link: "/docs/guides/migrate" },
          ],
        },
        {
          text: "Operate",
          items: [
            { text: "Indexes", link: "/docs/guides/indexes" },
            { text: "Retry dead jobs", link: "/docs/guides/retry" },
            { text: "Status and health", link: "/docs/guides/status" },
            { text: "Backup and restore", link: "/docs/guides/backup" },
            { text: "Upgrade", link: "/docs/install/upgrade" },
            { text: "Uninstall", link: "/docs/install/uninstall" },
          ],
        },
        {
          text: "Models",
          items: [
            { text: "How models work", link: "/docs/models/" },
            { text: "Pull, upgrade, remove", link: "/docs/models/pull" },
            { text: "Login and private catalogue", link: "/docs/models/login" },
            { text: "Air-gapped hosts", link: "/docs/models/air-gapped" },
          ],
        },
        {
          text: "Concepts",
          items: [
            { text: "Vector lock-in and embedding debt", link: "/docs/concepts/lock-in" },
            { text: "How postvec works", link: "/docs/concepts/" },
            { text: "Embedded vs remote", link: "/docs/concepts/modes" },
            { text: "Eventual consistency", link: "/docs/concepts/consistency" },
          ],
        },
        {
          text: "Reference",
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
    outline: { level: [2, 3], label: "Contents" },
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
