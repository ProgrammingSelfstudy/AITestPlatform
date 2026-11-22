import re

import pytest

from generated.pages.tc06c_pages import RoomPage, MusicPage, LocalLibraryPage

# 用例数据
BIG_FILE_NAME = "tst_big_55mb"      # 文件名，界面上可能被遮成 ***
BIG_FILE_MASKED = "***"
BIG_FILE_DURATION = "05:30"
# 列表里 05:30 只有这一条，按时长定位最稳（文件名可能被遮挡）
BIG_FILE_KEY = f" / {BIG_FILE_DURATION}"
BIG_FILE_ROW_PATTERN = rf"^.* / {re.escape(BIG_FILE_DURATION)}$"


@pytest.mark.case("TC-06c")
def test_tc06c_oversize_file_blocked(ui, account):
    # 前置：已登录、已开语聊房、已上麦并开启麦克风、音乐库未满、测试文件已推送，均由 conftest setup 保证

    # 步骤 2：More → Room Tools → Music → + Add Music
    room = RoomPage(ui)
    room.open_more()
    music = room.open_music()
    library = music.open_add_music()

    # 步骤 3：在 Local Music Library 中查找 tst_big_55mb.wav（可能显示为 ***，时长 05:30）
    row = library.scroll_to_song(BIG_FILE_KEY, exact=False)
    assert row is not None, f"预期1未满足：Local Music Library 中找不到时长 {BIG_FILE_DURATION} 的超大文件"

    # 预期 1：该文件正常展示，时长 05:30
    row_label = library.read_row(BIG_FILE_ROW_PATTERN)
    assert row_label is not None, f"预期1未满足：列表中没有时长为 {BIG_FILE_DURATION} 的条目"
    name = row_label.rsplit(" / ", 1)[0].strip()
    assert name == BIG_FILE_MASKED or BIG_FILE_NAME in name, (
        f"预期1未满足：时长 {BIG_FILE_DURATION} 的条目名称异常：{row_label}"
    )

    # 点击前：底部按钮为 Add(0)，没有 Toast
    assert library.add_zero_button() is not None, "前置检查失败：点击前底部按钮不是 Add(0)"
    assert not ui.has(LocalLibraryPage.SIZE_LIMIT_TOAST), "前置检查失败：点击前已经出现超大小提示"

    # 步骤 3/4：点击尝试选中，并记录 Toast 文案
    library.tap_song(BIG_FILE_KEY, exact=False)

    # 预期 2：点击后提示 "Music size cannot exceed 50MB"
    assert library.wait_size_toast(), (
        f"预期2未满足：点击超大文件后没有出现提示「{LocalLibraryPage.SIZE_LIMIT_TOAST}」"
    )

    # 等 Toast 消失后再检查选中状态
    library.wait_toast_gone()

    # 预期 2：保持未选中。选择框没有 content-desc，无法直接读取勾选状态，
    # 所以用底部计数仍为 Add(0) 来判断没有任何条目被选中
    assert library.add_zero_button() is not None, "预期2未满足：点击后底部计数不再是 Add(0)，超大文件可能被选中了"

    # 预期 3：底部按钮保持 Add(0) 置灰（不可点击）
    assert library.add_zero_button_clickable() is None, "预期3未满足：底部 Add(0) 按钮变成了可点击状态，没有保持置灰"
    # 置灰的视觉颜色无法从控件树读取，这里用「不可点击」代替判断
