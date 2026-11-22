import pytest

from generated.pages.tc04_pages import RoomPage

# 失败原因：Local Music Library 的顺序每次运行都不同（轨迹里 filler 的顺序就是乱的），
# 目标歌曲并不一定排在最后。上一版只在「滑到底后的最后一屏」里找目标，
# 目标出现在中间某屏时会被滑过去，于是断言失败；「目标必须是最后一行」的断言也同理不成立。
# 修正：滑动过程中累计所有见过的歌曲，记录目标是否出现过；滑到底后若目标已不在屏幕内，
# 再向下滑回到目标处后选中。列表末尾的判定改为「内容不再变化」并结合标题里的总数校验。

TARGET = "tst_swipe_zz"
MAX_SWIPES = 15


@pytest.mark.case("TC-04")
def test_tc04_swipe_find_and_add_song(ui, account):
    # 步骤 1「进入语聊房，上麦并开启麦克风」以及「个人音乐库为空」由 conftest 的 setup 保证
    room = RoomPage(ui)

    # 步骤 2：More → Music → Add Music → Local Music Library
    more = room.open_more()
    music = more.open_music()
    assert music.is_empty(), "前置条件不满足：添加前 Music 列表应为空（No music）"
    library = music.open_add_music()
    assert library.is_open(), "未进入 Local Music Library 页面"
    total = library.total_count()

    # 步骤 3：记录首屏歌曲
    first_screen = library.visible_songs()
    assert first_screen, "Local Music Library 首屏没有读到任何歌曲"
    assert library.add_button_count() == 0, "初始状态底部按钮应为 Add(0)"

    seen = set(first_screen)
    target_seen = any(song.startswith(TARGET + " / ") for song in first_screen)

    # 步骤 4：上滑列表，记录目标是否出现，并继续滑到列表末尾（内容不再变化）
    previous = first_screen
    reached_end = False
    unchanged = 0
    for _ in range(MAX_SWIPES):
        library.swipe_up()
        assert library.is_open(), "预期「滑动过程中不误触进入其他页面」未满足：滑动后离开了 Local Music Library"
        assert library.add_button_count() == 0, "预期「滑动过程中不误触」未满足：滑动时误选了歌曲（按钮不再是 Add(0)）"
        current = library.visible_songs()
        if any(song.startswith(TARGET + " / ") for song in current):
            target_seen = True
        if current == previous:
            unchanged += 1
            # 已见歌曲数达到标题总数，或连续两次滑动内容都不变，视为到底
            if (total is not None and len(seen) >= total) or unchanged >= 2:
                reached_end = True
                break
            continue
        unchanged = 0
        assert set(current) - set(previous), "预期「滑动时列表内容持续上移」未满足：滑动后没有出现新的歌曲"
        seen.update(current)
        previous = current

    assert reached_end, f"预期「能滑到列表末尾」未满足：滑动 {MAX_SWIPES} 次后列表仍在变化"
    assert first_screen[0] not in previous, "预期「滑动时列表内容持续上移」未满足：滑到底后首屏第一首仍在屏幕上"
    if total is not None:
        assert len(seen) >= total, \
            f"预期「能滑到列表末尾」未满足：标题显示共 {total} 首，滑动过程只看到 {len(seen)} 首"
    assert target_seen, f"滑动整个列表过程中没有看到目标歌曲 {TARGET}"
    # 列表顺序每次不同，不断言目标一定在最后一行

    # 目标可能已被滑过去，向下滑回到目标处
    assert library.scroll_back_to(TARGET), f"滑回列表后屏幕内找不到目标歌曲 {TARGET}"

    # 步骤 5：选中目标歌曲
    library.select_song(TARGET)
    # 选择框变为勾选是图形状态，content-desc 里读不到；以按钮计数变为 1 作为选中成功的依据
    assert library.add_button_count() == 1, \
        f"预期「找到 {TARGET} 后可以正常选中」及「底部按钮变为 Add(1)」未满足：按钮不是 Add(1)"
    assert library.is_open(), "选中歌曲后意外离开了 Local Music Library 页面"

    # 步骤 6：点击 Add(1)
    music = library.confirm_add(1)
    # 能进入 Music 页即说明 Add(1) 可点击；Music added successfully 提示是 toast，可能已消失，不作硬断言
    assert music.title() == "Music(1/50)", "预期「底部按钮 Add(1) 可点击、添加成功」未满足：Music 页标题不是 Music(1/50)"
    assert music.has_song(TARGET), f"预期「添加成功后 {TARGET} 出现在 Music 列表」未满足"
    assert not music.is_empty(), "添加后 Music 列表仍显示 No music"
