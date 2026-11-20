# One-KVM（Android USB 控制版）

One-KVM 是一个用 Rust 编写的轻量 IP-KVM，可通过网络远程管理电脑，并可通过 USB 同时控制一台 Android 手机。

- 电脑控制：USB HDMI 采集 + USB OTG / CH9329 / 蓝牙 HID
- 手机控制：USB 连接 Android 手机，通过 `adb` + `scrcpy-server` 采集画面和注入输入
- 控制台左上角可在 **PC / 手机** 之间切换，同一时间只控制一条路线
- 手机画面和声音复用现有 WebRTC 通道，支持触摸、滚轮、键盘和手机工具栏
- 离开控制台后停止 WebRTC 推流；手机会话保留 5 分钟后自动挂起，下次连接按需重启

手机模式仅支持 WebRTC。

## 环境要求

- Linux（推荐 Debian / Ubuntu / Armbian，aarch64 或 x86_64）
- Android 手机一部，支持 USB 调试
- 板端建议使用带供电的 USB Hub，避免手机高负载时供电不足
- 程序需要访问 `/dev/video*`、`/dev/hidg*`、USB 设备和 ALSA，建议以 root 运行

## 依赖安装

### 1. 安装 adb

Debian / Ubuntu / Armbian：

```bash
sudo apt update
sudo apt install -y adb android-sdk-platform-tools-common
```

确认 adb 可用：

```bash
adb version
adb devices -l
```

手机通过 USB 连接到板端的 **USB Host** 口（不是 OTG/Type-C gadget 口），在手机上打开：

1. 设置 → 关于手机 → 连续点击“版本号”开启开发者选项
2. 开发者选项 → 打开“USB 调试”
3. 连接后手机上弹出的“允许 USB 调试”选择允许

`adb devices -l` 中应出现类似输出：

```text
3B1621003AW00000  device usb:5-1 product:... model:... device:...
```

如果显示 `unauthorized`，请在手机上重新确认授权。

### 2. 安装 scrcpy-server

One-KVM 不需要在板端安装桌面版 scrcpy 客户端，只需要与版本匹配的 `scrcpy-server`。当前默认版本是 **3.3**。

```bash
sudo mkdir -p /usr/local/share/scrcpy
curl -fL -o /tmp/scrcpy-server-v3.3 \
  https://github.com/Genymobile/scrcpy/releases/download/v3.3/scrcpy-server-v3.3
sudo install -m 0644 /tmp/scrcpy-server-v3.3 /usr/local/share/scrcpy/scrcpy-server
```

支持的默认查找路径：

```text
/usr/local/share/scrcpy/scrcpy-server
/usr/share/scrcpy/scrcpy-server
/usr/share/one-kvm/scrcpy-server
/opt/one-kvm/scrcpy-server
```

`scrcpy-server` 的版本必须与 One-KVM 使用的版本号一致，否则服务端会拒绝启动。如果使用发行版自带的 scrcpy 包，请把 One-KVM 的版本号设置为 `scrcpy --version` 输出的版本，例如：

```bash
scrcpy --version
# scrcpy 3.3.4 ...
```

则配置：

```ini
Environment=ONE_KVM_SCRCPY_SERVER_VERSION=3.3.4
```

### 3. 运行库

如果使用 deb 包安装，依赖会由包管理器处理。直接运行二进制时通常需要：

```bash
sudo apt install -y libasound2 libudev1 libdrm2 libstdc++6 ffmpeg
```

`ffmpeg` 命令不是手机控制的硬性依赖，仅用于排查视频/音频问题。

### 4. 构建依赖（从源码编译时）

```bash
sudo apt update
sudo apt install -y \
  build-essential pkg-config cmake clang protobuf-compiler \
  libssl-dev libasound2-dev libudev-dev \
  libavcodec-dev libavutil-dev libx264-dev libx265-dev libvpx-dev \
  libturbojpeg0-dev libopus-dev
```

前端需要 Node.js 20 或更高版本。ARM 平台开启 `rkmpp` 时还需要 Rockchip MPP / RGA 开发库。

## 安装与运行

### 使用 deb 包

从 Releases 下载与设备架构匹配的包：

```bash
sudo apt install ./one-kvm_<version>_<arch>.deb
```

### 从源码运行

```bash
cd One-KVM

cd web
npm ci
npm run build
cd ..

# x86_64 / 不使用 RKMPP
cargo build --release

# ARM + Rockchip 平台
cargo build --release --features rkmpp
```

运行：

