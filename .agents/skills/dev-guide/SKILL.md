---
name: dev-guide
description: 当前仓库开发指南。修改或排查本仓库的 Rust 网关、Vue 管理端、账号与出站代理、接口协议、部署、CI、发布或文档时使用，定位权威规范和验证入口。Use for development in the repository containing this skill; not for generic Rust/Vue questions in unrelated repositories.
---

# 开发指南

先读根目录的 [AGENTS.md](../../../AGENTS.md) 和 [CONTRIBUTING.md](../../../CONTRIBUTING.md)。
本技能只组织工作入口，架构、协议和视觉规则从下列权威文档读取，不维护第二份副本。

## 按任务读取

| 任务 | 技能与文档入口 |
| --- | --- |
| 用户能力、快速开始 | [README](../../../README.md) |
| 模块职责、调用链、状态归属 | [系统架构](../../../docs/architecture.md) |
| Rust 开发 | 加载并使用 `$rust-best-practices`，结合 [系统架构](../../../docs/architecture.md) 与 [项目约定](../../../CONTRIBUTING.md#项目约定) |
| HTTP 路由、请求与响应、导入导出 | [API 文档](../../../docs/api.md) |
| 前端开发、管理端组件、主题 Token、视觉验证 | 按下方「前端任务」加载设计约束，结合 [管理端主题](../../../docs/theme.md) 与 [界面验证](../../../CONTRIBUTING.md#界面验证) |
| 常规文档更新 | [文档职责与更新条件](../../../CONTRIBUTING.md#文档职责与更新条件)：维护当前说明，在原章节就地修订，不记录变更历史 |
| 部署、客户端配置、备份与恢复 | [部署说明](../../../deploy/README.md) |
| 数据迁移、冻结清单与本地测试库 | [迁移说明](../../../backend/migrations/README.md) |
| 验证命令与提交规范 | [贡献与审查](../../../CONTRIBUTING.md) |
| 创建或审查 PR | [github-pr](../github-pr/SKILL.md) |
| 整理或提交 Issue | [github-issue](../github-issue/SKILL.md) |
| 发布说明、版本规划与发版 | [release](../release/SKILL.md) |

## 前端任务

- 加载并使用 `$frontend-design`，先读 [界面文案与信息层级](../../../docs/theme.md#界面文案与信息层级)，将本项目的管理工具风格、现有字体与主题 Token 作为设计约束
- 开发前按该章节的参考入口和相邻页面核对布局、文案密度与操作位置，再选择共用组件
- 交付前按该章节检查界面文案中的句号、分号，以及过大的标题、重复说明和说明块占用的空间，结合 [界面验证](../../../CONTRIBUTING.md#界面验证) 核对实际页面

## 执行顺序

1. 从仓库根目录确认工作区、当前任务范围和相关运行实例；保留已有改动。
2. 按 `AGENTS.md` 的 CodeGraph 约定定位入口、调用链和职责归属，再阅读相关实现。
3. 按 [问题与方案依据](../../../CONTRIBUTING.md#问题与方案依据) 核实触发条件、问题发生版本与目标分支现状，
   优先依据适用的官方源码确定方案。结合上表读取所需文档，先找可复用的同类实现，再修改所属模块。
4. 文档和运行结果不一致时，追查当前源码、配置与版本，区分实现缺陷和文档过期；
   按 [文档职责与更新条件](../../../CONTRIBUTING.md#文档职责与更新条件) 判断是否修订，不为每次改动追加文档或发布说明。
5. 交付前按文档职责逐段复核最终文档 diff：新增内容是否回答当前读者的使用或维护问题，是否属于该文档，
   是否与其他位置重复，脱离本次变更背景能否独立理解。仅解释实现经过、前后对比或验证结果的内容移到
   任务报告或 PR；仍影响升级、恢复的说明保留适用条件与操作。没有文档差异时，只核对行为变化是否使现有说明失真。
6. 按 `CONTRIBUTING.md` 验证并报告结果。后端 Cargo 命令从 `backend/` 执行，或显式指定该目录的 manifest。

发布任务按 `release` 技能读取当前版本、发布入口和工作流，发布说明语言遵循 `CONTRIBUTING.md`。
