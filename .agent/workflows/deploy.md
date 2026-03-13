---
description: 远端编译部署工作流 — 同步代码到 zz@192.168.1.226 并编译/测试/部署
---

# 远端部署工作流

> **完整文档**: `/Volumes/Sam_4TB/Gravity/deploy/deploy-plan.md`
> **所有脚本**: `/Volumes/Sam_4TB/Gravity/deploy/`
> **配置文件**: `/Volumes/Sam_4TB/Gravity/deploy/deploy.conf`

## 环境
- 远端服务器：`zz@192.168.1.226`（28核, 32GB RAM, Ubuntu 24.04）
- 远端目录：`/home/zz/Gravity/`（每个项目有 git 目录和 dev 目录）
- 本地源码：`/Volumes/Sam_4TB/Gravity/{reth,gravity-reth,gravity-sdk}`
- dev 目录的 `target/` 软链接到 git 目录的 `target/`，共享编译缓存

## 三阶段流程

```
本地修改 → rsync 到远端 dev 目录 → 远端编译+测试 → 本地 commit+push → 远端 git pull
```

## 开发迭代

### 同步 + 编译
// turbo
```bash
PROJECT=gravity-reth /Volumes/Sam_4TB/Gravity/deploy/deploy.sh
```

### 同步 + 编译 + 测试
// turbo
```bash
PROJECT=gravity-reth /Volumes/Sam_4TB/Gravity/deploy/deploy.sh --test
```

### 同步 + 编译 + 测试（带过滤）
// turbo
```bash
PROJECT=gravity-reth /Volumes/Sam_4TB/Gravity/deploy/deploy.sh --test --test-filter "test_name"
```

### 只同步代码
// turbo
```bash
PROJECT=gravity-reth /Volumes/Sam_4TB/Gravity/deploy/sync.sh
```

### 快速检查编译（不生成二进制）
// turbo
```bash
PROJECT=gravity-reth /Volumes/Sam_4TB/Gravity/deploy/build.sh --check
```

### 只编译
// turbo
```bash
PROJECT=gravity-reth /Volumes/Sam_4TB/Gravity/deploy/build.sh
```

### 运行测试
// turbo
```bash
PROJECT=gravity-reth /Volumes/Sam_4TB/Gravity/deploy/test.sh
```

## 发布

### 发布并远端编译
```bash
PROJECT=gravity-reth /Volumes/Sam_4TB/Gravity/deploy/publish.sh -m "commit message" --build
```

### 只远端 pull（已手动 push）
// turbo
```bash
PROJECT=gravity-reth /Volumes/Sam_4TB/Gravity/deploy/publish.sh --pull-only
```

### 一行发布
```bash
PROJECT=gravity-reth /Volumes/Sam_4TB/Gravity/deploy/deploy.sh --publish -m "commit message"
```

## 状态和管理

### 查看状态（local / remote git / remote dev 三端对比）
// turbo
```bash
PROJECT=gravity-reth /Volumes/Sam_4TB/Gravity/deploy/status.sh
```

### 查看日志
// turbo
```bash
PROJECT=gravity-reth /Volumes/Sam_4TB/Gravity/deploy/status.sh --logs 50
```

### 停止进程
```bash
PROJECT=gravity-reth /Volumes/Sam_4TB/Gravity/deploy/stop.sh
```

## gravity-sdk 操作

以上所有命令把 `PROJECT=gravity-reth` 换成 `PROJECT=gravity-sdk` 即可。

## Shell 快捷指令（已配置在 ~/.zshrc）

| gravity-reth | gravity-sdk | 作用 |
|---|---|---|
| `dy` | `ds` | sync + build |
| `dyt` | `dst` | sync + build + test |
| `dys` | `dss` | 只 sync |
| `dyb` | `dsb` | 只 build |
| `dyc` | `dsc` | cargo check |
| `dyst` | `dsst` | 状态 |
| `dyl` | - | 日志 |
| `dyp -m "msg"` | `dsp -m "msg"` | 发布 |

## 注意事项
- gravity-sdk 需要 `tokio_unstable` flag（已通过 `.cargo/config.toml` 配置）
- `build.sh` 默认在 dev 目录编译，`--production` 在 git 目录编译
- `publish.sh` 的 commit+push 操作不可自动执行（非 turbo），需要用户确认
- 远端 GitHub SSH 访问较慢，首次 clone 大仓库耗时较长，后续增量更新快
