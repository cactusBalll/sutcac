---
name: commit
description: 提交当前工作区改动时使用：检查 diff、编写符合仓库风格的提交信息并提交
---
# 提交流程

1. 先运行 `git status` 和 `git diff` 查看当前改动，理解本次提交的内容。
2. 查看最近几条 `git log --oneline`，让提交信息风格与仓库保持一致。
3. 只暂存本次相关的文件（明确列出路径，不要盲目 `git add .`），不要提交密钥或 `.sutcac/config.toml` 等敏感文件。
4. 编写简洁的提交信息，概括改动目的。
5. 提交前运行 `cargo fmt` 和 `cargo test --workspace`（如适用），失败则先修复。
6. 执行 `git commit`，完成后把提交哈希和摘要报告给用户。
