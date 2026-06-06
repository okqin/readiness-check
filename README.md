# readiness-check

## Environment Setup

### pre-commit

pre-commit 是一个代码检查工具，可以在提交代码前进行代码检查。

```bash
pipx install pre-commit
```

安装成功后运行 `pre-commit install` 即可。

### Cargo deny

Cargo deny 是一个 Cargo 插件，可以用于检查依赖的安全性。

```bash
cargo install --locked cargo-deny
```

### typos

typos 是一个拼写检查工具。

```bash
cargo install typos-cli
```

### git cliff

git cliff 是一个生成 changelog 的工具。

```bash
cargo install git-cliff
```

### cargo nextest

cargo nextest 是一个 Rust 增强测试工具。

```bash
cargo install cargo-nextest --locked
```
