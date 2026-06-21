# browser-harness 交互技能速查

## 怎么用

```bash
browser-harness -c '你的Python代码写在这里'    # macOS / Linux
browser-harness -c "你的Python代码写在这里"    # Windows PowerShell
```

PS C:\Users\Incredible\Desktop> browser-harness -c "new_tab('https://www.sogou.com'); wait_for_load(); print(page_info())"
{'url': 'https://www.sogou.com/', 'title': '🐴 搜狗搜索引擎 - 上网从搜狗开始', 'w': 1041, 'h': 527, 'sx': 0, 'sy': 0, 'pw': 1026, 'ph': 660}
PS C:\Users\Incredible\Desktop>

**关键区别：**
- **macOS/Linux:** 外壳单引号 `'...'`，里面 Python 用双引号 `"..."`  
  `browser-harness -c 'new_tab("https://example.com")'`
- **Windows PowerShell:** 外壳双引号 `"..."`，里面 Python 用单引号 `'...'`  
  `browser-harness -c "new_tab('https://example.com')"`

> ⚠️ **首次打开页面必须用 `new_tab(url)`，不能 `goto_url(url)`**  
> `goto_url` 在当前标签导航，如果当前是 `chrome://` 页面会出错

```powershell
# Windows 示例：打开搜狗
browser-harness -c "new_tab('https://www.sogou.com'); wait_for_load(); print(page_info())"
```

### Windows PowerShell 引用大坑 ⚠️

**问题：** `js("...")` 里面的双引号会被 PowerShell 吃掉，导致 JS 报错。

```powershell
# ❌ 报错：PowerShell 吃掉 \"，导致 JS 语法错误
browser-harness -c "new_tab('url'); print(js('document.getElementById(\"id\").textContent'))"

# ✅ 正确：JS 里用单引号
browser-harness -c "new_tab('url'); print(js('document.getElementById('"'"'id'"'"').textContent'))"  # 过于复杂，不推荐

# ✅ 推荐方案一：用 querySelector（CSS 选择器，不用引号）
browser-harness -c "new_tab('url'); print(js('document.querySelector('#id').textContent'))"

# ✅ 推荐方案二：如果页面有全局变量/ID，直接引用
browser-harness -c "new_tab('url'); print(js('stepDisp.textContent'))"

# ✅ 推荐方案三：复杂脚本写 .py 文件，用 Get-Content 传入
browser-harness -c "$(Get-Content script.py -Raw)"
```

**`-c` 三种方案的取舍：**

| 方案 | 适用场景 | 优点 | 缺点 |
|------|----------|------|------|
| 直接写 `-c "..."` | 简单操作（打开、截图、获取信息） | 一行搞定 | js() 带字符串时引用地狱 |
| 写 `.py` 文件 + `Get-Content` | 复杂多步逻辑 | 无引用问题，可写多行 | 多一个文件 |
| 短 `-c` 分多次执行 | 状态不依赖的独立操作 | 各步独立、易调试 | 每次重开页面 |

**.py 文件最佳实践：**
```python
# script.py — 用单引号包裹 Python 字符串
new_tab('file:///C:/path/to/page.html')
wait_for_load()
print(page_info())
print('Steps:', js('stepDisp.textContent'))        # 引用全局变量
print('Boxes:', js('JSON.stringify(state.boxLocs)'))  # JSON.stringify 返回字符串无需引号
capture_screenshot('C:/path/to/screenshot.png')
js('move(0, -1)')
print('Player:', js('JSON.stringify(state.player)'))
```

然后运行：
```powershell
browser-harness -c "$(Get-Content script.py -Raw)"
```

> 💡 **经验：** `js('JSON.stringify(...)')` 是最安全的传值方式——返回字符串，不需嵌套引号。

---

## ⭐ readwebfetch — 反爬内容提取（首选）

**从 CSDN、Cloudflare、博客园等反爬网站提取干净的文章正文，一行命令搞定：**

```python
result = readwebfetch('https://blog.csdn.net/user/article/details/12345')
print(result['title'])     # 文章标题
print(result['text'])      # 完整正文（含代码块，已去除广告/导航等干扰）
print(result['excerpt'])   # 摘要
print(result['byline'])    # 作者
```

**实测 CSDN 博客：** 提取 12,000+ 字符，包含完整代码块、标签、元数据。

**返回字段：**

