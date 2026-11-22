"""Hafa 自动化脚本层（Python）：setup / teardown。

规划里「Python 只用于脚本执行和自动化这一层」：后端和执行节点是 Rust，节点通过
`python -m hafa_automation` 调起这里。代码来自技术验证里已在两台真机上验证过的实现，
改动只有三类：配置改读环境变量、账号改从 stdin 收（绝不读文件 / 不进命令行 / 不打印）、
输出改成机器可读的 JSON 行。
"""
