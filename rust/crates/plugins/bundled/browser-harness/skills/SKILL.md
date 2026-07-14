---
name: browser-harness
description: Use when automating browser interactions (open pages, click, type, screenshot), extracting content from anti-scraping sites (Cloudflare, bot detection), or using remote cloud browsers.
---

# Browser Harness — 浏览器自动化交互技能

基于 `browser-harness` CLI 工具的操作指南，涵盖反爬内容提取、网页浏览、截图、点击、表单填写、网络抓取、远程云浏览器。

> `browser-harness` 已安装在 PATH 中（`C:\Users\Incredible\.local\bin\browser-harness.exe`），直接使用即可，无需检查安装状态。

## Included Files

| File | Purpose |
|------|--------|
| `browser-harness-use.md` | 完整交互速查 — readwebfetch反爬提取、打开网页、截图、点击坐标、填写表单、JS执行、弹窗处理、多标签管理、网络抓取、云浏览器 |

## When to Use

Use this skill when **any** of the following apply:
1. **Content extraction from anti-scraping sites** — CSDN, Cloudflare, JS challenge, bot detection → **`readwebfetch(url)` 一行搞定**
2. **Browser automation** — need to programmatically control a browser (open pages, click, type, screenshot)
3. **UI testing / interaction** — need to fill forms, click buttons, handle dialogs via coordinates
4. **Remote cloud browsers** — need concurrent or persistent browser sessions
5. **Network monitoring** — need to capture network requests made by page

## Quick Start — readwebfetch 反爬提取

**从反爬网站（CSDN、Cloudflare 等）提取文章正文，一行命令：**

```python
result = readwebfetch('https://blog.csdn.net/user/article/details/12345')
print(result['title'])     # 文章标题
print(result['text'])      # 完整正文（含代码块）
print(result['excerpt'])   # 摘要
print(result['byline'])    # 作者
```

**返回字段：** `url`, `title`, `text`（全文）, `excerpt`（摘要）, `byline`（作者）

**优势：** 自动绕过 Cloudflare、CSDN、博客园等反爬机制，直接提取干净的文章内容。

## Core Workflow — 浏览器自动化

```python
new_tab("https://example.com")     # 打开页面
wait_for_load()                    # 等待加载
capture_screenshot()               # 截图确认状态
click_at_xy(450, 320)              # 坐标点击
type_text("hello")                 # 输入文字
print(js("document.body.innerText")) # 提取内容
```

## Key Capabilities

- **readwebfetch(url)**: Extract clean article text from anti-scraping sites (CSDN, Cloudflare, etc.) — **首选内容提取方式**
- **new_tab / goto_url**: Open and navigate pages
- **capture_screenshot**: Viewport or full-page screenshots
- **click_at_xy**: Coordinate-based clicking (bypasses iframe/Shadow DOM issues)
- **type_text / press_key**: Keyboard input
- **js()**: Execute arbitrary JavaScript in page context
- **cdp()**: Direct Chrome DevTools Protocol access
- **NetworkMonitor**: Capture HTTP requests
- **start_remote_daemon**: Cloud browser for concurrent tasks
- **PDF export, multi-tab management, alert handling**

## Windows PowerShell Notes

- Use double quotes `"..."` for `-c` argument, single quotes `'...'` inside Python
- Prefer `querySelector('#id')` over `getElementById("id")` to avoid quote nesting
- Use `JSON.stringify(...)` for safe data transfer from js()
- For complex scripts, write `.py` file and use `Get-Content` to pipe

See `browser-harness-use.md` for complete API reference and troubleshooting.
