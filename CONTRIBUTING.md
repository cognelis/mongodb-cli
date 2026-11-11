# Contributing and releasing

## Version policy

`Cargo.toml`'s `workspace.package.version` is the canonical product version.
`mongodb-cli` and `mongodb-client` inherit it. Their entries in `Cargo.lock` and both
plugin manifests and `build/npm/package.json` must carry the same release version.

Use plain `MAJOR.MINOR.PATCH` versions for releases:

| Change | Version increment |
| --- | --- |
| Compatible bug fix, security fix, or released Skill correction | Patch |
| New user-visible capability | Minor |
| Incompatible CLI, config, JSON, or public Rust API change while below 1.0 | Minor, with migration notes |
| Incompatible public behavior at 1.0 or later | Major, with migration notes |
| Internal refactor, tests, or documentation with no shipped behavior change | Record under Unreleased when useful; no standalone release is required |

Choose the highest required increment in a batch. Bump once for the completed
batch, not for each intermediate edit or test run. `+codex.<timestamp>` is a local
development cache key, not a substitute for a product version increment; never
use it in a release commit. A changed plain plugin version also selects a fresh
plugin cache during installation.

The JSON envelope's `schemaVersion` is a separate compatibility contract. Do not
change it just because the product version changes. Describe breaking behavior
and required caller changes explicitly in the changelog.

The Claude marketplace's `metadata.version` versions the marketplace descriptor,
not the plugin. Update it only when releasing a descriptor/schema change; plugin
versions belong in the plugin manifests.

## RSI：有界的工具自改进

每轮由编码 Agent 使用 CLI、发现失败、修改工具，再将验证结果作为下一轮的输入。
当前采用人工发起的单轮流程，复用现有测试，不运行后台自修改或自动发布任务。

1. 从脱敏的失败记录或可复现的边界场景提出一个假设，记录版本、输入和预期结果；数据库测试只用临时实例。
2. 在现有测试中加入用例，先确认原实现失败。预期结果来自 CLI 契约，不能为迁就修复而降低断言。
3. 检查共用函数的调用方及同一契约的其他入口（例如 URI 显示、驱动错误、输入校验），只修改根因；候选修复还需通过已有用例，不能只通过新增用例。
4. 执行下方发布前检查，区分真正执行的测试与因缺少服务跳过的测试，记录环境限制。
5. 保留通过验证的修复及回归用例，按版本规则收尾。失败时只撤销本轮候选改动；提交和发布仍遵守授权范围。

首轮（2026-11-11）：驱动错误中嵌入的 URI 也必须删除秘密查询参数。
`uri::tests::scrub_removes_secret_options_from_embedded_uris` 和已有服务器错误用例的新增断言，
在修复前失败、修复后通过；错误脱敏复用显示 URI 的敏感参数规则，保留错误分类与诊断信息。
补丁复查又发现相邻 URI 可能漏脱敏：先补失败断言，再修正扫描边界，使两个 URI 都能被处理。
这些用例验证工具契约，不代表已经测得 Agent 端到端任务成功率的提升。
验证记录：0.4.1 通过工作区测试（含临时 mongod）、fmt、clippy、9 项 Node 测试、13 项 Python 测试、
Agent 集成及版本检查。Docker 与真实知识库两个可选测试未启用；Atlas 使用本地模拟服务。

第二轮（2026-11-11）：从完整 URI 的脱敏扩展检查到驱动输入校验错误。
驱动会单独回显解码后的错误参数片段，不能靠查找 URI 脱敏；固定校验提示在边界阻断回显。
新增单元测试及真实 CLI 测试覆盖环境变量、stdin、百分号编码，并确认失败不修改配置、不保存凭据。
定向更新间接依赖 chacha20 至 0.10.2，清除上一轮安装时发现的撤回版本警告。
验证记录：0.4.2 通过工作区测试（含临时 mongod）、fmt、clippy、9 项 Node 测试、13 项 Python 测试、
Agent 集成及版本检查；Docker 与真实知识库仍未启用，Atlas 仍使用本地模拟服务。

