# 发布流程

## CI 门禁

`.github/workflows/ci.yml`，每次 push 和 PR 触发。

| Job | 平台 | 内容 |
|---|---|---|
| `test` | ubuntu / windows / macos | clippy `-D warnings` + `cargo test --workspace --locked` |
| `smoke` | ubuntu | `scripts/e2e/smoke.sh` 进程级冒烟 |

`fail-fast: false`——一个平台挂了也要看另一个的结果，跨平台差异正是这个 matrix 的目的。

传输层在三平台实跑：Windows 走命名管道，Linux/macOS 走 unix socket。只有 Linux 需要装 `libsqlite3-dev`，macOS 和 Windows 用 bundled SQLite。

`--locked` 让 `Cargo.lock` 与 manifest 不一致时直接失败，而不是悄悄改锁文件。

**smoke job 只在 ubuntu 跑**。`smoke.sh` 已内置 Windows 适配（cygpath、`OC_SOCKET` 管道隔离），但 CI 里还没挂 Windows 腿。

### 关于 `cargo fmt`

CI **不跑** `cargo fmt --check`。本仓从未整体 rustfmt 过，现状有 82 个文件、447 处待重排。全仓重排是一次独立的大 diff，不该混进日常改动。

要启用就先单独跑一次 `cargo fmt --all` 并单独提交，再放开 `ci.yml` 里注释掉的那两行。

---

## 发布构建

`.github/workflows/build-ubuntu.yml`，**仅在打 tag 时触发**，不跑测试（测试门禁在 `ci.yml`）。

产物矩阵：

| 平台 | 架构 | 产物名 |
|---|---|---|
| Ubuntu | x86_64 | `oc-ubuntu` |
| macOS | x86_64 (Intel) | `oc-macos-*` |
| macOS | arm64 (Apple Silicon) | `oc-macos-*` |

每个包内含 `oc` 二进制（已 strip）、`install.sh`、`INSTALL.txt`、systemd unit 模板。构建完自动创建 GitHub Release 并上传。

Windows 产物用 `scripts/build-windows.ps1` 本地构建，未进 CI。

---

## 发布步骤

1. 确认 `ci.yml` 在目标 commit 上三平台全绿
2. 更新 `Cargo.toml` 的 `workspace.package.version`
3. 更新 `CHANGELOG.md`：把变更归到新版本号下，写明 Added / Changed / Fixed
4. 提交：`chore(release): <version>`
5. 打 tag 并推送：

```bash
git tag -a v0.2.3 -m "v0.2.3"
git push origin v0.2.3
```

6. 等 `build-ubuntu.yml` 跑完，检查 Release 页面产物是否齐全

---

## 版本号

遵循 [语义化版本](https://semver.org/lang/zh-CN/)。当前处于 `0.x`，次版本号变更即可包含破坏性改动（如 schema 不兼容需删库重建）。

**踩过的坑**：v0.2.2 的诞生原因是 v0.2.1 的 macOS Intel job 因 runner label 下线（`macos-13`）永久排队，产物不齐。**重推同一个 tag 不会让 Release 补齐产物**——直接开新版本号更干净。

---

## 本地验证

发布前建议在本机跑一遍完整流程：

```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
bash scripts/e2e/smoke.sh
```

测试策略与如何写新测试见 [测试指南](testing.md)，需要人眼判断的验证项见 [人工探针清单](manual-probes.md)。
