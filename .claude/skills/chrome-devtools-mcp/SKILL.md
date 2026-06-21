---
name: chrome-devtools-mcp
description: Use when browsing web pages, extracting content from restricted sites (login walls, paywalls), debugging JS errors, analyzing network requests, or running performance audits via browser DevTools.
---

# Chrome DevTools MCP — 网页浏览与调试技能

基于 `chrome-devtools-mcp` 工具集的操作指南，涵盖网页浏览、交互调试、内容提取和性能分析。

## Included Files

| File | Purpose |
|------|---------|
| `browsing-guide.md` | 网页浏览突破限制指令 — 绕过登录墙、付费遮罩、复制限制、提取全文 |
| `debugging-guide.md` | 网页调试说明书 — JS错误排查、网络请求分析、DOM/样式调试、性能分析 |

## When to Use

Use this skill when **any** of the following apply:
1. **Browsing** — need to navigate web pages, extract content, bypass login walls/paywalls
2. **Debugging** — need to inspect console errors, network requests, DOM elements, or page performance
3. **Content extraction** — need to extract article text from restricted pages (Zhihu, CSDN, etc.)
4. **Interaction** — need to fill forms, click elements, handle dialogs on web pages
5. **Performance** — need to run Lighthouse audits, trace performance, or capture heap snapshots

## Core Workflow

```
1. new_page(url) / navigate_page(url)   → 打开/导航页面
2. wait_for(["关键词"])                   → 等待内容加载
3. take_snapshot()                       → 获取元素结构（uid）
4. take_screenshot()                     → 截图确认视觉效果
5. evaluate_script(() => ...)            → 执行JS/提取数据
6. list_console_messages()               → 检查控制台错误
```

## Key Capabilities

- **Bypass restrictions**: Remove login/paywall overlays, unlock copy restrictions, expand truncated articles
- **Debug JS errors**: List and inspect console messages, identify uncaught exceptions
- **Network analysis**: List network requests, inspect request/response bodies
- **DOM interaction**: Click, fill, type, hover, drag — all via accessibility tree (uid)
- **Performance**: Lighthouse audits, performance traces, memory heap snapshots
- **Device emulation**: Mobile viewport, user agent switching

## Reference

See `browsing-guide.md` for content extraction patterns and bypass techniques.
See `debugging-guide.md` for debugging workflows and tool references.
