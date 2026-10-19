# 插件 SDK

SDK 定义清单格式、节点类型、C ABI 和数据结构。完整声明见 [src/lib.rs](src/lib.rs)，可构建实现见[画面滤镜示例](../effects/)。当前执行与图形后端为 Windows/D3D11。

## 1. 插件与节点

插件是安装和版本管理单元；节点是图中的功能单元。每个节点拥有自己的输入、输出、默认参数、参数描述和服务依赖。

| `implementation` | 输入 | 输出 | 主要回调 |
| --- | --- | --- | --- |
| `video_shader` | 一个 `frame` | 一个 `frame` | `VideoApi.describe` |
| `frame_analysis` | 一个 `frame` | `draw_list`，可附带 `detections` | `PluginApi.process` |
| `scene_source` | 无 | 一个 `draw_list` | `PluginApi.process`，生成初始静态场景 |
| `overlay` | 一个 `draw_list` | 一个 `layer` | `PluginApi.render`，输出 `Rendered` |
| `detection_control` | 一个 `detections` | `input_commands`，可附带 `layer` | `PluginApi.render`，输出 `Scene` |

宿主提供原始画面、视频输出、叠加输出和远端输入节点。同类型端口才能连接，图中不允许环路。叠加输出支持多个图层并按用户顺序合成。

静态场景节点不请求 CPU 画面采样。控制节点的 `layer_renderer` 可指定一个绘制节点，用于将其图层输出转换为可呈现的网格。

## 2. 内嵌清单

节点插件在清单中使用 `capability: "nodes"`。最小结构如下，参数与节点 ID 仅作格式示例：

```json
{
  "id": "example.filters",
  "name": "Example Filters",
  "version": "0.1.0",
  "abi": 1,
  "capability": "nodes",
  "dependencies": [],
  "config": {},
  "nodes": [{
    "type_id": "example.filters.effect",
    "schema_version": 1,
    "name": "Effect",
    "description": "Describe the visible effect here.",
    "category": "Filters",
    "implementation": "video_shader",
    "inputs": [{"id": "frames", "name": "Frame", "data_type": "frame"}],
    "outputs": [{"id": "frames", "name": "Frame", "data_type": "frame"}],
    "dependencies": [],
    "config": {"amount": 0.5},
    "config_labels": {"amount": "Amount"},
    "config_schema": {"amount": {"order": 0, "min": 0, "max": 1, "step": 0.01}}
  }]
}
```

节点 ID 必须位于插件 ID 的命名空间下，例如 `example.filters.effect`。ID 和端口 ID 用于持久化与连接，不能随显示名称改变。

在 DLL 源码中调用一次：

```rust
farwing_plugin_api::embed_manifest!(include_bytes!("../manifest.json"));
```

宏把清单放入 Windows DLL 的只读段 `.fwmeta`。宿主读取该段时不执行 DLL。清单容器版本与执行 ABI 分别检查，JSON 大小上限为 64 KiB。源码中的 JSON 不需要随 DLL 分发。

## 3. 参数和依赖

`config` 提供新节点的默认值，`config_labels` 提供显示名称，`config_schema` 控制编辑方式：

| 字段 | 用途 |
| --- | --- |
| `order` | 参数显示顺序 |
| `min`、`max`、`step` | 数值编辑范围和步长；插件仍须校验运行参数 |
| `kind: "choice"`、`options` | 单选列表；选项包含 `label`、`value`，可通过 `updates` 同步修改其他参数 |
| `kind: "file"`、`extension` | 列出插件目录中指定扩展名的文件；`options` 可补充名称及关联参数 |
| `kind: "multi"` | 多选列表；当前实现从 `source` 指向的逗号分隔字符串取得项目名称，也支持 `source_choice` 的选项更新值 |
| `kind: "hotkey"` | 快捷键录入；当前由控制节点消费 |
| `kind: "hidden"` | 隐藏内部参数 |

普通字符串、布尔值和数值可直接按 `config` 的值类型生成控件。节点参数保存到节点图中，不写回 DLL。

没有节点的推理服务使用 `capability: "inference"`；其包级参数可在插件管理页配置。Windows 用户设置位于 `%LOCALAPPDATA%/Farwing/plugin-settings/<插件ID>.json`，与 DLL 分开保存。

`dependencies` 填服务插件 ID。包级依赖适用于所有节点，节点级依赖只用于该节点；可选能力应使用节点级依赖。当前服务接口为推理服务，一条依赖计划最多包含一个推理提供者。未使用的节点不会初始化其依赖。

宿主将 `model`、`runtime` 两个文件参数解析为插件目录内的绝对 UTF-8 路径，并检查目录边界；其他文件字段由插件自行处理。

## 4. 查询入口与实例生命周期

| 插件类型 | 导出符号 | 返回值 |
| --- | --- | --- |
| 普通节点 | `farwing_node_query_v1` | `*const PluginApi` |
| 视频 Shader 节点 | `farwing_video_node_query_v1` | `*const VideoApi` |
| 推理服务 | `farwing_plugin_query_v1` | `*const PluginApi` |