第三轮（2026-11-11）：本地 Atlas stub 返回 expires_in=u64::MAX，真实 CLI 原本 panic，
退出 101；改用 checked_add 后返回 incompatible_response（退出 7），不缓存 token、不继续业务请求。
既有 token 复用和撤销测试继续覆盖正常值；极端输入验证信任边界，不代表真实 Atlas 会返回该值。
同时补齐系统元数据的 gitignore。按用户要求，每轮验证完成后提交代码，推送和发布仍需单独授权。
验证记录：0.4.3 通过工作区测试（含临时 mongod）、fmt、clippy、9 项 Node 测试、13 项 Python 测试、
Agent 集成及版本检查；Docker 与真实知识库仍未启用，Atlas 使用本地模拟服务。
本机 CLI 与独立 Skills 已刷新并核验。npm 冒烟首次因使用错误的测试变量名而选中旧产物；
按脚本声明设置 MONGODB_CLI_TEST_BINARY 为本轮安装的二进制后通过，不将夹具版本错误计作产品缺陷。

第四轮（2026-11-11）：导出原先用 File::create 打开固定的目标.tmp-PID，失败后还会删除该路径。
预放同名无关文件的回归用例在原实现失败，证明会覆盖并删除不属于本次导出的文件。
复用已有 tempfile::NamedTempFile 独占创建并原子替换，失败自动清理。真实临时 mongod 测试覆盖
成功覆盖旧导出、相对路径、Unix 私有权限和失败后的目录完整性；不引入新依赖。
0.4.4 验收：工作区测试（含临时 mongod）、fmt、clippy、9 项 Node 测试、13 项 Python 测试、
Agent 集成及版本检查通过。Docker 与真实知识库未启用，未验证 Windows 文件语义。
本机 CLI 与独立 Skills 已刷新并核验，指定 0.4.4 二进制的 npm tarball 冒烟检查通过。

