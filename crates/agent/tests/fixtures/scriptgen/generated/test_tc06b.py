import pytest

from generated.pages.tc06b_pages import RoomPage

# Test data
LONG_SONG = "tst_long_11min"
EXPECTED_DURATION = "11:00"


@pytest.mark.case("TC-06b")
def test_tc06b_long_file_selection_blocked(ui, account):
    # Precondition: conftest setup has already logged in, entered the voice chat room,
    # joined a mic seat and turned the mic on (step 1 of the case).
    # The script starts from More -> Room Tools -> Music.

    # Step 2: More -> Room Tools -> Music -> + Add Music
    library = RoomPage(ui).open_more().open_music().open_add_music()

    # Step 3: find tst_long_11min.mp3 in Local Music Library
    row = library.find_song(LONG_SONG)
    assert row is not None, f"本地音乐库中找不到 {LONG_SONG}，测试数据可能未推送或未完成媒体扫描"

    # Expected 1: tst_long_11min.mp3 is displayed normally as 11:00
    duration = library.read_song_duration(LONG_SONG)
    assert duration == EXPECTED_DURATION, (
        f"预期1未满足：{LONG_SONG} 时长应展示为 {EXPECTED_DURATION}，实际为 {duration}"
    )

    add_before = library.read_add_button()
    assert add_before == "Add(0)", f"点击前底部按钮应为 Add(0)，实际为 {add_before}"

    # Step 3 (continued): tap to try selecting it
    library.tap_song(LONG_SONG)

    # Step 4 / Expected 2: record the Toast text. The Toast should say
    # "Music duration cannot exceed 10 minutes".
    assert library.wait_duration_limit_toast(), (
        f"预期2未满足：点击 {LONG_SONG} 后未出现提示 "
        f"\"{library.DURATION_LIMIT_TOAST}\"，当前页面：{ui.labels()}"
    )

    # Expected 2 (continued): the row stays unselected.
    # The selection circle is a graphic with no content-desc, so it cannot be read from the UI tree.
    # Indirect check: the selected count on the bottom button is still 0 (asserted below).

    # Expected 3: the bottom button stays at Add(0) and greyed out.
    add_after = library.read_add_button()
    assert add_after == "Add(0)", (
        f"预期3未满足：点击后底部按钮应保持 Add(0)，实际为 {add_after}（说明该行被选中了）"
    )
    # The grey (disabled) colour cannot be read from content-desc, so it is not asserted.
    # Add(0) staying unchanged is used as the check that the button is not usable.

    # The list should still show the song and its duration correctly
    assert library.read_song_duration(LONG_SONG) == EXPECTED_DURATION, (
        f"点击后 {LONG_SONG} 的时长展示异常，应仍为 {EXPECTED_DURATION}"
    )
