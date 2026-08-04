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
      {
        text: "Install",
        items: [
          { text: "Docker", link: "/docs/install/docker" },
          { text: "Packages", link: "/docs/install/packages" },
          { text: "From source", link: "/docs/install/source" },
          { text: "Configure", link: "/docs/install/setup" },
        ],
      },
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
            { text: "SQL functions", link: "/docs/guides/" },
          ],
        },
        {
          text: "Install",
          collapsed: false,
          items: [
            { text: "Docker", link: "/docs/install/docker" },
            { text: "Packages", link: "/docs/install/packages" },
            { text: "From source", link: "/docs/install/source" },
            { text: "Configure", link: "/docs/install/setup" },
            { text: "Upgrade", link: "/docs/install/upgrade" },
            { text: "Uninstall", link: "/docs/install/uninstall" },
            { text: "Verify", link: "/docs/install/verify" },
          ],
        },
        {
          text: "Usage",
          collapsed: false,
          items: [
            { text: "Enable", link: "/docs/guides/enable" },
            {
              text: "Search",
              link: "/docs/guides/search",
              collapsed: true,
              items: [
                { text: "Filters", link: "/docs/guides/filters" },
                { text: "Indexes", link: "/docs/guides/indexes" },
              ],
            },
            { text: "Templates", link: "/docs/guides/templates" },
            { text: "Chunking", link: "/docs/guides/chunking" },
            {
              text: "Adopt",
              link: "/docs/guides/adopt",
              collapsed: true,
              items: [{ text: "Bridge", link: "/docs/guides/bridge" }],
            },
            { text: "Migrate", link: "/docs/guides/migrate" },
            {
              text: "Status",
              link: "/docs/guides/status",
              collapsed: true,
              items: [
                { text: "Retry", link: "/docs/guides/retry" },
                { text: "Backup", link: "/docs/guides/backup" },
                { text: "Helpers", link: "/docs/guides/helpers" },
              ],
            },
          ],
        },
        {
          text: "Models",
          collapsed: false,
          items: [
            { text: "How models work", link: "/docs/models/" },
            {
              text: "Pull",
              link: "/docs/models/pull",
              collapsed: true,
              items: [
                { text: "Login", link: "/docs/models/login" },
                { text: "Air-gapped", link: "/docs/models/air-gapped" },
              ],
            },
            {
              text: "Providers",
              link: "/docs/models/providers",
              collapsed: true,
              items: [
                { text: "UniVec", link: "/docs/models/univec" },
                { text: "Connector files", link: "/docs/models/providers-file" },
              ],
            },
          ],
        },
        {
          text: "Remote",
          collapsed: false,
          items: [
            { text: "Overview", link: "/docs/server/" },
            {
              text: "Run a node",
              link: "/docs/server/node",
              collapsed: true,
              items: [
                { text: "Dashboard", link: "/docs/server/dashboard" },
                { text: "Connect", link: "/docs/server/connect" },
                { text: "Node models", link: "/docs/server/models" },
              ],
            },
            { text: "Fleet", link: "/docs/server/fleet" },
            { text: "Managed schema", link: "/docs/server/managed" },
            { text: "HTTP API", link: "/docs/server/http-api" },
            { text: "Node reference", link: "/docs/server/reference" },
          ],
        },
        {
          text: "Reference",
          collapsed: false,
          items: [
            {
              text: "How it works",
              link: "/docs/concepts/",
              collapsed: true,
              items: [
                { text: "Consistency", link: "/docs/concepts/consistency" },
                { text: "Modes", link: "/docs/concepts/modes" },
                { text: "Lock-in", link: "/docs/concepts/lock-in" },
              ],
            },
            { text: "SQL", link: "/docs/reference/sql" },
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
