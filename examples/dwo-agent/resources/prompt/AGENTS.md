## Python 运行规范

- 所有 Python 脚本/模块必须通过 `uv run` 执行，禁止直接调用 `python` / `python3`。
- 安装依赖用 `uv pip install` 或 `uv add`，不用 `pip`。
- SkillHub CLI 调用方式：
  `uv run --directory %USERPROFILE%\.skillhub python skills_store_cli.py <子命令>`
- 需要运行某个 Python 文件时：
  `uv run python <脚本路径>`
