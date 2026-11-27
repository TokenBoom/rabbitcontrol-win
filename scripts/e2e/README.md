# 实机联测辅助脚本（UIA 驱动，语言中立）

- `capture-window.ps1`：前置应用窗口 + DPI 物理坐标截图 → `%TEMP%\rc-e2e-win.png`
- `capture-by-pid.ps1`：按进程主窗口句柄截图（含 30px 阴影边距）
- `dump-uia-tree.ps1`：枚举主窗口 UIA 树（ControlType/Name/AutomationId）
- `login-uia.ps1`：UIA ValuePattern 填邮箱/密码 + InvokePattern 点登录

## 已知限制
- PasswordBox 对 SendKeys 合成键盘不稳定（首字符吞字）；UIA ValuePattern
  视窗口 busy 态可能落空。真人键盘输入不受影响。
- 截图/点击脚本必须 `SetProcessDPIAware()`（200%+ 缩放屏坐标虚化问题）。
- 机器锁屏（安全桌面激活）时截图拿到的是壁纸，`OpenInputDesktop` 失败可判定。
