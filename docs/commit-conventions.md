# 提交规范

本规范取代此前“提交只能有单行标题、禁止正文和 trailer”的要求。标题仍不使用 scope。

## 提交边界

- 一次提交表达一个可审阅的意图；包含实现、必要回归、文档、Skill 与对应版本元数据。
- 每轮 RSI 完成并通过现有检查后提交。纯整理、依赖更新若与功能无关，拆成独立提交。
- 版本号按仓库发布政策选择，不为每个中间修补重复递增。不提交密钥、生成产物和系统临时文件。
- commit 不等于授权 push、tag 或 publish；只在用户要求的范围内执行这些操作。

## 标题与正文

标题格式为 `<type>: <具体结果>`，建议不超过 72 个字符，不使用 scope。
类型使用 `feat`、`fix`、`refactor`、`perf`、`docs`、`test`、`build`、`ci`、`chore`、`revert`。
写用户或调用方可观察的结果，避免“update code”“misc fixes”或只有版本号。

非平凡提交在空行后包含以下正文；简单拼写修正可省略正文：

- **动机**：原先的具体问题、触发条件或新增能力的用途。
- **变更**：最终行为和必要的兼容性/迁移说明，不复述实现过程。
- **验证**：实际执行的测试/检查及结果；区分未执行、跳过、环境失败，不能把版本检查替代行为回归。

不兼容变更除相应版本递增外，在正文说明迁移方式，并使用 `BREAKING CHANGE:` trailer。
提交信息用中文说明；命令、API 名称和 trailer 保留标准拼写。可以使用仓库 `.gitmessage` 模板。

## 身份与 AI 署名

- 本项目后续提交的人类作者：`dnvyrn <dnvyrn@gamil.com>`。邮箱按维护者指定拼写保留，不自动改成 gmail.com。
- 不改写已有提交，也不通过 mailmap 将上游作者或其他真实贡献者冒认到维护者名下。
- AI 作为协作者，不能替换人类 Author。仅在实际参与实现、测试或审阅的提交末尾添加相应 trailer。
- 本项目约定：`Co-authored-by: Codex <noreply@openai.com>` 和 `Co-authored-by: Claude <noreply@anthropic.com>`。
- 两者都实际参与才同时添加；不能为补贡献者列表给所有提交自动添加两个名字，也不为未运行的审阅署名。
- Contributors 文档/包元数据登记项目维护者与 AI 协作工具；具体提交的参与证据以 Git 作者和 trailer 为准。

示例：

```text
feat: 为 doctor 增加包与 Skill 版本诊断

动机：连接诊断无法识别 CLI 与当前 Agent Skill 的版本漂移。

变更：追加只读维护检查，保持原有报告样式和连接退出码。

验证：填写本次实际通过的行为测试、安装验证及未验证项。

Co-authored-by: Codex <noreply@openai.com>
```
