# Chrome DevTools MCP — 网页浏览突破限制指令

基于 `chrome-devtools-mcp` 工具集，用于绕过知乎、CSDN 等网站的登录墙、复制限制、付费遮罩。

---

## 一、浏览网页标准流程

```
步骤1: new_page(url)          → 打开页面
步骤2: wait_for(["关键词"])    → 等待内容加载
步骤3: take_snapshot()        → 获取无障碍树（文本结构）
步骤4: take_screenshot()      → 截图确认视觉效果（可选）
步骤5: evaluate_script()      → 提取特定数据
```

## 三、突破限制指令

### 0. 标准检测-移除-提取三段式

```javascript
// Step 1: 检测
evaluate_script(() => {
  JSON.stringify({
    hasMask: !!document.querySelector('[class*="mask"], [class*="overlay"], [class*="passport"]'),
    hasReadMore: !!document.querySelector('.btn-readmore, [class*="readmore"], [class*="expand"]'),
    articleLen: document.querySelector('article')?.innerText.length || 0,
    title: document.title
  })
})

// Step 2: 移除遮罩
evaluate_script(() => {
  document.querySelectorAll('[class*="mask"], [class*="overlay"], [class*="passport"], [class*="login"], [class*="modal"], .hide-article-box')
    .forEach(el => el.remove());
  document.body.style.overflow = 'auto';
  document.body.style.position = '';
  const a = document.querySelector('article');
  if (a) { a.style.height = 'auto'; a.style.maxHeight = 'none'; }
})

// Step 3: 提取正文
evaluate_script(() => {
  const a = document.querySelector('article') || document.querySelector('[class*="content"]') || document.querySelector('[class*="article"]');
  return a?.innerText || 'not found';
})
```

### 1. 绕过登录墙 / 付费遮罩

```javascript
// 移除遮罩层
evaluate_script(() => {
  document.querySelectorAll('.login-guard, .pay-wall, .modal-mask, [class*="mask"], [class*="overlay"]')
    .forEach(el => el.remove());
})
```

```javascript
// 移除 body 滚动锁定并显示内容
evaluate_script(() => {
  document.body.style.overflow = 'auto';
  document.querySelectorAll('.login-guard, .pay-wall, .sign-in, .modal, .overlay')
    .forEach(el => el.remove());
  // 恢复被隐藏的内容
  document.querySelectorAll('[class*="content"], [class*="article"], [class*="main"]')
    .forEach(el => el.style.display = 'block');
})
```

### 2. 解除复制限制

```javascript
// 解除选中/复制限制
evaluate_script(() => {
  document.addEventListener('copy', e => e.stopPropagation(), true);
  document.addEventListener('selectstart', e => e.stopPropagation(), true);
  document.body.style.userSelect = 'auto';
  // 移除 -webkit-user-select: none
  document.querySelectorAll('*').forEach(el => el.style.userSelect = 'auto');
})
```

### 3. 提取被截断的全文

```javascript
// 标准流程：检测 → 移除遮罩 → 提取正文
evaluate_script(() => {
  // 1. 检测是否有遮罩
  const hasMask = !!document.querySelector('[class*="mask"], [class*="overlay"], [class*="passport"]');
  // 2. 检测是否有展开按钮
  const hasReadMore = !!document.querySelector('.btn-readmore, [class*="readmore"], [class*="expand"]');
  return JSON.stringify({hasMask, hasReadMore, articleLen: document.querySelector('article')?.innerText.length || 0});
})

// 如果有展开按钮，先点击展开
evaluate_script(() => {
  const btn = [...document.querySelectorAll('button, a, span, div')]
    .find(el => el.textContent.includes('展开阅读全文') || el.textContent.includes('全文'));
  btn?.click();
})
```

```javascript
// 知乎 — 展开全文
evaluate_script(() => {
  const btn = [...document.querySelectorAll('button, a, span')]
    .find(el => el.textContent.includes('展开阅读全文') || el.textContent.includes('全文'));
  if (btn) btn.click();
})
```