```bash
sudo ./target/release/one-kvm
```

浏览器访问：

```text
http://<设备IP>:8080
```

首次访问会引导创建管理员账号并完成基础配置。

### Docker

如果使用 Docker，需要把 USB 设备和系统目录映射进容器，并使用 host 网络：

```bash
docker run --name one-kvm -itd \
  --privileged=true --restart unless-stopped \
  -v /dev:/dev -v /sys:/sys \
  --net=host \
  silentwind0/one-kvm-full
```

## 手机控制使用

1. 手机通过 USB 连接到板端 USB Host 口。
2. 执行 `adb devices -l`，确认状态为 `device` 且有 `usb:` 字段。
3. 打开 One-KVM 控制台，点击左上角 **手机**。
4. 首次切换会推送 scrcpy-server、建立 adb reverse 隧道并启动服务，通常需要数秒。
5. 连接成功后可进行以下操作：

   - 触摸：单指点击、拖动
   - 滚轮：上下滚动
   - 键盘：物理键盘或虚拟键盘
   - 工具栏：返回、主页、最近任务、音量、电源、旋转、全屏

6. 点击 **PC** 可切回电脑；PC 的 USB OTG gadget 始终保持绑定，无需重新插拔。

### 会话与资源

- 手机画面通过 WebRTC 传输，视频规格默认为 `max_size=1600`、最高 60fps、8 Mbps
- 手机音频通过 Opus 编码后复用 WebRTC 音频通道
- 离开控制台页面后 WebRTC 断开；手机会话保留 5 分钟
- 5 分钟内重新连接会复用现有会话
- 超过 5 分钟无观看者会停止 scrcpy-server、移除 adb reverse 并挂起
- 下次连接 WebRTC 时会自动重启手机会话
- adb server 不会被停止，便于下次快速连接

### 配置项

以下环境变量可覆盖默认值：

| 环境变量 | 默认值 | 作用 |
|---|---|---|
| `ONE_KVM_ADB` | `adb` | adb 可执行文件路径 |
| `ONE_KVM_SCRCPY_SERVER` | `/usr/local/share/scrcpy/scrcpy-server` | scrcpy-server 文件路径 |
| `ONE_KVM_SCRCPY_SERVER_VERSION` | `3.3` | scrcpy-server 版本号 |

systemd 示例：

```bash
sudo systemctl edit one-kvm
```

```ini
[Service]
Environment=ONE_KVM_ADB=/usr/bin/adb
Environment=ONE_KVM_SCRCPY_SERVER=/usr/local/share/scrcpy/scrcpy-server
Environment=ONE_KVM_SCRCPY_SERVER_VERSION=3.3
```

修改后执行：

```bash
sudo systemctl daemon-reload
sudo systemctl restart one-kvm
```

## 常见问题

| 现象 | 排查 |
|---|---|
| 手机按钮灰色 / `unavailable` | 检查 `adb` 是否安装、scrcpy-server 路径是否正确 |
| `unauthorized` | 在手机上重新允许 USB 调试，必要时撤销授权后重新插拔 |
| 检测不到手机 | 确认使用 USB Host 口、数据线正常、`adb devices -l` 有 `usb:` 字段 |
| 切换后一直显示连接中 | 检查 scrcpy-server 版本是否与 `ONE_KVM_SCRCPY_SERVER_VERSION` 一致 |
| 有画面但触摸偏移 | 确认手机旋转后控制台已重新上报视频尺寸；刷新页面后重试 |
| 有画面无声音 | 确认 Android 11 及以上、手机未静音，且 One-KVM 音频未关闭 |
| 切回 PC 没有画面 | 检查 PC 采集卡是否被其他进程占用，查看控制台提示和运行日志 |
| 手机供电不足 | 使用带供电的 USB Hub，避免手机高负载时掉线 |

## 已知限制

- 仅支持单台 USB 手机
- 手机模式仅支持 WebRTC；切回 PC 后如原来使用 MJPEG，需要手动切回 MJPEG 模式
- 触摸为单指，不支持双指缩放和多指手势
- 键盘使用 Android keycode 注入，中文输入法组合输入暂未接入
- 手机锁屏后仍需人工解锁；`stay_awake` 不能绕过 Android 安全锁屏
- Android 10 及以下无法采集音频，会自动降级为纯视频

## 项目与许可

- 项目文档：<https://docs.one-kvm.cn/>
- 问题反馈：<https://github.com/zoneBen/One-KVM/issues>
- License: AGPL-3.0-only
