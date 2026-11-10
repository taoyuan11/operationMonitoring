# 共享交互组件

业务页面使用此目录的组件，不直接使用浏览器的 select、日期/颜色选择器、复选框外观、数字步进器、进度条、title 提示或校验气泡。底层保留语义化 button、input、textarea 和 form，以支持键盘、输入法、密码管理器和表单约束。文件窗口由操作系统提供，页面入口使用自定义上传区域。

## 设计约定

- 沿用 `styles/base.css` 的语义颜色变量，自动适配深浅主题与用户强调色。
- 常规表单高度 42px；紧凑工具栏沿用页面的 32–38px 高度。输入框圆角 6px，浮层圆角 8px。
- 悬停使用边框和表面色变化；选中使用强调色；键盘焦点使用明确的轮廓或光环；错误显示字段下方的文字，不能只通过颜色表达。
- 浮层渲染到 body 或当前全屏容器，自动避让视口边缘；Esc 关闭并返回触发控件，点击外部关闭。遵守系统减少动画设置。
- 给控件添加可见 label，独立图标操作添加 aria-label。纯提示通过 `v-tooltip`，不能把关键信息只放在提示内。

## 组件约定

| 组件 | 用法与行为 |
| --- | --- |
| `UiButton` | 默认 `type="button"`，提交操作显式指定 `type="submit"`；沿用 `primary-button`、`text-button` 等业务样式。支持原有 DOM 事件和 `focus()`。 |
| `UiInput` / `UiTextarea` | 字符串 `v-model`、`.trim`，透传 placeholder、required、autocomplete 等原生输入属性。 |
| `UiNumberInput` | 数字 `v-model`，清空为 null；支持 min/max/step。鼠标使用自定义加减按钮，键盘使用方向键；提交时保留范围校验。 |
| `UiCheckbox` | 布尔或字符串数组 `v-model`；数组项使用 value。支持 checked、indeterminate、disabled，change 仍返回 DOM Event。 |
| `UiSelect` | `options: { value: string \| number, label: string, disabled?: boolean }[]`；保留 value 类型；`change` 返回选中值。超过 8 项自动提供搜索，支持方向键、Home/End、Enter、Esc、Tab 与首字母定位。 |
| `UiDateTimePicker` | `v-model` 为本地时间 `YYYY-MM-DDTHH:mm` 或空字符串；change 返回字符串。先编辑、再确认；Esc 放弃，非必填支持清除。日历支持方向键、Home/End、PageUp/PageDown，年份可直接输入。 |
| `UiColorPicker` | `#RRGGBB` 字符串 `v-model`，色板和 HEX 输入在确认后生效，input 返回颜色值。 |
| `UiFileInput` | 放在现有自定义上传 label 内，隐藏浏览器按钮；透传 accept/multiple/disabled 和原生 change 事件，支持 `click()`。上传区域提供可见焦点。 |
| `UiProgress` | value/max，提供 progressbar 语义和自定义轨道。 |
| `UiForm` | 替换 form，保留 submit/reset/外部 form 属性关联；阻止浏览器校验气泡，在字段下显示错误并聚焦首个错误项。 |

样式集中在 `styles/ui.css`；浮层定位和事件清理由 `useControlPopover` 统一处理。不要使用 div 模拟文本输入或可点击按钮。

验证：`pnpm test`、`pnpm build`。涉及交互变更时还需验证深浅主题、390px 窄屏、键盘导航、抽屉内浮层、必填校验及值更新。