```javascript
// CSDN — 移除登录遮罩 + 提取全文（实战验证 2026）
evaluate_script(() => {
  // 移除所有遮罩/登录元素
  document.querySelectorAll('.mask, .mask-dark, .passport-login-tip-container, .passport-login-container, .passport-login-box, .passport-login-mark, .hide-article-box')
    .forEach(el => el.remove());
  document.body.style.overflow = 'auto';
  document.body.style.position = '';
  // 恢复文章高度
  const article = document.querySelector('article') || document.querySelector('.article_content');
  if (article) {
    article.style.setProperty('height', 'auto', 'important');
    article.style.setProperty('max-height', 'none', 'important');
  }
})

// 提取正文
evaluate_script(() => {
  const art = document.querySelector('article') || document.querySelector('.article_content') || document.querySelector('#article_content');
  return '标题: ' + document.title + '\n\n' + art.innerText;
})
```

### 4. 提取页面文本

```javascript
// 获取正文纯文本
evaluate_script(() => {
  const article = document.querySelector('article') ||
    document.querySelector('[class*="content"]') ||
    document.querySelector('[class*="article"]') ||
    document.querySelector('main');
  return article ? article.innerText : document.body.innerText;
})
```

```javascript
// 获取页面所有文本（保留结构）
evaluate_script(() => {
  return [...document.querySelectorAll('h1, h2, h3, p, li, pre, code')]
    .map(el => el.tagName + ': ' + el.innerText.trim())
    .filter(s => s.length > 3)
    .join('\n---\n');
})
```

### 5. 知乎专用

```javascript
// 知乎 — 跳过登录弹窗 + 显示回答全文
evaluate_script(() => {
  // 关闭弹窗
  document.querySelector('.Modal-closeButton, button[class*="close"]')?.click();
  document.querySelector('[class*="signIn"], [class*="Modal"]')?.remove();
  // 展开所有折叠的回答
  document.querySelectorAll('.RichContent.is-collapsed').forEach(el => {
    el.classList.remove('is-collapsed');
    el.style.height = 'auto';
    el.style.maxHeight = 'none';
    el.style.overflow = 'visible';
  });
  document.body.style.overflow = 'auto';
})
```

### 6. 微信公众号文章（搜狗直通车）

微信公众号文章普通浏览器打不开（需要登录），但搜狗微信搜索是官方的内容索引入口，可以直接访问。

```javascript
// Step 1: 搜索公众号文章
navigate_page('https://weixin.sogou.com/weixin?type=2&s_from=input&query=' + encodeURIComponent('搜索关键词'))

// Step 2: 获取结果列表
evaluate_script(() => {
  const items = [...document.querySelectorAll('.news-list2 .wx-rb, .news-list2 li')].filter(el => el.querySelector('h3 a'));
  return items.slice(0, 10).map(el => ({
    title: el.querySelector('h3 a')?.textContent?.trim(),
    link: el.querySelector('h3 a')?.href,
    source: el.querySelector('.account')?.textContent?.trim(),
    date: el.querySelector('.time')?.textContent?.trim(),
    summary: el.querySelector('.txt-info')?.textContent?.trim()?.slice(0, 80)
  }));
})

// Step 3: 点开原文阅读全文（无需登录）
navigate_page('结果中的link')

// Step 4: 提取正文
evaluate_script(() => document.body.innerText)
```

**实测验证（2026）：** 搜狗微信搜索 `chrome devtools` 返回 634 条结果，点开原文直接阅读 2856 字全文，无任何限制。

### 7. 模拟移动端（部分站点移动端限制更少）

```javascript
// 模拟手机用户代理 + 移动端视口
emulate({
  userAgent: 'Mozilla/5.0 (iPhone; CPU iPhone OS 16_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/16.0 Mobile/15E148 Safari/604.1',
  viewport: '375x667x2,mobile,touch'
})
```

## 四、常用指令速查

