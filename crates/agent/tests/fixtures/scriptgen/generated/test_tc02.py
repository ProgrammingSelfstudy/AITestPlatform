import pytest

from generated.pages.tc02_pages import RoomPage, LocalLibraryPage

# 用例数据
EXISTING_SONG = "tst_normal_01"


@pytest.mark.case("TC-02")
def test_tc02_added_song_not_selectable(ui, account):
    # 前置（已登录、已开语聊房、已上麦开麦、个人音乐库已包含 tst_normal_01）由 conftest setup 保证

    # 步骤：More → Room Tools → Music
    room = RoomPage(ui)
    more = room.open_more()
    music = more.open_music()
    assert music.has_song(EXISTING_SONG), f"Music 列表中没有找到已添加的歌曲 {EXISTING_SONG}，前置条件不满足"

    # 步骤：点击 + Add Music 进入 Local Music Library
    library: LocalLibraryPage = music.open_add_music()
    assert library.is_open(), "点击 Add Music 后没有进入 Local Music Library 页面"

    # 步骤：在列表中定位歌曲（列表顺序每次不同，按歌名滚动查找）
    row = library.locate_song(EXISTING_SONG)
    assert row is not None, f"Local Music Library 中找不到 {EXISTING_SONG}"

    # 预期1：该行右侧显示 Added
    label_before = library.song_row_label(EXISTING_SONG)
    assert label_before is not None, f"读取不到 {EXISTING_SONG} 行的文案"
    assert label_before.endswith(LocalLibraryPage.ADDED_MARK), \
        f"{EXISTING_SONG} 行右侧没有显示 Added，实际文案：{label_before}"

    # 预期2：选择框为绿色勾选且置灰，不可再被选中
    # 颜色（绿色/置灰）无法从控件树读取，只能验证该行为只读（不可点击）
    assert not library.song_row_clickable(EXISTING_SONG), \
        f"{EXISTING_SONG} 行仍可点击，预期已添加的行置灰且不可再被选中"

    labels_before = set(ui.labels())

    # 步骤：尝试点击该行
    library.tap_song(EXISTING_SONG)

    # 预期3：点击该行不改变选中状态，也不弹出 Toast
    label_after = library.song_row_label(EXISTING_SONG)
    assert label_after == label_before, \
        f"点击后 {EXISTING_SONG} 行状态发生变化：点击前「{label_before}」，点击后「{label_after}」"
    new_labels = set(ui.labels()) - labels_before
    # 注：只能证明读取那一刻没有 Toast，一闪而过的提示无法完全排除
    assert not new_labels, f"点击已添加的行后页面出现了新文字（可能是 Toast）：{sorted(new_labels)}"

    # 预期4：底部按钮保持 Add(0) 且置灰不可点击
    assert library.add_button_present(), "点击已添加的行后，底部按钮不再是 Add(0)"
    assert not library.add_button_clickable(), "底部 Add(0) 按钮可点击，预期置灰不可点击"

    # 观察：点击 Add(0) 也不应有任何效果，仍停留在本地库页面
    library.tap_add_button()
    assert library.is_open(), "点击置灰的 Add(0) 后离开了 Local Music Library 页面"
    assert library.add_button_present(), "点击 Add(0) 后底部按钮不再是 Add(0)"
    assert library.song_row_label(EXISTING_SONG) == label_before, \
        f"点击 Add(0) 后 {EXISTING_SONG} 行状态发生变化"
