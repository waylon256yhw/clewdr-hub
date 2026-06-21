# clewdr-hub 文档站

[![Built with Starlight](https://astro.badg.es/v2/built-with-starlight/tiny.svg)](https://starlight.astro.build)

[gproxy.leenhawk.com](https://gproxy.leenhawk.com/) 风格的 [Astro Starlight](https://starlight.astro.build) 文档站，源码即 `website/`，发布到 GitHub Pages：<https://waylon256yhw.github.io/clewdr-hub/>。

用户文档请直接看上面的站点；本 README 只讲怎么改文档站本身。

## 本地开发

```bash
cd website
bun install
bun dev        # http://localhost:4321/clewdr-hub/
```

内容是 `src/content/docs/` 下的 `.md` / `.mdx`，文件名即路由。侧边栏分组在 `astro.config.mjs`，组内顺序由各页 frontmatter 的 `sidebar.order` 控制。

## 常用命令

| 命令 | 作用 |
| :-- | :-- |
| `bun install` | 安装依赖 |
| `bun dev` | 本地开发服务器（含搜索、热更新） |
| `bun build` | 构建到 `./dist/`（同时生成 Pagefind 搜索索引） |
| `bun preview` | 本地预览构建产物 |

## 部署

推送到 `master` 且改动了 `website/` 时，`.github/workflows/docs.yml` 会自动构建并发布到 GitHub Pages。

> 站点用了 `base: '/clewdr-hub/'`（GitHub Pages 项目站点）。若改用自定义域名，把 `astro.config.mjs` 里的 `site` 换成域名、`base` 改回 `'/'`。内部链接一律用相对路径，切换 base 不会断。