| 字段 | 说明 |
|------|------|
| `url` | 最终解析的 URL |
| `title` | 文章标题 |
| `text` | 完整正文（最常用） |
| `excerpt` | 摘要 |
| `byline` | 作者信息 |

**使用场景：**
1. **读取反爬网站内容** — CSDN、博客园、掘金等，无需登录
2. **替代 WebFetch** — 当 WebFetch 被反爬拦截时，用 readwebfetch 绕过
3. **批量内容提取** — 配合多标签管理批量抓取文章
4. **结合 JS 分析** — 先用 readwebfetch 提取正文，再用 js() 操作页面

**注意：** readwebfetch 直接发起请求并解析，无需先 new_tab 打开页面。

---

## 一、打开网页

```python
new_tab("https://news.ycombinator.com")   # 新标签页打开
wait_for_load()                            # 等页面加载完
print(page_info())                         # 打印页面信息
```

**效果：** 浏览器打开一个新标签，加载 Hacker News，打印当前页面的标题、URL、视口尺寸。

```python
goto_url("https://example.com/page2")     # 在当前标签导航
```

> 首次打开用 `new_tab`，后续跳转用 `goto_url`（不会新建标签）

---

## 二、截图

```python
capture_screenshot()                       # 截当前视口，自动发给 AI
capture_screenshot("/tmp/shot.png")        # 保存到文件
capture_screenshot(max_dim=1800)           # 限制尺寸，防止模型拒收
capture_screenshot(full=True)              # 截整个页面（含折叠部分）
```

**效果：** 截一张图，AI 可以"看到"页面长什么样。这是最重要的操作——先截图，再决策。

> 注意：截图是设备像素，点击坐标是 CSS 像素。2× 屏下先 `js("window.devicePixelRatio")` 换算再点。

---

## 三、点击

```python
# 1. 先截图，看目标在哪
capture_screenshot()

# 2. 算坐标，点下去
click_at_xy(450, 320)                      # 点击 (450, 320) 位置

# 3. 再截图，确认生效
capture_screenshot()
```

**效果：** 第一次截图看到按钮位置 → 鼠标点在按钮上 → 第二次截图验证页面变化了。

> 坐标点击穿透 iframe、Shadow DOM、跨域，比 CSS 选择器靠谱。只有对隐藏元素（0×0 节点）才用 DOM 操作。

---

## 四、填写表单

```python
# 先点进输入框
click_at_xy(300, 400)
# 再打字
type_text("hello world")
# 提交
press_key("Enter")
```

**效果：** 鼠标点进搜索框 → 输入"hello world" → 按回车搜索。

```python
# 或者直接用 JS 填
js("document.querySelector('input').value = 'hello'")
```

---

## 五、获取页面文字

```python
print(page_info())                         # 标题 + URL + 视口
print(js("document.body.innerText"))       # 页面全部文本
print(js("document.title"))                # 页面标题
```

**效果：** 直接拿到页面内容，不用截图也能知道页面上有什么。

---

## 六、执行任意 JavaScript

```python
# 获取数据
data = js("""
  JSON.stringify({
    title: document.title,
    links: [...document.querySelectorAll('a')].map(a => a.href)
  })
""")

# 修改页面
js("document.querySelector('.ad-banner')?.remove()")
js("document.body.style.background = 'white'")

# 操作 API
result = js("""
  (async () => {
    const r = await fetch('/api/data');
    return r.json();
  })()
""")
```

**效果：** 在页面里跑 JS，可以读数据、改样式、调接口，和在 DevTools Console 里一样。

### js() 在 Windows 下的引用技巧

PowerShell 下 `js()` 的参数如果有双引号会非常麻烦，**优先用这些写法绕过：**

```python
# 方案 A：CSS 选择器（单引号友好）
js("document.querySelector('#stepDisplay').textContent")

# 方案 B：直接引用页面的全局变量（最干净）
# 查看页面 JS 代码，找到暴露的全局变量
js("stepDisp.textContent")         # 如果页面有 const stepDisp = ...
js("levelDisp.textContent")        # 同理
js("won")                          # 布尔值
js("state.player")                 # 对象
js("state.boxLocs")                # 数组

# 方案 C：JSON.stringify 序列化复杂数据
js("JSON.stringify(state.player)")   # 返回 JSON 字符串，无引号问题
js("JSON.stringify(state.boxLocs)")

# 方案 D：template literal（反引号，不需要引号）
js("`Steps: ${stepDisp.textContent}`")

# 方案 E：实在需要字符串参数时，用 .py 文件（见上文）
```

