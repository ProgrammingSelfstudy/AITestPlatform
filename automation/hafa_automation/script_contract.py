"""生成脚本的静态接口约定，供 Rust 校验器读取。"""
from __future__ import annotations

import argparse
import inspect
import json

from .runtime import Ui

UI_API = {
    name: [parameter for parameter in inspect.signature(method).parameters if parameter != "self"]
    for name, method in inspect.getmembers(Ui, inspect.isfunction)
    if not name.startswith("_")
}

ALLOWED_IMPORTS = [
    "pytest", "re", "time",
    "hafa_automation", "hafa_automation.*",
    "generated", "generated.pages", "generated.pages.*",
]


def main() -> None:
    parser = argparse.ArgumentParser(description="输出生成脚本的静态校验契约")
    parser.add_argument("--json", action="store_true", required=True, help="输出 JSON")
    parser.parse_args()
    print(json.dumps({"ui_api": UI_API, "allowed_imports": ALLOWED_IMPORTS}, ensure_ascii=False))


if __name__ == "__main__":
    main()