| 操作 | 工具 | 说明 |
|------|------|------|
| 打开页面 | `new_page(url)` | 新标签页打开 |
| 导航 | `navigate_page(url)` | 当前页跳转 |
| 等待内容 | `wait_for(["文本"])` | 等待文本出现 |
| 截图 | `take_screenshot()` | 全屏截图 |
| DOM快照 | `take_snapshot()` | 无障碍树文本结构 |
| 执行JS | `evaluate_script(fn)` | 任意JS操作 |
| 带带参数JS | `evaluate_script(fn, args)` | 传参执行 |
| 提取内容 | `evaluate_script(() => document.body.innerText)` | 纯文本提取 |
| 移除元素 | `evaluate_script(() => el.remove())` | 移除遮罩弹窗 |
| 点击元素 | `click(uid)` | 根据快照uid点击 |
| 模拟设备 | `emulate({userAgent, viewport})` | 切换UA/视口 |
| 滚动 | `press_key({key: "Space"})` | 模拟按键 |

## 五、FAQ（实战经验）

### 1. 弹窗类名不匹配怎么办？
先查看实际遮罩的元素：
```javascript
evaluate_script(() => {
  // 打印所有可能遮罩的元素
  [...document.querySelectorAll('div[style*="fixed"], div[style*="absolute"], [class*="overlay"], [class*="modal"], [class*="mask"], [class*="popup"]')]
    .map(el => ({tag: el.tagName, cls: el.className.slice(0,80), visible: el.offsetParent !== null}))
})
```

### 2. 如何判断内容是完整的还是被截断了？
```javascript
evaluate_script(() => {
  const a = document.querySelector('article') || document.querySelector('.Post-RichText');
  const ratio = a.scrollHeight / a.clientHeight;
  JSON.stringify({
    textLen: a.innerText.length,
    scrollH: a.scrollHeight, clientH: a.clientHeight,
    ratio: ratio.toFixed(2), // > 1.2 说明有溢出被隐藏
    endText: a.innerText.slice(-100)
  })
})
```
如果结尾有 `-- The End --`、版权声明或明显是自然结尾，则完整。

### 3. CSDN 实测遮罩类名（2026）
| CSDN 类名 | 说明 |
|-----------|------|
| `.mask` + `.mask-dark` | 背景遮罩 |
| `.passport-login-tip-container` | 登录提示条 |
| `.passport-login-container` | 登录弹窗容器 |
| `.passport-login-box` / `.passport-login-mark` | 登录框和遮罩 |
| `.hide-article-box` | 文章折叠条 |

### 4. 知乎实测遮罩类名（2026）
| 知乎类名 | 说明 |
|---------|------|
| `.Modal.Modal--default.signFlowModal` | 登录弹窗 |
| `.signFlowModal-container` | 登录容器 |
| 正文选择器: `.Post-RichText` 或 `.RichText` | |

### 5. 文章实际很短 vs 被截断
- 有些文章本身就短（配图多、代码多但文字少），例如 2081 字但 scrollHeight = 8550px
- 验证方法：看结尾是否有自然结束标记，或查 `document.title` 确认标题
- 知乎专栏若无登录态可能直接重定向到搜索页，此时检查 `location.href`**

### 6. 能破 vs 不能破的情况

| 类型 | 原理 | 能否突破 | 示例 |
|------|------|----------|------|
| DOM遮罩型 | 内容在DOM里，只是上面盖了一层div | ✅ 移除即可 | CSDN、知乎专栏 |
| 懒加载型 | 内容在DOM外，滚动后加载 | ✅ 滚动触发即可 | 大部分评论区 |
| API鉴权型 | 内容靠登录cookie获取API | ❌ 无cookie拿不到 | B站评论、微博 |
| SSR有隐藏型 | 服务端渲染但加class隐藏 | ✅ 改样式即可 | 掘金付费文章 |

### 7. Chrome 重启 / 掉线处理

MCP 模式会自动管理浏览器生命周期，CLI 模式用以下命令：
```bash
chrome-devtools stop     # 停止后台
chrome-devtools status   # 检查状态
```