方法参考：[Darwin Gödel Machine](https://arxiv.org/abs/2505.22954) 的候选修改与实测验证，
以及 [Agent evals](https://www.anthropic.com/research/demystifying-evals-for-ai-agents) 的失败样本积累与回归检查。

## Record changes as you work

Add concise, user-facing entries under `CHANGELOG.md`'s `Unreleased` section using
`Added`, `Changed`, `Fixed`, `Removed`, or `Security` as appropriate. Include
migration notes when commands, authentication, defaults, or public APIs change.
Do not rewrite a previously published release's history to hide a later change.

Keep CLI behavior, README examples, and the bundled Skills under
`plugins/mongodb-cli/skills/` in sync; the canonical Skill is
`plugins/mongodb-cli/skills/mongodb-cli/SKILL.md`. The Skills are embedded in the
executable, so rebuild before updating installed copies.

## Prepare a release

After the batch and its changelog entries are ready, choose the increment:

```sh
python3 scripts/release.py prepare minor
# Or: prepare patch / prepare major
python3 scripts/release.py check
```

`prepare` moves Unreleased notes into a dated release entry, updates the workspace,
lockfile, both plugin manifests, and the npm package, and leaves a fresh Unreleased section. It
uses the local calendar date; `--date YYYY-MM-DD` allows an explicit release date.
It rejects an empty Unreleased section, mismatched versions, and duplicate or
out-of-order release entries before changing files. It does not commit, tag,
push, publish, or install anything.

Use Python 3.9 or newer for the repository scripts. Run the checks before finalizing:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
node --test build/npm/install.test.js
python3 -m unittest discover -s tests -p 'test_*.py'
python3 scripts/validate-agent-integration.py
python3 scripts/release.py check
git diff --check
```

`cargo test` includes an end-to-end suite against a real server. It uses the disposable
server in `MONGODB_CLI_TEST_URI` when set, otherwise starts a temporary `mongod` from
`MONGODB_CLI_TEST_MONGOD` or `PATH`, and skips itself when neither exists. CI provides a
MongoDB service container. Never point `MONGODB_CLI_TEST_URI` at shared data: the suite
creates and drops its own database.

For npm distribution, also build the native executable and run
`npm --prefix build/npm run test:package`. This checks the tarball's file allowlist,
installs it into an isolated directory, and exercises the launcher with a verified
native-binary fixture. Public download verification remains required before publishing.
See [docs/releasing.md](docs/releasing.md) for the first npm publish and CI setup.

Review the version bump and migration notes together with the implementation.
When a release is requested, commit the reviewed changes and create an annotated
`vX.Y.Z` tag on that commit, using the matching CHANGELOG section as release notes.
A request to commit changes alone does not require creating a release tag. Do not
tag an older commit while the release changes are still uncommitted. Push tags or
publish packages only when those actions are requested.

## Refresh local installations

After verification, update the executable and its embedded Skills:

```sh
cargo install --path crates/mongodb-cli --locked
mongodb-cli version --output json
mongodb-cli skills install
```

For an already configured local Codex marketplace, refresh the plugin using its
new release version:

```sh
codex plugin add mongodb-cli@mongodb-cli
```

Verify that the installed Skills and active plugin cache match the canonical
source. New Codex threads pick up the refreshed plugin instructions. Claude Code
plugin-managed installations use their own plugin update flow; standalone Skills
are updated by `mongodb-cli skills install`.

## 第五轮：doctor 包与 Skill 版本诊断

参考 `/Users/caspian/git/jenkins-cli/internal/app/doctor.go` 的诊断规则：更新查询最多 5 秒，
可显式跳过；加载版本与安装版本分开判断，升级信息不参与连接健康或退出码计算。
新检查追加到原报告末尾，保留原有列与输出 envelope；版本信息来自公开发布元数据和本地 Skill 文件，
不复用业务认证。Skill frontmatter 版本随产品版本更新，并由发布检查防止漂移。

## 提交与贡献者

提交正文、验证记录和 AI 署名遵循 [提交规范](docs/commit-conventions.md)。项目维护者及 AI 协作工具见 [CONTRIBUTORS.md](CONTRIBUTORS.md)。

## 第五轮验收

0.5.0：工作区测试（含临时 mongod）、9 项 Node 安装测试、14 项 Python 测试、fmt/clippy、Skill 集成和发布检查通过；Docker 与真实知识库可选测试未启用。
包版本查询使用模拟发布响应覆盖更新、离线/错误、超时与响应大小边界；Skill 覆盖缺失、旧版、未知、当前、需重载、项目安装及不可读状态。
贡献者和提交规范一同进入本版本，后续 Author 为 `dnvyrn <dnvyrn@gamil.com>`，AI trailer 只记录实际参与者。

本轮本地安装和 Skill 内容已核验；Rust 两个包的 npm tarball 安装冒烟通过。三个 doctor 的实际表格输出保持原样，维护检查只追加行。

## 第六轮 RSI：请求与结果的相邻边界

原索引检查只沿 inputStage 链前进，完全走索引的 OR/inputStages 计划仍被拒绝。改为遍历 winning plan 的所有子分支，拒绝任一 COLLSCAN、未知或空分支，不检查 rejectedPlans。用临时 mongod 证明两个已索引分支可以查询，混合未索引条件仍被拒绝。计划结构依据 [MongoDB Explain 文档](https://www.mongodb.com/docs/manual/reference/explain-results/)。

第六轮验收：0.5.1：工作区测试（含临时 mongod 的全索引 OR 与未索引分支拒绝）、9 项 Node 安装测试、14 项 Python 测试、fmt、clippy、Agent 集成及发布检查通过。 未实测生产服务及其他操作系统。

本轮本地 CLI/Skills 已刷新，124 个安装文件与源码核对一致；两个 Rust 包的安装冒烟通过。按新提交规范记录动机、变更、验证与实际 Codex 署名，不推送或发布。
