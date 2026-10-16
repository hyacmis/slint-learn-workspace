# Slint UI 学习计划

## 项目概述

这是一个个人学习 Slint 框架的仓库，采用"工作区+独立示例"的结构，通过多个小项目逐步掌握 Slint 的核心概念和高级功能。

## 项目结构

每个学习项目应包含：

```
project-name/
├── Cargo.toml      # 依赖配置
├── build.rs        # Slint 构建脚本
├── src/
│   └── main.rs     # Rust 代码
└── ui/
    └── *.slint     # UI 定义（如 app-window.slint）
```

## 参考资源

- [Slint 官方文档](https://slint.dev/docs.html)
- [Slint 标准组件库](https://slint.dev/latest/docs/slint/reference/overview/)
- [Slint 示例](https://github.com/slint-ui/slint/tree/master/examples)
- [SlintPad 在线编辑器](https://slintpad.com)