---

## 七、弹窗处理

```python
# 场景：点击按钮后弹出 alert
click_at_xy(200, 300)
# 弹窗出现了，JS 被冻结
cdp("Page.handleJavaScriptDialog", accept=True)   # 点"确定"
```

**效果：** 遇到 `alert()` / `confirm()` / `beforeunload` 弹窗时，从 CDP 层面直接关掉，用户看不到弹窗，反爬也检测不到。

如果想拦截所有弹窗不让他们弹出来：
```python
js("""
window.alert=m=>{};           # alert 变静默
window.confirm=m=>true;       # confirm 自动返回"确定"
window.onbeforeunload=null;   # 关掉"确认离开"提示
""")
```

---

## 八、多标签管理

```python
# 场景：在多个页面间切换
tab1 = new_tab("https://a.com")            # 打开第一个
tab2 = new_tab("https://b.com")            # 打开第二个
switch_tab(tab1)                            # 切回第一个
cdp("Target.activateTarget", targetId=tab1) # 显示到前台（可选）

# 列出所有标签
for t in list_tabs():
    print(t["url"][:60])
```

---

## 九、等待页面加载

```python
wait_for_load()                            # 等页面加载完成
wait_for_text("登录")                       # 等"登录"文字出现（最多 10 秒）
```

---

## 十、网络请求抓取

```python
# 场景：提交表单后想确认后端收到了
from browser_harness.helpers import NetworkMonitor
monitor = NetworkMonitor()

fill_form({"name": "张三", "email": "a@b.com"})
click_at_xy(500, 600)

requests = monitor.get_requests()          # 拿到刚发出的网络请求
```

---

## 十一、滚动

```python
# 场景：页面很长，需要滚到底加载更多
js("window.scrollTo(0, document.body.scrollHeight)")
wait_for_load()
capture_screenshot()                       # 确认新内容出现了
```

---

## 十二、PDF 导出

```python
# 场景：把当前页面存为 PDF
cdp("Page.printToPDF", landscape=False, printBackground=True)
```

---

## 十三、键盘操作

```python
press_key("Enter")                         # 回车
press_key("Tab")                           # 跳格
press_key("Escape")                        # 取消
type_text("搜索关键词")                    # 连续打字
```

---

## 十四、调试技巧

```python
# 卡住了不知道什么状态
print(page_info())                         # 看标题/URL/视口
print(current_tab())                       # 看当前附加到哪个标签
tabs = list_tabs()                         # 看所有标签页
ensure_real_tab()                          # 修复附加到假标签的问题
```

**常见问题速查：**

| 现象 | 原因 | 解法 |
|------|------|------|
| 截图空白 | 附加到了 omnibox 假标签 | `ensure_real_tab()` |
| 点了没反应 | 坐标不对/没点到 | 重新截图算坐标，或用 `js` 操作 |
| 页面不动了 | 有弹窗冻结了 JS | `cdp("Page.handleJavaScriptDialog", accept=True)` |
| 点了链接没跳 | 有 `beforeunload` | `cdp("Page.handleJavaScriptDialog", accept=True)` |
| 获取不到数据 | 需要登录 | 先让用户登录，或 `sync_local_profile` |
| `js()` 报错 SyntaxError | PowerShell 吃了双引号 | 用 `querySelector` / 全局变量 / `.py` 文件方案 |
| `page_info()` 标题有 🐴 | browser-harness 自动注入，正常现象 | 忽略即可 |
| 连续移动/操作不生效 | 可能被墙/箱子挡住了 | `print(js('JSON.stringify(state)'))` 检查状态 |
| 步骤 `steps--` 变负数 | 不会发生，`undo()` 有 `history.length` 保护 | 但注意 `undo` 不会触发 win 状态重置 |

---

## 十五、远程云浏览器

仅用于 **Browser Use Cloud**，适合并发子任务或免维护运行。

```python
start_remote_daemon("work")                # 启动一台云浏览器
start_remote_daemon("work", proxyCountryCode=None)  # 关闭代理
```

```bash
BU_NAME=work browser-harness -c '
new_tab("https://example.com")
print(page_info())
'
```

```python
stop_remote_daemon("work")                 # 停止，停止计费
```

带登录态启动：
```python
list_cloud_profiles()                      # 看云端有哪些已存配置
sync_local_profile("我的Chrome配置")       # 上传本地 cookie
start_remote_daemon("work", profileName="我的Chrome配置")
```
