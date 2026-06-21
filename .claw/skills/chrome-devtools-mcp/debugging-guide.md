# Chrome DevTools MCP — 网页调试说明书

基于 `chrome-devtools-mcp` 工具集，用于调试网页、检查错误、分析性能。

---

## 一、工具总览

```
类别              工具                             用途
───              ───                              ───
导航             new_page / navigate_page         打开/跳转页面
                 close_page / select_page         关闭/切换标签页
                 list_pages                       列出所有标签页
                 wait_for                         等待文本出现

调试             evaluate_script                  在页面执行JS
                 take_snapshot                    获取无障碍树（元素uid）
                 take_screenshot                  截图
                 list_console_messages            列出控制台日志
                 get_console_message(msgid)       查看特定日志详情
                 lighthouse_audit                 Lighthouse审计

交互             click(uid)                       点击元素
                 fill(uid, value)                 填写输入框
                 fill_form([{uid,value}])         批量填表
                 type_text(text)                  键盘输入
                 press_key(key)                   按键（Enter/Tab/Ctrl+A）
                 hover(uid)                       悬停
                 drag(from_uid, to_uid)           拖拽
                 handle_dialog(action)            处理浏览器弹窗
                 upload_file(path, uid)           上传文件

网络             list_network_requests            列出网络请求
                 get_network_request(reqid)       查看请求详情/响应体

性能             performance_start_trace          开始性能录制
                 performance_stop_trace           停止 + 分析
                 performance_analyze_insight      分析特定指标
                 take_memory_snapshot             内存快照(heap)

仿真             emulate({userAgent, viewport})   模拟设备
                 resize_page(width, height)       调整窗口
```

---

## 二、调试标准流程

### 流程1：JS错误排查

```
1. navigate_page(url)                      → 进入页面
2. list_console_messages()                 → 查看报错
3. get_console_message(msgid)              → 查看具体错误详情
4. evaluate_script(() => { /* 修复 */ })  → 修复问题
5. 验证
```

### 流程2：网络请求分析

```
1. navigate_page(url)                      → 加载页面
2. list_network_requests()                 → 列出所有请求
3. get_network_request(reqid)              → 查看请求/响应体
4. 定位404、CORS错误、慢请求
```

### 流程3：DOM/样式调试

```
1. take_snapshot()                         → 获取页面元素结构（带uid）
2. click(uid) / fill(uid, value)           → 交互
3. evaluate_script(() => getComputedStyle(el))  → 检查样式
4. evaluate_script(() => { el.style.color = 'red' })  → 临时修改
5. take_screenshot()                       → 截图确认
```

### 流程4：性能分析

```
1. performance_start_trace({reload: true})  → 开始录制+重载
2. （等待页面加载完成）
3. performance_stop_trace()                 → 停止分析
4. performance_analyze_insight({insightName, insightSetId})  → 深入特定指标
```

---

## 三、调试指令速查

### 控制台

```javascript
// 查看所有控制台消息
list_console_messages({includePreservedMessages: true})

// 查看特定消息
get_console_message({msgid: 0})
```

### 元素检查

```javascript
// 获取可交互元素列表（带uid）
take_snapshot()

// 详细版（包含更多属性）
take_snapshot({verbose: true})

// 检查元素样式
evaluate_script(() => {
  const el = document.querySelector('h1');
  return getComputedStyle(el);
})

// 获取元素尺寸/位置
evaluate_script(() => {
  const el = document.querySelector('h1');
  return el.getBoundingClientRect();
})
```

### 页面交互

```javascript
// 点击（先take_snapshot获取uid）
click({uid: "element-123"})

// 填写
fill({uid: "input-456", value: "搜索内容"})

// 填写+回车
fill({uid: "input-456", value: "搜索内容"})
press_key({key: "Enter"})

// 键盘快捷键
press_key({key: "Control+A"})
press_key({key: "Control+C"})

// 处理浏览器弹窗（alert/confirm）
handle_dialog({action: "accept"})
handle_dialog({action: "dismiss"})
```

### 网络

```javascript
// 查看所有网络请求
list_network_requests({pageSize: 50, resourceTypes: ["XHR", "Fetch", "Document"]})

// 查看请求详情
get_network_request({reqid: 0})

// 保存响应体到文件
get_network_request({reqid: 0, responseFilePath: "response.json"})
```

### 内存调试

```javascript
// 捕获堆快照（用于分析内存泄漏）
take_memory_snapshot({filePath: "heap.heapsnapshot"})
```

### Lighthouse 审计

```javascript
// 无障碍 + SEO + 最佳实践
lighthouse_audit({device: "desktop"})
lighthouse_audit({device: "mobile"})
lighthouse_audit({mode: "snapshot"}) // 不重新加载，分析当前状态
```

---

## 四、典型场景

### 场景A：修复页面白屏/JS报错

```
1. list_console_messages()             → 查看是否有 JS 报错
2. get_console_message(0)             → 看第一条错误详情
3. evaluate_script(() => { ... })     → 在页面临时修复测试
4. 在源码中修复后 reload 验证
```

### 场景B：API接口调试

```
1. navigate_page('https://example.com')
2. list_network_requests({resourceTypes: ["XHR", "Fetch"]})  → 只看接口
3. get_network_request(0)  → 查看请求参数 + 响应数据
```

### 场景C：表单提交验证

```
1. take_snapshot()                    → 获取表单元素uid
2. fill({uid, value})                 → 填写各字段
3. click({uid})                       → 点击提交按钮
4. list_network_requests()            → 检查请求是否发出
5. list_console_messages()            → 检查是否有错误
```

### 场景D：响应式布局调试

```
1. emulate({viewport: '375x667x2,mobile,touch'})   → 切换手机
2. take_screenshot()                                 → 截图看效果
3. emulate({viewport: '1280x720'})                   → 切回桌面
4. take_screenshot()                                 → 对比效果
```
