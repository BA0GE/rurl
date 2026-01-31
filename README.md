# rurl - 极速多线程下载器 (Rust URL)

`rurl` 是一个基于 Rust 编写的高性能并发下载工具，旨在提供类 `curl` 的使用体验，同时内置多线程加速、断点续传和动态分块等高级特性。

它不仅具备极速下载能力，还集成了专业下载软件的核心功能，如工作窃取（Work Stealing）负载均衡、文件完整性校验、管道流支持以及跨平台兼容性。

## 功能特性

- **🚀 极速多线程**：支持多线程并发下载，充分利用带宽资源。
- **⚡️ 动态分块**：采用 Work Stealing 算法，智能拆分慢速任务，防止"长尾效应"。
- **💾 断点续传**：自动记录下载进度，支持程序中断后秒级恢复。
- **🛡️ 完整性校验**：支持 MD5、SHA1、SHA256 校验，确保文件完整无损。
- **🔗 类 Curl 体验**：兼容 `-O`, `-A`, `-x`, `-u`, `-i`, `-I`, `-L` 等常用参数。
- **🌊 管道支持**：
  - **输入管道**：`echo "url" | rurl`
  - **输出管道**：`rurl url -o - > file` (流式输出)
- **🌐 Web API**：内置 RESTful API 服务器，支持远程管理下载任务。
- **🔌 代理支持**：原生支持 HTTP/HTTPS/SOCKS5 代理。
- **📦 跨平台**：支持 Windows, macOS, Linux。

## 安装与运行

确保已安装 Rust 环境 (Cargo)，然后克隆仓库并运行：

```bash
git clone https://github.com/your-repo/rurl.git
cd rurl
cargo build --release
# 二进制文件位于 target/release/rurl
# 建议将其移动到系统 PATH 中
cp target/release/rurl /usr/local/bin/
```

## ⚡️ 动态分块 (Dynamic Split / Work Stealing)

**动态分块**是 `rurl` 解决多线程下载中"长尾效应"的核心技术。

### 什么是长尾效应？
在普通多线程下载中，文件被均匀分成 N 个块。如果某个线程因为网络波动或服务器限制导致速度极慢，整个下载任务就必须等待这个最慢的线程完成，即使其他线程早已空闲。这被称为"长尾效应"。

### rurl 是如何解决的？
`rurl` 内置了**工作窃取 (Work Stealing)** 算法：
1.  **实时监控**：调度器实时监控所有线程的状态和剩余工作量。
2.  **自动拆分**：当检测到某个线程（如线程 A）剩余工作量很大且速度较慢，而此时有其他线程（如线程 B）已经空闲时。
3.  **工作窃取**：线程 B 会主动从线程 A 的任务中"窃取"一半的剩余数据，创建一个新的分块进行下载。
4.  **循环往复**：这个过程会持续进行，直到所有分块都小于最小阈值（默认 1MB）。

### 如何使用？

动态分块功能**默认开启**，无需额外配置。

```bash
# 默认开启动态分块
rurl "https://example.com/big-file.iso"
```

如果你想自定义分块行为，可以使用以下参数：

```bash
# 设置最小分块大小为 512KB (默认 1MB)
# 当分块剩余大小小于 2 * 512KB 时，将不再拆分
rurl --min-chunk-size 512k "https://example.com/big-file.iso"

# 禁用动态分块 (不推荐，除非服务器不支持 Range 请求或有严格限制)
rurl --no-dynamic-split "https://example.com/file.zip"
```

### 演示场景

假设你正在下载一个 100MB 的文件，开启 4 个线程：
1.  初始状态：线程 1-4 各分配 25MB。
2.  运行中：线程 1, 2, 3 快速完成，但线程 4 卡住了，只下载了 1MB，还剩 24MB。
3.  动态介入：
    - 空闲的线程 1 发现线程 4 任务繁重。
    - 线程 1 从线程 4 窃取 12MB（后半部分）。
    - 现在线程 4 负责前 12MB，线程 1 负责后 12MB。
4.  结果：原本需要漫长等待的线程 4 的任务被分担，下载迅速完成。

## 命令行使用说明

### 1. 基础下载

```bash
# 下载文件（自动解析文件名）
rurl "https://example.com/file.zip"

# 下载文件到指定路径
rurl "https://example.com/file.zip" -o ./downloads/game.zip

# 使用远程文件名 (类似 curl -O)
rurl -O "https://example.com/file.zip"
```

### 2. 高级功能

```bash
# 多线程加速 (16 线程)
rurl -t 16 "https://example.com/big-file.iso"

# 限制下载速度 (5MB/s)
rurl --limit-rate 5m "https://example.com/file.zip"
# 或者使用短参数
rurl -l 500k "https://example.com/file.zip"

# 身份认证 (Basic Auth)
rurl -u "user:password" "https://example.com/secure-file"

# 设置代理 (HTTP/SOCKS5)
rurl -x "socks5://127.0.0.1:7890" "https://google.com/file"

# 自定义 User-Agent 和 Header
rurl -A "MyDownloader/1.0" -H "Authorization: Bearer token" "https://api.example.com/data"

# 文件完整性校验 (下载完成后自动根据长度识别 md5/sha1/sha256)
rurl "https://example.com/file.zip" --checksum "d41d8cd98f00b204e9800998ecf8427e"
```

### 3. 管道模式 (Streaming)

```bash
# 从 stdin 读取 URL 列表
cat urls.txt | rurl

# 将下载内容输出到 stdout (适用于与其他工具配合)
rurl "https://example.com/archive.tar.gz" -o - | tar -xzf -
```

### 4. 发送 HTTP Body (API 测试)

```bash
# 发送 JSON 数据 (隐含 POST 方法)
rurl "https://api.example.com/create" --json '{"name": "test", "value": 123}'

# 发送表单数据
rurl "https://api.example.com/submit" -d "key1=value1&key2=value2"

# 指定请求方法
rurl -X PUT "https://api.example.com/resource/1"
```

### 5. HTTP 协议调试

```bash
# 查看响应头 (Include headers)
rurl -i "https://example.com"
```
