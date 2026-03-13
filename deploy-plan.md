# Gravity 远端编译部署工作流

## 概述

本地修改代码 → rsync 同步到远端 → 远端编译+测试验证 → 本地 commit+push → 远端 git pull

**设计原则**：代码读写在本地，编译/测试/运行在远端服务器。远端维护两套目录：dev（开发迭代）和 git（正式版本）。

## 环境

| 项 | 值 |
|---|---|
| 远端服务器 | `zz@192.168.1.226`（Ubuntu 24.04, 28核, 32GB RAM） |
| 本地源码 | `/Volumes/Sam_4TB/Gravity/{gravity-reth,gravity-sdk,reth}` |
| 远端目录 | `/home/zz/Gravity/` |
| 部署脚本 | `/Volumes/Sam_4TB/Gravity/deploy/` |
| 配置文件 | `/Volumes/Sam_4TB/Gravity/deploy/deploy.conf` |

## 远端目录结构

```
/home/zz/Gravity/
  gravity-reth/              # Git 仓库（clone from GitHub，正式版本）
    target/                  # 编译产物
  gravity-reth-dev/          # rsync 开发目录（可能有未提交改动）
    target/ → ../gravity-reth/target/   # 软链接，共享编译缓存
  gravity-sdk/               # Git 仓库
    target/
  gravity-sdk-dev/           # rsync 开发目录
    target/ → ../gravity-sdk/target/
  logs/
```

**关键设计**：dev 目录的 `target/` 是软链接到 git 仓库的 `target/`，两者共享编译缓存，避免重复编译。

## 三阶段工作流

### Phase 1: Dev Sync（开发同步）

本地改完代码后，rsync 到远端 dev 目录：

```bash
# gravity-reth
dys                           # sync only
dy                            # sync + build
dyt                           # sync + build + test
dyc                           # sync + cargo check (最快)

# gravity-sdk
dss                           # sync only
ds                            # sync + build
dst                           # sync + build + test
dsc                           # sync + cargo check
```

等价的完整命令（无需 alias）：
```bash
PROJECT=gravity-reth /Volumes/Sam_4TB/Gravity/deploy/deploy.sh
PROJECT=gravity-reth /Volumes/Sam_4TB/Gravity/deploy/deploy.sh --test
PROJECT=gravity-sdk /Volumes/Sam_4TB/Gravity/deploy/build.sh --check
```

### Phase 2: Remote Build + Test（远端验证）

在 dev 目录中编译和测试，确认代码可以安全提交：

```bash
dyb                           # build (dev 目录)
dyb --profile dev             # dev profile（更快编译）
dyb --check                   # cargo check（最快）
dyb --clippy                  # clippy 检查
dyb --production              # 在 git 目录编译
dyt                           # sync + build + test
PROJECT=gravity-reth /Volumes/Sam_4TB/Gravity/deploy/test.sh --filter "test_name"
PROJECT=gravity-reth /Volumes/Sam_4TB/Gravity/deploy/test.sh -p reth-trie-parallel
```

### Phase 3: Publish（发布）

验证通过后，本地 commit+push，远端 git pull：

```bash
# commit + push + remote pull
dyp -m "feat: your change description"

# commit + push + remote pull + remote build
dyp -m "feat: your change" --build

# 已手动 push，只让远端 pull
dyp --pull-only

# 一行完成发布
PROJECT=gravity-reth /Volumes/Sam_4TB/Gravity/deploy/deploy.sh --publish -m "feat: your change"
```

## Shell 快捷指令速查

已配置在 `~/.zshrc` 中，新终端自动生效。

### gravity-reth (`dy*`)

| 指令 | 作用 |
|------|------|
| `dy` | sync + build |
| `dyt` | sync + build + test |
| `dys` | 只 sync |
| `dyb` | 只 build（可加 `--check`, `--clippy`, `--production`） |
| `dyc` | cargo check（最快验证） |
| `dyst` | 查看状态（local / remote git / remote dev 三端对比） |
| `dyl` | 查看远端日志（默认50行） |
| `dyp -m "msg"` | 发布（commit + push + remote pull） |

### gravity-sdk (`ds*`)

| 指令 | 作用 |
|------|------|
| `ds` | sync + build |
| `dst` | sync + build + test |
| `dss` | 只 sync |
| `dsb` | 只 build |
| `dsc` | cargo check |
| `dsst` | 查看状态 |
| `dsp -m "msg"` | 发布 |

## 脚本列表

| 脚本 | 用途 |
|------|------|
| `deploy.conf` | 中心配置（服务器地址、项目定义、rsync 排除等） |
| `lib.sh` | 共享函数库（SSH、日志、路径、tmux 辅助） |
| `sync.sh` | rsync 到远端 dev 目录，自动创建 target 软链接 |
| `build.sh` | 远端 cargo build（默认 dev 目录，`--production` 用 git 目录） |
| `test.sh` | 远端 cargo test（支持 `--filter`, `--package`, `--nextest`） |
| `publish.sh` | 本地 commit+push → 远端 git pull（可选 `--build`） |
| `deploy.sh` | 编排器（dev 循环 / `--publish` 发布模式） |
| `status.sh` | 状态面板（支持 `--logs`, `--follow`, `--attach`） |
| `run.sh` | tmux 中启动二进制 |
| `stop.sh` | 优雅停止（先 SIGINT 等30s，再 force kill） |
| `setup-remote.sh` | 一次性远端环境初始化 |

## 配置说明

### deploy.conf 核心配置

- `REMOTE_HOST`: 远端服务器地址（`zz@192.168.1.226`）
- `REMOTE_BASE`: 远端根目录（`/home/zz/Gravity`）
- `LOCAL_BASE`: 本地源码根目录（`/Volumes/Sam_4TB/Gravity`）
- `VALID_PROJECTS`: 支持的项目列表（`reth gravity-reth gravity-sdk`）
- `RSYNC_EXCLUDES`: rsync 排除 `target/`, `.git/` 等

### 添加新项目

在 `deploy.conf` 的三个函数中各添加一行 `case`：
- `get_project_binary()`: 编译产物路径
- `get_project_cargo_args()`: cargo build 参数
- `get_project_run_args()`: 运行参数

## 注意事项

1. **gravity-sdk 需要 `tokio_unstable`**: 已通过 `.cargo/config.toml` 配置 `rustflags = ["--cfg", "tokio_unstable"]`
2. **远端 GitHub 访问慢**: git 走 SSH 协议，无代理。首次 clone 大仓库（如 gravity-aptos 396K objects）很慢，后续增量更新快
3. **stop.sh 安全性**: 先发 SIGINT 等待 30s 优雅关闭，超时才 force kill，对 reth 数据库安全
4. **Rust 版本**: 远端安装了 1.88 和 1.93 两个 toolchain，编译时由项目 `rust-toolchain.toml` 自动选择