两个节点查询入口接收 `(abi_version: u32, type_id: *const u8, len: usize)`。服务查询只接收 ABI 版本。节点 ID 是借用的 UTF-8 字节，不以零结尾。未知版本或未知节点返回空指针；成功时返回在 DLL 存活期间有效的静态函数表。

`PluginApi` 的调用顺序为 `create → process/render → destroy`。`create` 接收配置 JSON 和 `HostServices`；节点配置中会包含宿主添加的 `$node_type`。每个节点实例独立持有状态，不能把一个节点的状态覆盖到另一个节点上。

所有跨 DLL 数据使用 SDK 的 `repr(C)` 类型、字节缓冲区或 JSON，不传递 Rust 容器的内部布局。输入指针仅在调用期间有效；输出写入宿主提供的缓冲区，并填写实际长度。返回 0 表示成功，非零表示失败。失败的 `create` 应自行清理未交付的资源，panic 或异常不得跨越 ABI。

推理服务通过 `inference` 回调返回 `InferenceService`，由 `open / run / close` 管理模型与推理。当前张量接口每次传递一个 Float32 输入和一个 Float32 输出，输入形状固定四维，输出用 `rank` 表示至多四维。`HostServices` 指针只在 `create` 调用期间有效；节点可复制服务函数表，在自身实例存活期间使用服务上下文，不能保留指向临时函数表的指针。

`VideoApi` 不创建持续实例：`describe` 接收配置 JSON，并把 `VideoProgram` JSON 写入输出缓冲区。

## 5. 画面分析与叠加层

分析输入 `Frame` 为正向、无插件叠加层的 RGBA8，宽高均不超过 640。`sequence` 用于识别样本，`stride` 表示行跨度；帧数据只在回调期间有效。宿主保留最新待处理样本，不为插件排队积压历史画面。

`process` 的空帧调用用于生成初始场景。`scene_source` 使用这一通路生成静态场景，不接收图像采样；窗口变化由绘制回调中的 `Viewport` 处理。

`Scene` 包含检测结果、绘图指令和图层。`Space::Video` 使用相对于视频的归一化坐标，`Space::View` 使用相对于视频锚点的本地逻辑像素。`persistent` 图层可跨采样间隔保留；依赖某帧的标注应设为非持久层。

绘制节点接收场景和视口，输出 `Rendered`：纹理更新、分层网格、裁剪矩形和顶点索引。顶点颜色及输出纹理像素使用预乘 Alpha 的 RGBA8。场景 JSON 上限为 1 MiB，渲染结果 JSON 上限为 8 MiB；具体结构与常量以 SDK 源码为准。

## 6. 视频 Shader

当前 `VideoProgram.backend` 固定为 `d3d11-rgba16f`，每个 Pass 提供 D3D11 `ps_5_0` 字节码。

| 绑定 | 内容 |
| --- | --- |
| `t0` | 当前 Pass 的 RGBA 输入纹理 |
| `t2` | 历史输入纹理；是否有效由 `history_valid` 指示 |
| `s0` | 线性采样器 |
| `b1` | 四个 float：`phase, input_width, input_height, history_valid` |
| `b2` | `VideoPass.params` 的四个 float |

`scale_numerator / scale_denominator` 决定输出尺寸比例。`phases` 为递增的输出时间分数，范围 `(0,1]` 且最后一项必须为 1。历史不可用时只生成当前帧；多个输出必须声明非零 `delay_frames`。

普通单帧滤镜使用比例 `1/1`、`phases: [1.0]`、`delay_frames: 0`。当前视频图最多 8 个节点、16 个 Pass，路径声明延迟最多 2 帧，输出倍率最多 4 倍。接口提供像素 Shader 描述，不开放任意 GPU 设备或资源句柄。

## 7. 控制节点

控制节点消费标准检测结果，通过 `InputCommand` 输出相对移动、修正量或短按点击。它们必须经过宿主的前台归属、控制权限、有效期和激活代次检查；绘制或分析输出中的控制状态不能绕过这些检查。

快捷键参数形如 `{"binding":{"key":119,"modifiers":0},"mode":"toggle"}`。Windows 下 `key` 使用虚拟键码，鼠标键也沿用该命名；修饰位依次为 Ctrl=1、Shift=2、Alt=4、Win=8。`mode` 为 `toggle`、`hold` 或 `trigger`，在控件右键菜单中切换。

所有已配置快捷键条件按 AND 组合；全部清空时不执行控制。每个功能最多四个条件，最多一个单次触发条件。`enabled` 参数可用作状态浮层中的总开关。失焦、菜单接管、断线和节点图替换会清理激活状态及过期指令。

`Scene.observation` 由宿主提供，包含样本身份、采样/调度时的实体鼠标累计位移和已提交修正量。提交成功不代表远端已执行。当前声明式接口只提供 SDK 中列出的鼠标指令，不代表已有任意键盘注入、剪贴板或文件传输接口。

## 8. 边界

宿主会校验清单、端口、图结构、资源大小、数据身份和执行时限。插件进程隔离用于故障处理，不是安全沙箱；原生 DLL 仍具有运行账号的系统权限。

Windows 是当前已实现的执行/绘制平台。SDK 的其他平台元数据布局不代表已有对应运行后端。源码与示例遵循项目根目录的[许可](../../LICENSE)。
