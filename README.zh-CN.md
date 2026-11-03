# Vivido

**一款面向开发者和 AI 智能体的跨平台 GPU 终端。**

Vivido 是一个在 macOS、Windows 和 Linux 上运行的 GPU 渲染终端模拟器。除了文本之外，它还可以在终端窗口内显示图像、视频和音频，并提供一套自动化 API，让 AI 智能体能够驱动、读取和观察它的窗口。

<img width="1915" height="1077" alt="vivida_screenshot" src="https://github.com/user-attachments/assets/752ddf5b-f8e6-46b0-b4a1-b8ce5133eece" />

（截图来自 [Vivida](https://github.com/vivido-dev/vivida)，它是 vivido 的一个封装。vivido 本身没有工作区和分屏，但你可以在 Vivido 中使用 [vvmux](https://github.com/vivido-dev/vvmux) 达到类似的效果）

## 面向开发者

### 性能

Vivido 通过 [wgpu](https://github.com/gfx-rs/wgpu) 在 GPU 上渲染文本，在 macOS 上使用 Metal，在 Windows 上使用 DirectX 12，在 Linux 上使用 Vulkan。在我们的[基准测试](docs/benchmarks.md)中，它在各终端里拥有最佳的平均成绩。

### 内联媒体

图像、编码视频、音频和摄像头画面可以与文本一起显示在终端窗口内，这样你无需先把文件复制到别处，就能直接查看图表或截图。通过 SSH 连接时，内置的 `vvssh` 可以从远程主机转发同样的媒体，并支持拖放式文件传输。

为 Kitty graphics 协议编写的工具同样可用：`kitten icat`、`timg`、`chafa`、`yazi` 输出的静态图片会内嵌显示，随文本一起滚动，并通过 Unicode 占位符透过 tmux 显示。图像数据必须经由终端本身传输——不支持文件、临时文件和共享内存传输方式。

### Windows

Vivido 在 Windows 上原生运行，使用 DirectX 12 和 ConPTY，具备与 macOS 和 Linux 上相同的渲染、内联媒体和自动化功能。PowerShell 和每个已安装的 WSL 发行版都会作为标签页打开在同一个窗口中，WSL shell 会自动获得 `TERM=vivido` 和真彩色支持。`vivido msg` 和无头会话通过仅限所有者访问的命名管道工作。签名安装程序会为你配置好 PowerShell 7、带有 Ubuntu 的 WSL 以及 PATH。参见 [在 Windows 上使用 Vivido](docs/windows.md)。

### 无障碍

在 macOS 上，屏幕阅读器接收到的是真实的文本、其几何位置、光标位置和回滚缓冲区，而不是一张渲染好的位图。

## 面向 AI 智能体

Vivido 的设计目标不仅是让人敲键盘输入，也为了让程序控制。

- **自动化 API。** `vivido msg` 为智能体提供结构化的网格快照、键入输入、窗口和进程控制、屏幕截图、净化后的会话记录、状态等待以及可重放的事件订阅，让智能体把终端当作数据来读取，而不是去刮取屏幕。
- **无头会话。** 在后台运行一个完整的终端——包括渲染器——而没有窗口，可按名称寻址、可被发现，并且可以安全地写入脚本：

  ```sh
  eval "$(vivido --headless --session build)"
  vivido msg get-text
  vivido msg screenshot
  ```

  这为 CI 任务、智能体和脚本提供了一个真实终端，而无需可见窗口。

- **发现与等待。** 智能体可以查找窗口、等待提示符出现并监听变化，而不是轮询。

从 `vivido msg capabilities` 开始，然后阅读
[智能体自动化指南](docs/ipc.md)和[无头会话](docs/headless.md)。

## 立即体验

从 releases、[vivido.dev](https://vivido.dev/) 下载，或使用 cargo。

```sh
cargo install vivido
```

参见 [INSTALL.md](INSTALL.md)。如果你只想快速浏览亮点功能，参见 [Features](docs/features.md)。

**给你下一个编程智能体一个真正可用的终端。** 让它用上 `vivido msg`，然后放手让它工作。

## 生态系统

Vivido 是构建在
[Vivid Protocol 1.5](https://github.com/vivido-dev/vivid_protocol) 之上的一系列工具的中心，它们都在
[`vivido-dev` 组织](https://github.com/vivido-dev)下：

| 工具 | 仓库 | 简介 |
|---|---|---|
| vvmux | [vivido-dev/vvmux](https://github.com/vivido-dev/vvmux) | 可分离的终端复用器；面板、回滚缓冲和实时媒体在分离后依然保留 |
| Vivi | [vivido-dev/vivi](https://github.com/vivido-dev/vivi) | 图像查看器和媒体播放器：以流量控制和关键帧恢复能力查看或提交图像、编码视频和音频 |
| vvrd | [vivido-dev/vvrd](https://github.com/vivido-dev/vvrd) | 全屏 PDF/EPUB/Markdown/Mermaid/Office 文档阅读器，带保留视口 |
| vrowser | [vivido-dev/vrowser](https://github.com/vivido-dev/vrowser) | 终端内的浏览器，支持视频和音频 |
| vvpaint | [vivido-dev/vvpaint](https://github.com/vivido-dev/vvpaint) | 终端内基于 Vivid 协议的"画图" |
| vvcam | [vivido-dev/vvcam](https://github.com/vivido-dev/vvcam) | 将连接的摄像头（V4L2/DirectShow，H.264）流式传输到全窗口终端表面 |
| vvland | [vivido-dev/vvland](https://github.com/vivido-dev/vvland) | 运行并流式传输隔离的 Weston 或 Sway 桌面，或单个 Wayland 应用 |
| vivida | [vivido-dev/vivida](https://github.com/vivido-dev/vivida) | 围绕 Vivido 面板构建的工作区和标签页管理器 |
| vvDOOM | [vivido-dev/vvdoom](https://github.com/vivido-dev/vvdoom) | 通过 Vivid 媒体栈运行的 Doom——旗舰级示例生产者 |

（`vvssh` 不需要单独的仓库——它随 Vivido 本身一起发布。）

## 致谢

Vivido 最初从 [Alacritty](https://github.com/alacritty/alacritty) 分叉而来——我们衷心感谢它的作者和贡献者们打下的出色基础。

## 许可证

Vivido 基于 Apache-2.0 许可证发布。参见 [LICENSE](LICENSE)。

## 注意

> [!WARNING]
> **在 Windows 上，请不要与 Vivido 一起安装 WezTerm。** 较旧的 Vivido 版本可能会从 `PATH` 加载 WezTerm 自带的 `conpty.dll`。我们复现了该 DLL 导致按键释放事件丢失的问题，会使 vvdoom 等应用中的控制在第一次移动后失去响应。更新版本的 Vivido 改用 Windows 内置的 ConPTY。如果已经安装了 WezTerm，请将其目录从 `PATH` 中移除或将其卸载，然后完全重启 Vivido。
