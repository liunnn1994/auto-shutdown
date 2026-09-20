/*
 * Vite 配置：把管理界面构建成"单文件 HTML"（CSS/JS 全部内联），再按
 * `<!-- @段落名 -->` 标记切成片段，生成固件引用的 ../htmx_min.h。
 *
 * 段落约定（index.html 中的注释标记，勿删）：
 *   @head  <!doctype> 到页面骨架容器（内联后的 <style>/<script> 都在这段）
 *   @login 登录页正文
 *   @main  登录后主界面正文
 *   @tail  页脚与收尾标签
 * 固件按 PAGE_HEAD +（PAGE_LOGIN 或 PAGE_MAIN）+ PAGE_TAIL 拼接返回。
 */

import { defineConfig } from "vite";
import tailwindcss from "@tailwindcss/vite";
import minifyHtmlPkg from "@minify-html/node";
import fs from "node:fs";
import path from "node:path";

const minifyHtml = minifyHtmlPkg.default ?? minifyHtmlPkg; // CJS 包：函数挂在 default 上

const ROOT = import.meta.dirname;
const DIST = path.join(ROOT, "dist");
const OUT = path.join(ROOT, "..", "htmx_min.h");

const REQUIRED = ["head", "login", "main", "tail"];
const DELIM = ')html"'; // C 原始字符串分隔符，片段内容中出现会导致提前结束

// HTML 压缩：源码（index.html）可以用 prettier 随意格式化保证可读性，
// 进入固件前把无意义的空白/换行/注释全部去掉，节省 PROGMEM。
// 说明：Vite/oxc 的 minify 只覆盖 JS 与 CSS，HTML 标记的压缩由这里完成；
// 内联的 <script>/<style> 已由 Vite 压缩，故 minify_js/minify_css 关闭。
const HTML_MINIFY_CFG = {
  keep_comments: true, // 分片标记 <!-- @段落 --> 靠注释实现，压缩后仍需保留
  keep_closing_tags: true, // @tail 段含 </body></html> 等可选闭合标签，确保保留
  keep_html_and_head_opening_tags: true, // 片段里 <html>/<head>/<body> 必须原样保留
  do_not_minify_doctype: true,
  minify_js: false,
  minify_css: false,
};

// 按标记切分 HTML，返回 { 段名: 内容 }
function splitSections(html) {
  const marker = /<!--\s*@(\w+)\s*-->/g;
  const sections = {};
  let last_name = null;
  let last_pos = -1;
  for (const m of html.matchAll(marker)) {
    if (last_name !== null) sections[last_name] = html.slice(last_pos, m.index);
    last_name = m[1];
    last_pos = m.index + m[0].length;
  }
  if (last_name !== null) sections[last_name] = html.slice(last_pos);
  return sections;
}

// 生成 htmx_min.h 的自定义插件：在构建收尾时把 dist 内联成单文件并切分
function fwHeaderPlugin() {
  return {
    name: "fw-progmem-header",
    apply: "build",
    async closeBundle() {
      let html = fs.readFileSync(path.join(DIST, "index.html"), "utf8");

      // ---- 单文件化：把构建出的本地 CSS / JS 内联回 HTML ----
      html = html.replace(/<link[^>]*rel="stylesheet"[^>]*>/g, (tag) => {
        const href = tag.match(/href="([^"]+)"/)?.[1];
        if (!href || !href.startsWith("/assets/")) return tag;
        const css = fs.readFileSync(path.join(DIST, href.slice(1)), "utf8");
        return `<style>${css}</style>`;
      });
      html = html.replace(/<link[^>]*rel="modulepreload"[^>]*>/g, "");
      html = html.replace(/<script[^>]*type="module"[^>]*src="([^"]+)"[^>]*>\s*<\/script>/g, (tag, src) => {
        const file = path.join(DIST, src.replace(/^\//, "").split("?")[0]);
        const js = fs.readFileSync(file, "utf8");
        return `<script type="module">${js}</script>`;
      });

      // ---- 压缩整页（HTML 标记级）----
      // 在整份文档上压缩（而非逐段），保证解析器看到的 DOM 完整、
      // @tail 段的 </body></html> 等闭合标签不会被误删
      html = minifyHtml
        .minify(Buffer.from(html), HTML_MINIFY_CFG)
        .toString();

      // ---- 按标记切段并生成 PROGMEM 头文件 ----
      const sections = splitSections(html);
      const missing = REQUIRED.filter((r) => !sections[r]);
      if (missing.length) {
        throw new Error(`构建产物缺少段落标记 @${missing.join("、@")}——请勿删除 index.html 中的 <!-- @段落 --> 注释`);
      }

      const out = [
        "/* 自动生成：由 web/vite.config.mjs 在 Vite 构建收尾时生成",
        " * （web/index.html 经 Vite 构建并压缩内联后的单文件片段），请勿手改。 */",
        "#pragma once",
        "#include <pgmspace.h>",
        "",
      ];
      let total = 0;
      for (const name of REQUIRED) {
        const chunk = sections[name];
        if (chunk.includes(DELIM)) {
          throw new Error(`段落 @${name} 中包含原始字符串分隔符 ${DELIM}，无法安全嵌入`);
        }
        total += chunk.length;
        out.push(`// 段落 @${name}（压缩后的单文件 HTML 片段，${chunk.length}B）`);
        out.push(`static const char PAGE_${name.toUpperCase()}[] PROGMEM = R"html(${chunk})html";\n`);
      }
      fs.writeFileSync(OUT, out.join("\n"), "utf8");

      // 产物只有 htmx_min.h，清理 dist
      fs.rmSync(DIST, { recursive: true, force: true });
      console.log(`已生成 ${OUT}（单文件 HTML 共 ${html.length}B，片段合计 ${total}B）`);
    },
  };
}

// 开发预览辅助：pnpm dev 没有固件后端。若不加拦截，Vite 的 SPA fallback 会把
// /status、/scan 等 htmx 轮询请求也回以整页 index.html——页面里又带轮询属性，
// htmx 换入后继续轮询，DOM 会指数级嵌套增长。这里对接口类路径直接返回 404。
function devNoBackendPlugin() {
  return {
    name: "dev-no-backend",
    configureServer(server) {
      server.middlewares.use((req, res, next) => {
        const url = (req.url || "/").split("?")[0];
        const ok =
          req.method === "GET" &&
          (url === "/" ||
            url.startsWith("/@") || // Vite 内部模块（HMR 客户端等）
            url.startsWith("/src/") ||
            url.startsWith("/node_modules/") ||
            path.extname(url) !== ""); // 静态资源
        if (ok) return next();
        res.statusCode = 404;
        res.setHeader("Content-Type", "text/plain; charset=utf-8");
        res.end(`404：dev 预览无固件后端（${url} 不可用），请烧录固件后在设备上使用`);
      });
    },
  };
}

export default defineConfig({
  plugins: [devNoBackendPlugin(), tailwindcss(), fwHeaderPlugin()],
  build: {
    // 注意：不再设 es2018 目标——htmx 4 与 Tailwind v4 的 CSS 本身就要求
    // 2022 年后的内核（oklch/@layer），语法转译只增加体积不增加兼容性。
    // 老环境的真正兼容点已单独处理：
    //   1. htmx 指示器样式改为自提供（见 index.html 的 htmx-config meta）
    //   2. main.js 未用新 API；<dialog> 有 open 属性回退
    // 无动态加载需求，禁用 preload polyfill 之类的额外注入
    modulePreload: { polyfill: false },
  },
});
